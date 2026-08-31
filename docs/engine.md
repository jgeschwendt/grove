# The engine

One driver task per declared root (`crates/grove-daemon/src/engine/`), owning that root's
status cache, its single background slot, and its warm-pool hint. `engine::RootSet` owns
the map from declared slug to running engine and reconciles it on every `roots_changed`.

The engine is the only resident part of grove that knows something disk cannot say. Status
is otherwise a cache — never persisted, re-derived from the filesystem on restart. What a
*running* engine adds is the transient: `cloning` and `degraded` are facts about work in
flight or a failure just recorded, and only the process driving the root has them.

## Status

`grove_api::RootStatus`, pinned in `contracts/wire-vocab.json` as `root_status`:

| status | means |
|---|---|
| `ready` | the bare clone **and** `.trunk` are both on disk, no failure recorded |
| `cloning` | a reconcile has been dispatched for a root not yet on disk |
| `degraded` | a terminal failure stopped the engine; it waits for an operator or a change |
| `missing` | declared, nothing on disk yet |
| `unknown` | no engine has run for this root yet — the cache has nothing to say |
| `unavailable` | **a reader's verdict**, never a state an engine enters |

`unavailable` is what a *reader* says when the engine or the lane did not answer inside
the reader's budget. Three readers produce it, each for the same reason and none of them
the engine:

- `stream::view` — a root whose one lane job of git reads missed `SNAPSHOT_BUDGET` (3 s),
  or whose read the lane shed. The row still stands, because a UI must see the root, but
  it reports `unavailable` rather than the engine's status: "ready" beside an empty
  worktree list is a lie a client cannot detect, and this root's reads are exactly what
  did not happen.
- `engine::set` — a status read that did not answer inside `STATUS_BUDGET` (5 s). A driver
  only ever awaits its mailbox, so this should never fire; it is the belt to that braces,
  and it is what keeps a whole-home doctor from hanging on one bad engine.
- the CLI — `grove tree list` reads an `unavailable` row as *no answer* and falls through
  to the disk (`commands::tree_list`).

The transition table itself can only ever *preserve* `unavailable`; it never introduces
it.

## The transition table

Every status change goes through one function, `engine::status::next_status(current,
transition)`. The question it answers is always the same: *preserve the transient the
driver owns, or trust disk?* Getting it wrong in either direction is a real defect — trust
disk too eagerly and a root mid-clone reads `missing`, so the next event dispatches a
second clone; preserve too eagerly and a finished clone stays `cloning` forever.

`disk_status(home, slug)` is the engine's one filesystem read: `Ready` when
`<root>/.git` **and** `<root>/.trunk` are both directories, else `Missing`. Both, not
either — a bare with no `.trunk` is exactly the half-realized state reconcile exists to
finish.

| current ↓ / transition → | `ReconcileDispatched` | `Derive(Ready)` | `Derive(Missing)` | `ReconcileError(Ready)` | `ReconcileError(Missing)` |
|---|---|---|---|---|---|
| `unknown` | `cloning` | `ready` | `missing` | `ready` | `missing` |
| `missing` | `cloning` | `ready` | `missing` | `ready` | `missing` |
| `cloning` | `cloning` | `ready` | **`cloning`** | `ready` | **`missing`** |
| `ready` | `ready` | `ready` | `missing` | `ready` | `missing` |
| `degraded` | `cloning` | `ready` | **`degraded`** | `ready` | `missing` |
| `unavailable` | `unavailable` | `ready` | `missing` | `ready` | `missing` |

Read as three clauses:

- **`ReconcileDispatched`** — a root not yet on disk (`missing`, `unknown`, or a
  `degraded` root retrying its clone) surfaces `cloning`; anything else is unchanged.
  Disk is deliberately not consulted: the dispatch precedes any read, and the point is to
  surface the *intent* to clone.
- **`Derive`** — disk `ready` always wins upward, because a completed clone is
  authoritative. Otherwise a driver-owned transient (`cloning`, `degraded`) is preserved
  rather than masked as `missing`, and everything else takes the honest disk verdict.
