# Architecture

<!-- stele:landmark doc-gate -->

Grove cultivates git worktrees. `manifest.toml` declares desired state, git on disk is
actual state, and grove converges actual toward desired — additively, never destroying a
human's or an agent's work.

One Rust workspace, one shipped executable. `grove serve` *is* the daemon; every other
subcommand is the CLI. A UI lives outside this repo and reads the same state through
the HTTP API.

## The four crates

```
crates/
├── grove-ops     # the domain layer: manifest · git · env · roots · worktrees · pool
│                 #   · doctor · clock · wire · error · telemetry · testfix
├── grove-api     # the shared HTTP contract: envelope, route bodies, event vocabulary
├── grove-daemon  # tokio/axum: routes, boot state, guards, lanes, engines, watcher,
│                 #   event bus, log ring, the read surface
└── grove         # the CLI + `grove serve` + process custody + self-update
```

Dependencies run one way: `grove-ops` ← `grove-api` ← `grove-daemon` ← `grove`. `grove`
also depends on `grove-ops` and `grove-api` directly, so the offline realizer and the
API client speak the same types the daemon serves. Edition 2024, resolver 3,
`unsafe_code = "forbid"` workspace-wide, `clippy::pedantic` warn-by-default with named
opt-outs (`Cargo.toml`).

`grove-ops` is a library only — no subprocess, no framing, no handshake. The CLI, the
daemon's lanes, and the offline realizer all call it in-process.

## Files are authoritative

Desired state is `manifest.toml`; actual state is git on disk. There is no database and
no second source of truth. Everything the daemon holds in memory is a cache:

- a root's engine status (`unknown | missing | ready | degraded | cloning | unavailable`)
  is re-derived from disk on every roots-changed event, and from scratch on restart;
- the warm-pool count is a hint — `pool.fill` re-observes disk before it mutates, so a
  stale hint costs at most one no-op fill;
- the log ring is a bounded tail of what this process said, not a record.

A daemon may be killed and restarted with only the filesystem intact. Nothing depends on
process continuity, and an abandoned clone is re-derived from disk on the next boot.

The manifest is edited format-preservingly through `toml_edit`, under an exclusive
advisory `flock` on a sibling `manifest.toml.lock` — never on the manifest itself, which
a write replaces by rename, so a lock on its inode would not survive its own
publication — and canonicalized (roots sorted by slug, worktrees by name, one blank line
between tables) on both grove's own writes and a human's hand-edit. `grove-ops::manifest`
is its sole owner; the CLI edits it offline and the daemon edits it in-process.

## One realizer, ever

Declaration is universal — the manifest is written by whoever runs the command.
Realization is not: it belongs to a running daemon whenever one answers, and to the CLI
only when none does.

Every command that *delegates* a state change probes `GET /api/health` first and reads
the answer three ways (`grove::api::Reachability`):

| | `Up` — a daemon answered | `Busy` — connected, no answer in time | `Offline` — connection refused |
|---|---|---|---|
| `clone add` · `tree add` | declare, then `POST /api/roots/reconcile` | declare only | realize in-process |
| `clone remove` · `tree remove` | delegate to the API | **refuse**, exit 4 | remove in-process |
| `doctor` | `POST /api/doctor` | error, exit 4 | run in-process |
| `sync` | `POST /api/roots/sync` (accept-only) | **refuse**, exit 4 | `roots::sync` in-process |
| `tree list` | the daemon's snapshot, else the disk | the disk | the disk |
| `apply` | always local | always local | always local |

Only a connection-level failure means "no daemon". A timeout, or any other post-connect
error, means something is there and slow — realizing in-process against it would race
that daemon into a dual clone. A destructive op has no declare-only analogue, so `Busy`
refuses outright rather than guessing.

`sync` refuses on `Busy` for the same reason a remove does, arrived at from the other
side: it is a git *write* on the root (fetch, fast-forward, prune stranded warm slots),
so there is nothing to merely declare and fetching here would put a second writer in a
root whose daemon may be mid-reconcile. On `Up` it delegates without waiting — the
daemon's sync is accept-only — so the outcome is read from `grove tree list` or the event
stream rather than returned; offline it runs `roots::sync` here and prints the report the
delegating arm cannot produce. A diverged or dirty trunk is exit 0 either way: carried
law 9 makes it a report, not a failure, and the two arms must not disagree about a state
only one of them can see.

`tree list` is the row that never fails on the daemon's account: a read has no realizer
to race, so a snapshot that does not arrive — or whose row the daemon marks
`unavailable` — falls back to the disk rather than reporting the daemon's empty list as
an answer.

