# The HTTP API

The daemon serves ten routes on loopback. `grove-api` is the shared contract crate: the
daemon serves these types and the CLI client decodes them, so neither end can drift from
the other's spelling. `contracts/wire-vocab.json` is the vocabulary SSOT — every status
string, error code, event name and payload field name, derived from the producing types.

## The envelope

Every enveloped body on this surface — that is, every route but `GET /api/events` —
guard refusals and framework fallbacks included:

```jsonc
{"ok": true,  "data": <T>}
{"ok": false, "error": {"code": <ErrorCode>, "message": <string>, "data": <any>?}}
```

`grove_api::Envelope` is the only thing that writes it, and `grove_daemon::reply` is the
only adapter from it to an axum response — including on a serialization failure, which
answers a 500 envelope rather than axum's bare-text error path. `GET /api/events` is the
one route outside that adapter: its frames are the tagged `Event` objects described under
§ Events, streamed as SSE with no `{"ok": …}` wrapper around them.

Three properties:

- **Tagged on `ok`.** The decoder reads the boolean, not which of `data`/`error` happens
  to be present. `{"ok":true,"error":{…}}` and `{"ok":false,"data":…}` are decode errors,
  and a body with no `ok` is not an envelope at all.
- **`error.data` is carried.** It is route-specific detail — a slug, a reason, a boot
  status — kept as raw JSON so the envelope stays one type, and read back typed with
  `ApiError::data_as`. A payload of an unexpected shape reads as absent rather than
  failing the decode: an optional diagnostic must never turn a reported failure into a
  decode failure.
- **The status code travels beside the envelope**, not inside it. HTTP already carries it.

`ApiError` displays as `code: message`, and that string is what the CLI prints, so an
operator can match what they see against the table below — **followed by
`ApiError::detail()`, the human rendering of `error.data`**, when the route sent any.
That is not decoration: `doctor_failed` and `remove_failed` carry deliberately generic
messages whose whole content is `data.reason`, so a client that drops it prints
`doctor_failed: doctor failed` at the moment the daemon knows the manifest's parse error
down to the line and column.

## Routes

Slugs contain `/`, so every route that names one is a body-carrying POST rather than a
path parameter.

| method + path | request | 200 `data` |
|---|---|---|
| `GET /api/health` | — | `{status: "ready", version, home}` |
| `GET /api/daemon/version` | — | `{version, uptime_ms}` |
| `POST /api/daemon/shutdown` | — | `{stopping: true}` |
| `POST /api/roots/reconcile` | — | `{reconcile: "scheduled"}` |
| `POST /api/roots/sync` | `{slug}` | `{sync: "accepted"}` |
| `POST /api/roots/remove` | `{slug, force?}` | `{removed: <slug>}` |
| `POST /api/worktrees/remove` | `{slug, name}` | `{removed: <name>}` |
| `POST /api/doctor` | `{slug?, dry_run?, fix?}` | the doctor payload (below) |
| `GET /api/roots` | — | the snapshot (below) |
| `GET /api/events` | — | an SSE stream of events |

`GET /api/health` carries the **home** this daemon realizes, not only its version. That
field is identity, and the CLI reads it as such: `GROVE_HOME` and `GROVE_BIND` are
resolved independently, so a client scoped to one home routinely finds a daemon serving
another on the default bind. A daemon whose home differs from the caller's is treated as
absent — this home has no realizer, so the CLI realizes in-process — rather than being
handed a `roots/remove` it would execute against its own home.

### Failures

| status | `error.code` | when | `error.data` |
|---|---|---|---|
| 503 | `degraded` | `GET /api/health` while boot state is degraded | `{status: "degraded", reason}` |
| 503 | `unavailable` | health while booting or stopping; any non-whitelisted route behind the readiness gate; a root's lane at its queue bound; a sync for a declared root no engine is driving | `{status}` for health; `{slug}` for a saturated lane and for a sync with no engine |
| 403 | `forbidden` | the mutation guard refused a cross-origin state-changing request | — |
| 404 | `not_found` | the slug or worktree named is not declared | `{slug}` / `{slug, name}` |
| 422 | `invalid_request` | a missing or malformed body parameter | — |
| 422 | `sync_failed` | a sync request the daemon could not decide on — the manifest would not read | `{reason}` |
| 422 | `remove_failed` | a remove reached the ops layer and failed there — including a guarded root remove refusing over unsaved work | `{reason}` |
| 422 | `doctor_failed` | a doctor run failed | `{reason}` |
| 404/405/500 | `error` | no such route, wrong method, or an ops task that panicked | — |