- **`ReconcileError`** — the attempt is over, so trust disk *plainly*. `cloning` is **not**
  preserved here, unlike under `Derive`; that asymmetry is the whole reason there are two
  events instead of one. Preserving it would strand a transient nothing is going to clear.

Properties the tests pin (`engine/status.rs`): the table is total over the whole cross
product and never leaves the published vocabulary; it never returns `unknown` (an entry
state only); it never *invents* `unavailable`; and re-deriving against an unchanged disk
is a fixed point, which is what makes a burst of manifest events cost nothing.

## One background slot

At most one of reconcile / sync / fill is in flight per root, chosen in that priority
order. Every condition is level-triggered state, re-evaluated on each completion
(`Driver::drive`):

1. a background op is in flight ⇒ do nothing; the next op is picked when this one lands;
2. `reconcile_pending` ⇒ mark `ReconcileDispatched` and dispatch `reconcile`;
3. `sync_pending` **and** status is `ready` ⇒ dispatch `sync`;
4. status is `ready` and the pool hint is off the declared target ⇒ dispatch `fill`,
   which adds a slot below target and reclaims the highest one above it. Convergence
   runs both ways: an add-only pool left a target lowered to zero holding its checkouts
   forever, with `doctor` reporting `n/0` and no command that could fix it. A converge
   that *fails* latches (`fill_blocked`) until the next real event, because the level
   condition it failed on is still true — without the latch the root re-dispatches into
   the same failure as fast as git can run.

The slot is taken *synchronously in `dispatch`*, before the task that will do the work
exists. That is what makes the coalescing airtight: a second trigger arriving one line
later already sees the slot occupied. It is not a lock, not a de-dupe cache, and a double
clone is unrepresentable rather than merely unlikely.

`reconcile_pending` is set at start and on every `roots_changed`, and a reconcile is
re-dispatched on every manifest change even for a realized root — the op is idempotent and
worktrees may have changed out of band. Syncing an unrealized root is meaningless, so
`sync_pending` simply waits for a later drive to find the root ready.

A reconcile folds the pool read — the declared `pool.size` **and** the observed slot
count — into the same lane job as `roots::reconcile_one`, so the manifest read never
queues separately behind the clone it follows. The `Applied` outcome and the pool read are
applied separately, so a read that succeeded still lands when the reconcile beside it
errored. The observed count is taken as truth rather than adjusted by a delta: a reconcile
is where a declared worktree *claims* a slot (`worktrees::create`/`reconcile` promote one
when the pool has any), so a hint that only ever counted fills would sit stale-high and
starve the refill that should follow.

`task_started` is published from *inside* the lane job, once the work actually begins —
so neither a reconcile queued behind its own root's lane nor one waiting on the clone
semaphore is reported as running. The signal that a queued reconcile exists is the root's
own `cloning` status. `task_finished` is published by the driver when the outcome lands.
Both name one of the three background kinds (`reconcile | sync | fill`) and a coarse
outcome (`ok | failed`); foreground work — remove, doctor — answers on its own request and
produces no frame.

## No timers

Convergence is push-based. A failure degrades one root, logs, and waits — the next event
is the retry. There is no retry timer, no poll, and no periodic sweep in the engine. The
only deadlines in the daemon are the lane's idle reap and the watcher's debounce, and both
are budgets on an event rather than a cadence.

The operator-facing consequence is that recovery signals matter: `grove doctor` prints
non-`ready` engine statuses and under-filled pools precisely because a background refill
that failed retries on the next event, not on a clock.

## Terminal vs transient

An op failure carries one of seven stable codes from `grove_ops::Error::code`
(`stele:landmark wire-error-codes`): `not_declared`, `not_ready`, `invalid_input`,
`conflict`, `network`, `git`, `io`. `grove_api::policy::classify_error` partitions them:

- **Terminal** — `conflict`, `invalid_input`. Retrying cannot help: a would-clobber
  conflict or a malformed declaration is stuck until a human edits the manifest, and that
  edit is itself the event that re-drives the engine. So the root goes `degraded`, where
  doctor can see it, instead of re-deriving to `cloning` and retrying forever.