`apply` is the deliberate exception, and the only one: it probes nothing and realizes in
this process whatever the manifest declares. It *is* the offline realizer, exposed as a
command — an operator who types it is reaching for the local one. The cost is that it
opts out of the guarantee the rest of the table buys: with a daemon up, `grove apply`
drives `roots::reconcile_one` on roots whose engine may be mid-reconcile, off any lane,
which is exactly the concurrent-caller hazard `grove-ops` warns about (`lm:lane-is-callers`,
`docs/worktrees.md` § Serialization). It is carried as a hazard on `crates/grove` rather
than as a safe path; on a box with a daemon, `grove clone add` / `POST /api/roots/reconcile`
is the gated way to ask for the same convergence.

Landmarks: `grove::commands` (`stele:landmark single-realizer`) and
`grove::api::ApiClient::reachable`.

## The daemon

`grove serve` builds a multi-thread tokio runtime, installs a `tracing` subscriber with
two sinks (stderr, and the daemon's log ring), binds, signals readiness, and serves until
the shutdown route drains the accept loop. Binding, readiness and serving are three
separate steps: a port-0 bind must report the address it got, and readiness is the
caller's signal — nothing inside the daemon may declare itself ready.

### Lanes — per-root serialization

`grove-ops` was written against a single-git-writer-per-root invariant and does not
enforce it; its `flock` serializes the *manifest*, deliberately not git. So every
git-writing op for a root — reconcile, sync, fill, promote, remove, worktree ops, doctor's
converge — goes through that root's lane, and only one runs at a time. Different roots run
in parallel. A lane is a tokio task; the work itself runs on `spawn_blocking`, because
`grove-ops` is synchronous.

Three queues, drained strictly in order, with no preemption of an op already running:

| tier | what | depth | shed behaviour |
|---|---|---|---|
| `Foreground` | a mutation a user is waiting on: remove, promote, doctor converge | 256 | overflow → `LaneError::Busy` (HTTP 503 `unavailable`) |
| `Read` | the git reads a snapshot row needs | 16 | overflow → `Busy`; a read whose caller has given up is dropped at the head of the lane |
| `Background` | the engine's convergence: reconcile, sync, fill | unbounded | coalesced desired state — at most one outstanding per root per kind, so it cannot grow |

Reads earn a tier of their own because they arrive from *polling* — one lane job per root
per snapshot, on every `GET /api/roots`, every `/api/events` connect and every bus-lag
resync. Sharing the mutation queue let a dashboard shed a `roots/remove` with 503
`unavailable`. A read whose caller has already gone is abandoned; a mutation runs to
completion however bored its caller got.

Lanes spawn lazily and reap themselves after 60 s idle. The reap and a concurrent submit
both hold the registry lock across look-up-and-enqueue / re-check-and-unregister, so no
job is ever handed to a lane that has decided to die. `Lanes::quiesced` counts accepted-
and-unfinished jobs so a drain can ask whether any git write is still in flight.

### Engine — one driver per declared root

One task per root, owning that root's status cache, its single background slot, and its
pool hint. At most one of reconcile / sync / fill is in flight, chosen in that priority
order, with every condition re-evaluated as level-triggered state on each completion. No
timers anywhere: a failure degrades one root, logs, and waits — the next event is the
retry. See `docs/engine.md`.

`engine::RootSet` owns the map from declared slug to running engine. It subscribes to the
event bus and reconciles the map on every `roots_changed`, so anything that publishes that
event updates the engines for free.

### Watcher — discover and announce

One task, three triggers, one job. The triggers are the HTTP nudge
(`POST /api/roots/reconcile`, and the poke a root removal sends after itself), an optional
`notify` watch on `$GROVE_HOME/manifest.toml`, and one announcement at boot. The job is
`roots::adopt` — declare any undeclared on-disk bare, **never clone** — then read the
declared set and publish `roots_changed`. Bursts are collapsed by a 150 ms debounce with
single-in-flight-plus-pending coalescing.

The nudge is the reliable path; the filesystem watch is opportunistic coverage for what
the CLI cannot tell the daemon about (a hand-edit, a git-synced manifest). It is off in
`Config::new` — every test drives the nudge — and on in `Config::from_env`
(`GROVE_MANIFEST_FS_WATCH=0` turns it off in production). A watcher that fails to arm
logs a warning; it never fails the boot.

The watcher never clones, and an engine never reads the manifest for anyone but itself.

### Events and logs — two channels, one wire

`events::EventBus` is a bounded (256) tokio `broadcast` carrying the four **state** events:
`roots_changed`, `root_sync_changed`, `task_started`, `task_finished`. Every one is
level-triggered — "something changed, re-read it" — so a subscriber that falls behind
loses freshness, never correctness: it is told it lagged and answers by re-reading the
world. `EventBus::publish` debug-asserts `Event::is_state`, so a snapshot or a log line
cannot reach the bus.

`logs::LogRing` is a separate channel: a bounded ring of the last 500 lines plus a
broadcast of each as it lands, fed by a `tracing` layer the *process* installs. A log line
is not level-triggered — a dropped one is gone — so sharing the bus would let a chatty
minute evict pending state events and leave a UI stale for the wrong reason. The layer
records a fixed shape (level, target, message) plus only the whitelisted fields grove's own
macros key on, each rendered through a redactor that strips URL userinfo; anything else is
dropped before it can reach a subscriber. Capture level is `GROVE_LOG_RING` (default
`info`), separate from `GROVE_LOG`, which is what the process writes to stderr.

`stream` is the read surface: `snapshot()` assembles the one `Snapshot` shape served both
as `GET /api/roots` and as the opening frame of `GET /api/events`, and `pump` produces the
stream — snapshot, then every state event, every log line, and a heartbeat comment every
15 s of silence, read off the clock seam.

### Boot state and guards

`booting → ready → stopping`, with a sticky `degraded(reason)` beside it. `mark_ready`
never clears a degrade: a fault recorded while the process was coming up must survive the
unconditional post-boot ready signal, or the self-update health gate would accept the
bundle that broke it. `mark_stopping` does override a degrade — a daemon asked to drain
*is* draining.

Two middleware layers wrap every `/api` route, readiness outermost so it runs first:

- **Readiness** 503s every non-whitelisted `/api` path while `stopping` or `degraded`.
  The whitelist is exactly `/api/health` and `/api/daemon/shutdown` — health so the state
  stays observable, shutdown so a late or repeated stop is not itself refused. `booting`
  is deliberately not gated.
- **Mutation** passes safe methods (GET/HEAD/OPTIONS) untouched. For the rest it allows a
  **missing** `Origin` (the CLI and every non-browser client send none) or an origin whose
  host is one of the fixed set `localhost`, `127.0.0.1`, `::1`; anything else is 403
  `forbidden`. The list is fixed, never the request's own `Host` — that is what makes it a
  DNS-rebinding defense.

Both, plus the loopback bind refusal in `config::guard_loopback`, are compensating controls
for an unauthenticated API and retire only when served-mode auth lands.

### Drain

`POST /api/daemon/shutdown` marks stopping *before* it answers, then schedules the accept
loop to stop after a 100 ms grace so the acknowledgement reaches the client. `Daemon::serve`
then spends one budget (`drain_budget`, default 5 s, armed off the clock seam at the
trigger) in two phases: open responses first, then lane work via `Lanes::quiesced`. What
does not land in either phase is logged by slug and abandoned.

The bound is not a nicety. An SSE response finishes only when its body stops being written,
so one peer that stopped reading would hold `grove serve` open forever and make every stop
end at SIGTERM. The default sits under the CLI's own 10 s stop grace — pinned from both
sides by `the_drain_budget_fits_inside_the_cli_stop_grace` and
`the_budgets_stay_in_their_intended_order`.

### Process custody

Custody lives in the CLI (`grove::server::ServerControl`), not the daemon: `grove on`
launches the grove binary detached as `<launcher> serve`, tracks it by
`$GROVE_HOME/grove.pid`, and gates readiness on `GET /api/health`. `grove off` drains over
the API, then escalates SIGTERM → SIGKILL. A pid is re-verified before every escalation
signal — liveness plus a start-time identity token — so a recycled pid reads as stale
rather than as grove still running. A bind held with no pid file refuses the start rather
than adopting a stranger. `grove reboot` is off then on.

## The wire contract

`grove-api` is the one place the HTTP shape is written down, and both ends compile against
it. Three properties it exists to hold:

1. **One serializer.** `Envelope` is the only thing that writes `{ok, …}`.
2. **Discrimination on `ok`.** The envelope is tagged on its boolean, not on which of
   `data`/`error` happens to be present, and a body that disagrees with its own tag is a
   decode error.
3. **Typed vocabularies.** Error codes, root status, sync notes, boot status, event names
   and the engine's policy curations are enums, declared through the `wire_enum!` macro so
   the variant list, the serde spelling and the `ALL` array are one piece of text.

`contracts/wire-vocab.json` is the fixture: every vocabulary plus a key set per payload
shape, all derived from the producing types by `crates/grove-api/tests/wire_vocab.rs`. Each
key set is one object deep, so a nested shape carries its own group (`git_status_keys`,
`log_field_keys`) — one added under an existing shape and not given a group is pinned by
nothing. A rename fails that test; re-bless with
`BLESS_WIRE=1 cargo test -p grove-api`. Nothing outside this repo reads the fixture — the
dashboard decodes the wire by hand — so the guard is grove-side only: a rename is caught
here, deliberately re-blessed here, and still reaches a consumer as a runtime miss.
The byte-for-byte JSON — which optionals are omitted, how each event tags itself — is
pinned by round-trip tests beside the types in `grove-api`. Details in `docs/api.md`.

## The clock seam

`grove_ops::clock` is the one place `Instant::now()` is called. Every wall-clock budget is
a `Deadline` read off an injected `Clock`: production wires `SystemClock`, tests wire
`TestClock` and step time by hand, so a "60 s idle reap" test costs milliseconds and cannot
flake on a loaded machine. `Deadline` is absolute rather than a re-based `Duration`, so
handing it down a call chain cannot multiply the budget by the chain's depth.

The seam is enforced, not conventional:
`harness_meta::every_wall_clock_read_goes_through_the_clock_seam` fails the build the moment
an `Instant::now()` appears outside `clock.rs`.

`tokio::time` has a clock of its own, so the daemon's two timers do not use it directly:
`wait::until` / `wait::within` poll the injected clock on a 50 ms tick, which is what lets
a fake-clock `advance()` end a 60 s budget inside a test.

## Telemetry

`grove_ops::telemetry` carries an OpenTelemetry span exporter behind `GROVE_TELEMETRY`,
with a redacting URL helper and the span attribute discipline (`grove.op`, `grove.outcome`,
`grove.slug`, redacted `grove.url`). The transport is a simple (synchronous) span exporter
over blocking HTTP, and it has no default destination: with `GROVE_TELEMETRY` on but
`GROVE_OTLP_TRACES_ENDPOINT` unset, `init` hands back the same no-op handle it hands back
when telemetry is off. A daemon that wires this up wants a batch exporter first, so an
export cannot sit on a runtime thread.

**No binary in this tree calls `telemetry::init` today** — neither `grove serve` nor any
CLI dispatch path — so the module ships unwired: the daemon's observability is the
`tracing` subscriber (stderr plus the log ring) described above. The module's own doc
comment still describes the daemon replacing the transport as a future step.

## Testing and gates

`mise run check` is format plus lint; `mise run test` is `cargo nextest run --workspace`
plus `cargo test --doc --workspace`; `mise run smoke` drives `scripts/install.sh` and
`scripts/uninstall.sh` against a fixture release. CI runs exactly those three, plus the
stele graph gate below.

Two tiers, by naming convention: a test whose name begins with `slow_` is excluded by the
default nextest profile and included by `NEXTEST_PROFILE=ci` (`.config/nextest.toml`). The
tier is visible at the call site and needs no registration anywhere.

## The architecture graph

The repo carries a [stele](https://github.com/jgeschwendt/stele) graph: one node per
container and per crate, declared in that directory's `AGENTS.md`, each with a purpose, the
commands that prove it, and its invariants and hazards. Every claim is anchored to a
`// stele:landmark <slug>` comment (or a `path#symbol`) in the code it governs. A claim
proved by a *separate* test target names it in `enforced_by:`; a claim proved by the inline
`mod tests` beside its own anchor names nothing, deliberately — `enforced_by` exempts a
claim from digest-staling, and the digest gate is the only thing watching a region whose
tests live inside it. `.stele/graph.lock` is the compiled
graph; the `<!-- stele:begin router -->` regions and `.stele/index/` are projections of it.
`.steleignore` keeps `plans/`, `.github/` and `.claude/` out of the scan.

```sh
stele check         # anchors resolve, dependencies match, every directory is routed,
                    # freshness holds, documented commands still resolve
stele emit --check  # the committed projections still match the lock
stele build         # recompile the lock and re-stamp each claim's `verified` mark
```

`check` and `emit --check` run in the lefthook pre-push hook and in CI. **`build` runs in
neither** — it re-stamps freshness, so a gate that ran it would launder staleness into a
green tick. When `check` reports a stale claim, re-read the anchored code, satisfy yourself
the claim still holds, then `stele build` by hand and commit the lock alongside the change
that staled it.

`crates/grove-ops/tests/harness_meta.rs` is the meta-check that keeps the harness honest:
every test file is reachable from `mise run test`, CI runs the same gate as local, the
install smoke is reachable from a gate, the slow tier is configured and inhabited, every
wall-clock read goes through the seam, no scratch directory is parked in a `static`, and no
test hides behind `#[ignore]`. It reads the *committed* tree via `git ls-files` (through
`grove_ops::git::git_command`, so an inherited `GIT_DIR` cannot redirect it) and refuses an
inventory that does not contain itself.