`ErrorCode` decoding is **strict**: an unrecognized code is a decode failure, not a silent
fallback onto `error`. The vocabulary is closed — one binary serves and consumes it, and
the self-update gate refuses a version mismatch on the port — so an unknown code means
genuine drift.

The HTTP taxonomy is unrelated to `grove_ops::Error::code`, which classifies an
*operation*'s failure and drives the engine's terminal/transient split. Both are contract,
and neither is mapped onto the other: `remove_failed` carries the ops message in
`data.reason` and leaves the ops category out of the HTTP code.

## Guards

Two middleware layers wrap every `/api` path, readiness applied outermost so it runs first
— a draining daemon answers 503 rather than spending a cross-origin verdict on a request it
would not serve either way.

**Readiness** 503s every non-whitelisted `/api` path while the boot state is `stopping` or
`degraded`. The whitelist is exactly two paths:

```
/api/health              # so the state stays observable — the self-update gate reads it
/api/daemon/shutdown     # so a late, repeated, or degraded-daemon stop is not refused
```

`booting` is deliberately not gated; in practice that window is microseconds wide, between
`Daemon::bind` and the caller's `mark_ready`.

**Mutation** passes GET/HEAD/OPTIONS untouched. For every other method it allows:

- a **missing** `Origin` — the CLI and every other non-browser client sends none, so
  dropping this allowance would break every mutation grove itself performs; or
- an `Origin` whose host is one of the fixed set `localhost`, `127.0.0.1`, `::1`.

Anything else is 403 `forbidden`. The list is **fixed**, never the request's own `Host`
header — that is what makes it a DNS-rebinding defense: a page served from `evil.example`
still sends `Origin: https://evil.example` after its name has been rebound to 127.0.0.1,
so the origin, which a browser will not let the attacker forge, fails the list where a
`Host` check would pass. An unparseable origin — `null` from a sandboxed frame, a bare
host, an authority carrying userinfo — has no host and is refused: the parse fails closed.

Residual and accepted until auth lands: another app already on a loopback port can present
a loopback origin. Closing that needs request authentication, which is the work this guard
stands in for.

Both guards, and the loopback bind refusal, are compensating controls for an
unauthenticated API. See `docs/deployment.md`.

## The snapshot

`GET /api/roots` answers `Snapshot`, and it is the same type `GET /api/events` opens with —
one shape on purpose, because a UI that renders from the stream and a UI that polls must be
looking at the same thing.

```jsonc
{
  "roots": [{
    "slug": "o/r",
    "url": "git@github.com:o/r.git",
    "status": "ready",                       // root_status
    "pool": {"observed": 1, "target": 2},
    "syncing": true,
    "sync_note": "diverged",                 // omitted when there is none
    "trunk": "/home/code/o/r/canary",        // absolute path — what a UI opens
    "trunk_branch": "canary",                // the branch it checks out; "" from an older daemon
    "trunk_status": {...},                   // the trunk's own git drift; omitted when absent
    "worktrees": [{
      "name": "feat",
      "branch": "feature/x",                 // what the manifest DECLARES
      "base": "main",                        // omitted when unknown
      "declared": true,
      "present": true,
      "path": "/home/code/o/r/feat",
      "status": {...}                        // what is actually checked out; omitted when absent
    }]
  }],
  "logs": [{"at_ms": 0, "level": "info", "target": "…", "message": "…", "fields": [...]}]
}
```

Absent optionals are **omitted, never rendered as `null`**, and that is pinned byte for
byte by `a_snapshot_serializes_the_shape_a_dashboard_renders`.

`trunk_branch` travels beside `trunk` rather than being read back out of it: the checkout
is named by folding the branch's `/` to `-`, so `feature-x` cannot be un-folded into
`feature/x` by a consumer. It is also what tells the trunk apart from the ordinary
worktrees beside it, none of which carry anything in their name to say which one is
grove's. It is the one required field defaulted on the way in — a body from a daemon that
predates it is a single absent key, and refusing the whole decode for that would cost
`grove tree list` its status header and the row with it.

`branch` and `status.branch` both travel because their disagreement is the drift a
dashboard flags and doctor reports. `logs` is the daemon's bounded log ring (500 lines,
oldest first), so a viewer that attached after the daemon started still sees what it
missed.