- **Transient** — `not_declared`, `not_ready`, `network`, `git`, `io`. The attempt is
  over, so the engine takes `ReconcileError(disk)` — trusting disk plainly rather than
  stranding a stale `cloning` — and waits for the next event.

The classifier is a total match on the `Error` enum, so a new category stops the build
until it is classified. `policy::terminal_error_codes()` reads its wire strings off real
`Error` values, so a rename in `Error::code` travels rather than leaving a stale literal.

Note the deliberate asymmetry: `invalid_input`/`conflict` are the engine's terminal pair,
while the CLI's exit-code map groups `invalid_input` with `network`/`git`/`io` and pairs
`conflict` with `not_ready`. Two unrelated partitions over one taxonomy; both are contract.

A genuine clone *failure* does not arrive as an `Err` at all — it arrives as
`Applied { status: Failed }`, which `policy::classify_reconcile` maps to `degraded`
(`cloned`/`present` map to `ready`). That partition is total by construction: the match has
no catch-all, so a new `ReconcileStatus` stops the build.

A background task that returns nothing at all (`BgOutcome::Crashed`) is a third case: a
reconcile crash degrades **that root only**, a sync crash leaves a `failed` sync note, a
fill crash is logged and forgotten.

## The clone semaphore

`Deps::clones` is a global `Semaphore` with `DEFAULT_CLONE_LIMIT = 4` permits
(`GROVE_CLONE_LIMIT`), bounding how many roots may hold a clone at once. Without it a
daemon booting a home with N declared roots fans out N concurrent network fetches.

The permit is **tried at the head of the root's own lane, and never waited for there**.
Two failure modes have to be avoided at once, and only this ordering avoids both.

Acquiring dispatch-side and *then* awaiting lane admission inverts the bound: a reconcile
doing nothing at all — merely waiting out a foreground op on its own root — would hold one
of the four permits against roots whose lanes are completely free. A whole-home doctor
takes every root's foreground lane at once, so four such reconciles would stall home-wide
convergence for the length of the pass.
`a_reconcile_parked_on_a_busy_lane_holds_no_clone_permit` pins that.

*Blocking* for a permit inside the lane job is worse, because the job holds the lane while
it waits. Once the permits are all held by clones that have stalled — a peer that accepts
the connection and then goes silent, a dropped VPN, a slept laptop's half-open socket —
every other root's reconcile parks on the semaphore holding its own lane, and those roots'
snapshot reads shed as `unavailable`. The whole home then reports unavailable and converges
nothing while `/api/health` still answers `ready`: damage meant to be local to a busy root
becomes the daemon's.

So the lane job takes the permit with `try_acquire` and, failing, returns immediately —
the lane is released, reads answer again, and the reconcile is re-armed. A waiter task then
awaits a permit *off* the lane purely as a wake-up: it drops the permit it got and pokes
the driver, which re-dispatches into the same try. No timer, and no permit is ever held by
something that is not cloning.
`a_reconcile_without_a_clone_permit_frees_its_lane_and_resumes_when_one_lands` pins it.

The permit travels back to the driver with the result and is dropped *after*
`task_finished` is published, so a started/finished pair brackets a held permit exactly —
which is what makes the bound observable from the event stream rather than merely true.

The root's status stays honest while it waits: `cloning` is set when the reconcile is
dispatched, not when it reaches a permit.

## Sync

`Engine::sync` is **accept-only**: it returns as soon as the request is recorded, never
when the fetch finishes, and a re-request while one is pending or in flight coalesces into
it. `root_sync_changed` is broadcast on *accept*, so every attached view shows the in-flight
state immediately — not only the client that asked — and again on completion.

The accept records the request; the driver decides when it runs, and only from `ready`
(above). So the second broadcast follows a root the driver can dispatch on — from `cloning`
or `missing`, on the drive after its reconcile lands — while a `degraded` root, which the
engine leaves only on a dispatched reconcile, holds `syncing: true` until one arrives. That
is the level-triggered latch working as written, not a lost request, but it is why
`syncing` means "holds a sync request" rather than "is fetching".