A row whose `status` is `unavailable` is a row whose git reads did not land inside their
budget: it is structurally present but its git-derived fields are empty. `ready` beside an
empty worktree list would be a lie a client cannot detect, so the status says so instead.
Consumers must treat such a row as *no answer* — the CLI's `tree list` falls through to the
disk on it.

Assembly: per root, two in-memory engine reads plus **one** lane job carrying every git read
the row needs, on the lane's **read** tier under a 3 s budget. Roots are read concurrently
and the rows come back in declared order regardless.

## Events

`GET /api/events` is Server-Sent Events. Each frame's `event:` line is the event name and
its `data:` line is the JSON object, which carries the same name in its own `event` field —
so `addEventListener("task_finished", …)` and `onmessage` both work without either knowing
about the other. A comment frame (`: beat`) is written after 15 s of silence.

| event | payload | source |
|---|---|---|
| `snapshot` | the whole `Snapshot` | synthesized per connection — always the first frame |
| `resync` | the whole `Snapshot` | synthesized when this subscriber fell behind the bus |
| `roots_changed` | `{roots: [slug]}` — the full declared set, not a delta | the watcher |
| `root_sync_changed` | `{slug}` | an engine, on sync accept and on completion |
| `task_started` | `{slug, kind}` | inside the lane job, when the work actually begins |
| `task_finished` | `{slug, kind, outcome}` | the engine driver |
| `log` | one `LogLine` | the daemon's log ring |

`kind` is `reconcile | sync | fill` — exactly the engine's background slot. Foreground work
(promote, remove, doctor) answers on its own request and produces no frame. `outcome` is
`ok | failed`, deliberately coarse: a consumer re-reads the root's status and sync note
rather than reconstructing them from a token, so a finer vocabulary here would be a second,
drift-prone copy of state that is already published.

**Level-triggered.** The four state events each say *something changed, re-read it* and
carry no state a consumer should accumulate, so a dropped one costs a stale view and never
a divergent one. That is what lets the bus be a bounded broadcast that drops for a slow
subscriber. When it drops, the connection is not left with a gap: it receives a `resync`
carrying a fresh snapshot. `resync` is distinct from `snapshot` so a client can tell "here
is the start" from "you lost your place"; the payload is the same because the recovery is
the same — replace what you hold.

`log` is the exception and is carried on a channel of its own. A log line *is* the
information — a dropped one is gone — so a lag there is reported the only way it can be: a
synthetic `warn` line naming how many this connection will never see. Keeping the two
channels separate is what stops a chatty minute from evicting pending state events.

A log line's shape is fixed: `at_ms` (through the clock seam), `level`, `target`, `message`,
and only the whitelisted `fields` grove's own macros key on, each rendered through a
redactor that strips URL userinfo. A `tracing` event carries arbitrary fields, and a stream
that forwarded all of them would publish whatever a future `info!` happened to attach.

Ordering on connect: the route subscribes to the bus *before* it builds the snapshot. The
reverse would leave a window in which an event lands after the read and before the
subscription and is lost. This way the same event is merely delivered twice — once folded
into the snapshot, once as its own frame — which costs a redundant re-read and nothing else.

A drain ends every open stream: the producer's every await races the shutdown signal, and
`Daemon::serve` bounds the whole drain, because a peer that stopped reading cannot be
written to at all. Neither half alone is sufficient.

## Sync

`POST /api/roots/sync` takes `{slug}` and answers `{"sync": "accepted"}`. It is
**accept-only**: the route returns as soon as the root's engine has recorded the request,
never when the fetch lands, mirroring `Engine::sync` and `POST /api/roots/reconcile`'s
`{"reconcile": "scheduled"}`. The two acks are spelled apart deliberately — `scheduled` is
a level-triggered nudge over the whole home and says nothing about any one root, while
`accepted` names a root whose engine holds a pending sync — and neither decodes as the
other.

Waiting would buy nothing and cost a great deal: the fetch runs on the root's lane behind
whatever that lane is already doing, so the connection would be held open for minutes, and
the route would have to invent a second, request-scoped answer for a fact the daemon
already publishes twice. **Completion is observed, never returned:**

- `root_sync_changed` on `GET /api/events`, broadcast on *accept* and again on completion —
  so every attached view shows the in-flight state, not only the client that asked;
- the snapshot's `syncing` and `sync_note` fields, on `GET /api/roots` and on every stream
  frame that carries a snapshot.

The completion half of that arrives only for a root the engine can actually dispatch on —
see *`accepted` is recorded, not scheduled* below, which is the difference between a latched
request and a running one.

`grove sync <slug>` is the CLI half and prints the ack; `grove tree list <slug>` is where an
operator reads the result.

| the daemon finds | answer |
|---|---|
| the slug is not declared | 404 `not_found`, `{slug}` |
| declared, no engine driving it | 503 `unavailable`, `{slug}` |
| declared, engine running | 200 `{"sync": "accepted"}` |

**`accepted` is recorded, not scheduled.** The engine dispatches a pending sync only once
the root is `ready` — syncing an unrealized root is meaningless — so the ack means the
request is *latched*, not that a fetch is coming. From `cloning` or `missing` the latch is
the point: the sync runs on the drive that follows the reconcile, which is why the route
does not refuse there. `degraded` is the corner with no clock — the engine leaves it only
on a dispatched reconcile (a manifest change, or `POST /api/roots/reconcile`), so until one
arrives the root reports `syncing: true` with no second `root_sync_changed` behind it.

So a client reads `syncing` as **"this root holds a sync request"**, never as "a fetch is in
flight", and pairs it with `status` to tell the two apart: `syncing` with `ready` is work
running or about to; `syncing` with `degraded` is work parked behind a recovery nobody has
asked for yet. A UI that renders an unconditional spinner off `syncing` alone will spin
forever on a degraded root.

The 503 is the honest arm and the order is load-bearing. Declaredness is decided from the
manifest first, off any lane — it writes nothing, and an existence question must not queue
behind an hour-long clone — because without it an undeclared slug would fall into the
no-engine case and be told to retry forever. The no-engine case itself is transient: the
engine set starts a driver for every declared root on the next `roots_changed`, so it names
a window of milliseconds after a declare, or a daemon running no engine room at all.
Acknowledging there would be the one lie an accept-only contract can tell — a client
watching for `root_sync_changed` would wait forever.

The route takes **no lane**. The engine owns the dispatch, under its single background slot,
so a burst of sync requests coalesces into one fetch rather than queueing a lane job each.
A trunk carrying local commits or dirty tracked files is *reported*, never forced (carried
law 9): the sync still succeeds, and the outcome reaches the client as `sync_note`.

## Doctor

`POST /api/doctor` takes `{slug?, dry_run?, fix?}`, every field defaulting: an **empty
body** is a valid whole-home, non-dry, non-fixing run. A body that is present but carries
no `application/json` content type is a 422, not a silent default — axum's optional
extractor would read an unlabelled body as *no body*, turning a scoped dry run into a
whole-home materializing converge.

```jsonc
{
  "report":   [ /* ShareOutcome — see docs/worktree-environment.md */ ],
  "pools":    [ {"slug": "o/r", "observed": 0, "target": 2} ],
  "statuses": [ {"slug": "o/r", "status": "cloning"} ],
  "checks":   [ {"check": "worktree", "status": "mismatch", "slug": "o/r",
                 "name": "feat", "detail": "checked out on `main`, declared `feature/x`"} ]
}
```

`report` is required; the other three default to empty.

`statuses` carries every running engine's status and is the only place `cloning` and
`degraded` are observable — neither is derivable from disk. A daemon with no engine room
reports none, which is honest: nothing is driving those roots.

`checks` is the git-plumbing pass, and it is **report-only** but for one finding: `fix`
stays scoped to shares plus the `legacy-layout` migration. Every other plumbing finding is
either a human's edit to reconcile with or a job the reconciler already owns, and a doctor
that silently re-cloned under an operator asking "what is wrong?" would be the opposite of
a diagnosis.

| `check` | asks |
|---|---|
| `manifest` | does `manifest.toml` parse, and does every declaration pass the validators |
| `root` | only ever a finding: this root's pass did not answer inside its budget |
| `bare` | is `<root>/.bare` there |
| `trunk` | is the trunk checkout there — the directory this root's trunk branch names |
| `legacy-layout` | is this root laid out the old way — a bare at `.git`, a trunk at `.trunk` |
| `worktree` | is a declared worktree realized, and on the branch it declares |
| `drift` | what does git know about that the manifest does not |