`POST /api/roots/sync {slug}` is the one caller (`grove sync <slug>` behind it), and it
carries that semantics onto the wire unchanged: `{"sync": "accepted"}`, never a report.
The route takes no lane of its own — the engine owns the dispatch, so a burst of requests
coalesces into one fetch through the background slot rather than queueing a lane job each —
and it answers 503 `unavailable` for a declared root no engine is driving, because nothing
would record the request. See `docs/api.md` § Sync for the full contract.

`roots::sync` fetches the default branch (bounded, single-refspec), fast-forwards `.trunk`
onto the tracking ref, and prunes any warm-pool slot stranded at a pre-sync tip. It never
forces. A trunk carrying local commits or dirty tracked files is *reported*:
`SyncNote::of` maps `git::FastForward` to the note the snapshot publishes beside `syncing`
— `diverged`, `dirty`, or nothing for `updated`/`already_current`. `SyncNote::Failed` is
the synthetic member: a sync that errored, or whose task never returned, leaves it. The
next clean sync clears the note.

Pruned slots leave the pool hint stale-high, which would starve the refill guard, so the
driver decrements it by what the report says was pruned.

## Promote

There is no promote *route* and no engine-level promote call. Claiming a warm slot is a
step inside realizing a declared worktree, so it lives in the realizer that every path
reaches: `worktrees::create` (the offline `grove tree add`) and `worktrees::reconcile`
(what the engine runs after a declare-and-nudge, and after a hand-edited manifest). A
promote wired to one caller is a pool that is filled and never redeemed — paid for on
every clone, reported as `pool n/n` readiness, and claimed by nothing.

A `Promotion::Cold` is not a failure — it is the signal to cold-create, distinguishing an
empty pool (`Empty`) from a name collision (`Conflict`) — so the warm and cold paths are
interchangeable and the caller does not choose between them. The engine learns what the
claim cost from the next reconcile's pool read, and refills toward the target from there.

## Nothing blocking runs in the driver

Every `grove-ops` call leaves the driver as a task, runs on the root's lane, and comes back
as a message. The driver's own loop only ever awaits its mailbox, so `status` keeps
answering while a clone drags on for minutes.

`Engine::stop` is explicit rather than "drop the last handle": a background task holds a
sender so it can report its outcome, so an undeclared root would otherwise stay alive for
the length of its in-flight clone. In-flight work still runs to completion on the lane — it
simply reports to a mailbox nobody reads.

## The engine set

`RootSet` is one task owning the slug → engine map, so "start the declared-minus-running,
stop the running-minus-declared" is an ordinary set difference rather than a concurrency
problem. That single ownership is also what closes the undeclare → redeclare race: a slug
stopped in one pass is already out of the map before the next pass reads it, because both
passes are the same task.

It cold-boots by reading the declared set itself rather than waiting for the first
broadcast — the boot-time announcement can be published before the task subscribes, and a
set that waited for it would sit empty until the next manifest change. It is driven by the
event bus, never by the watcher directly.

## Tests

`crates/grove-daemon/tests/engine.rs` is the behavioural gate. Named highlights: a
declared root clones and reports ready; a burst of manifest events collapses into one
reconcile; a failed reconcile degrades and stops; a degraded root recovers on the next
event; a terminal fault degrades where a transient one re-derives; the sync note is set by
a diverged trunk and cleared by a clean sync; sync accepts immediately and announces its
completion; a sync accepted on a degraded root latches — `syncing` true, nothing dispatched,
one broadcast — and rides the drive that follows the recovery; a sync requested over HTTP is
accepted, announced on the stream and settles on the fetched commit (`tests/observe.rs`,
`tests/http_contract.rs`); the pool fills to target
and a declared worktree claims a warm slot; the same
declaration is cold-created when the pool is empty; a lowered pool target reclaims its
slots; the clone semaphore bounds concurrent reconciles; a reconcile without a permit frees
its lane and resumes when one lands; a restarted engine re-derives its status from disk;
the engine set mirrors the declared roots; an unreadable manifest leaves every engine
running; a reconcile parked on a busy lane holds no clone permit.

The transition table's own tests live beside it in `engine/status.rs`, and the whole-flow
scenario — declare → realize → claim a warm slot → sync → remove — is
`crates/grove-daemon/tests/scenario.rs`.