`legacy-layout` is the one finding `--fix` acts on, because it is the one a reconcile
cannot reach: the root is intact and converged, it is simply named the old way. The fix
migrates it in place, each step idempotent so an interrupted run resumes — rename the bare
to `.bare` and rewrite every worktree's gitdir pointer, rename `.trunk` to the directory
the trunk branch names and rewrite the registration that points back at it, set the bare's
`HEAD`, then run the share pass, which repoints every link laid through the old name. It
verifies with a `git status` in the trunk before reporting `fixed`, and refuses — reporting
rather than guessing — when a `.bare` is already there beside a `.git`, which is a half-run
migration rather than a legacy root.

| `status` | means |
|---|---|
| `ok` | the check ran and passed |
| `missing` | declared, and not on disk / not in git |
| `mismatch` | present, but not what was declared — another branch, or detached |
| `invalid` | the declaration is rejected by the validators, so it can never realize |
| `undeclared` | on disk and undeclared; additive reconcile adopts these |
| `unavailable` | the check could not run — git would not answer, or the root exceeded its budget |

Execution: the manifest check runs once for the whole request, off any lane — it reads no
git and must answer even when every root is wedged. Each root's share converge, pool read
and plumbing checks then run as **one** job on **that root's own** lane (foreground tier)
under a 30 s budget, concurrently across roots, with reports concatenated in declared
order.

A root that misses its budget is *named rather than absent*: in the whole-home form it
contributes a `root`/`unavailable` check and every other root's answer still returns. A
request that **named** a slug is answered strictly and 422s — the caller asked about one
root, and an empty report would read as "nothing wrong".

The CLI's exit verdict comes from `report` alone: any `error` row exits 1, else N
unresolved conflicts exit 5, else success. The plumbing checks are exit-neutral, so
`grove doctor` stays wirable into CI over ordinary transient drift.

## The vocabulary fixture

`contracts/wire-vocab.json` pins, in one file: `adopt_status`, `check_kind`,
`check_status`, `cold_reason`, `error_codes`, `event_keys`, `events`, `fast_forward`,
`git_status_keys`, `health_keys`, `http_error_codes`, `log_field_keys`, `log_level`,
`log_line_keys`, `pool_view_keys`, `promotion`, `reconcile_ack`, `reconcile_status`,
`root_status`, `root_view_keys`, `share_status`, `snapshot_keys`, `sync_ack`, `sync_note`,
`task_kind`, `task_outcome`, `worktree_keys`, `worktree_outcome`, `worktree_view_keys`.

It is a generated projection of the Rust types that produce each vocabulary — `grove-ops`
for the operation ones, `grove-api` for the HTTP ones — and is never hand-edited.
`crates/grove-api/tests/wire_vocab.rs` snapshots it; the test lives in `grove-api` because
that is the end of the dependency graph that can see every producer. A rename fails
`cargo test -p grove-api`; re-bless with `BLESS_WIRE=1 cargo test -p grove-api`, which
rewrites the file byte-for-byte in the form checked in.

The lists cannot silently omit a variant. Unit vocabularies are declared through the
`wire_enum!` macro, which emits the enum, its serde spelling and its `ALL` array from one
`Variant => "spelling"` line each — so adding a variant *is* adding it to the fixture, and
the snapshot then fails until it is deliberately re-blessed. `Event`, whose variants carry
payloads, keeps a hand-written `NAMES` list guarded by a test that walks one value of every
variant through `name()`, the serde tag and `NAMES`.

**Nothing outside this repo reads the fixture.** The dashboard decodes the wire by hand —
its root decoder takes `root["status"]` as a bare string and falls back to `"unknown"` —
so a re-blessed rename reaches it as a blank badge at runtime, not a broken build. The fixture
is drift detection on *this* side: it makes a rename deliberate and reviewable here, and
gives a consuming end one file to diff. It is not a downstream guard, and no consuming end
generates types from it. There is no TypeScript codegen in this repo.

Making the claim true would take a test on the consuming side that reads
`contracts/wire-vocab.json` and asserts every vocabulary it renders is still spelled that
way. Until one exists, the cross-process rename is caught only by a human reading this
file's diff.

The fixture pins vocabulary; the *byte-for-byte* JSON — which optionals are omitted, how
each event tags itself, what an empty body decodes to — is pinned by the round-trip tests
beside the types in `grove-api`, and the live behaviour by
`crates/grove-daemon/tests/http_contract.rs` and `observe.rs` against a real daemon over
HTTP.
