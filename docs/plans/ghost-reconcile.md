# Plan: a stopped engine must not keep writing the root

Status: proposed · issue [#2](https://github.com/jgeschwendt/grove/issues/2) · authored 2026-09-05

## Problem

`Engine::stop` is explicit and deliberately soft: in-flight lane work runs to completion
and reports to a mailbox nobody reads (`docs/engine.md`). The engine set's single owner
closes the undeclare → redeclare race on the _map_ — never two engines for one slug. It
does not close the race on the _lane_: the stopped engine's reconcile job is still queued
or running on the shared `deps.lanes`, still holds a clone permit, and still calls
`realize`, which repairs a declared-but-missing root by cloning it.

Two operator-visible consequences.

**A ghost reconcile re-clones a root being removed.** `roots::remove` deletes the root
directory _before_ it edits the manifest. Between those steps the root is declared and
missing — precisely the state `realize` exists to repair. A reconcile that reaches `realize`
in that window (the start-up reconcile, a watcher tick, or one queued behind a foreground
op on the slug's lane) performs a full network clone. The manifest edit then stops the
engine, but the clone finishes anyway: an undeclared `code/<owner>/<repo>` reappears
after `grove clone remove` reported it gone, `.grove-cloning` is already removed, and
`adopt` re-declares it on the next manifest event.

**`degraded` over a `ready` disk is never re-derived.** `apply_reconcile` maps
`Applied{status: failed}` to `degraded` without consulting disk, and `next_status` keeps
`degraded` on `Derive(missing)`. A failure whose cause has already resolved — a collision
with a concurrent writer, a transient git error after the clone landed — leaves the root
reported `degraded` indefinitely while `disk_status` says `ready`. This is what the CI red
fixed in 3dfa29c looked like from outside: not a hang, a stuck cache.

## Evidence

`a_restarted_engine_re_derives_its_status_from_disk` before 3dfa29c: the restarted
engine reports `ready` from disk, its start-up reconcile is still ahead of `realize` when
the test deletes the root, the ghost re-clones, the third engine's clone collides with it
and reports `failed`, and the root sits `degraded` over a `ready` disk for the whole
budget. Deterministic on CI's runners and under any added delay locally; the trunk-by-branch
`converge_trunk` widened the pre-`realize` window with extra git calls, which is when it
started failing. The test-side fix waits for the ghost's `TaskFinished{reconcile}`; the
daemon-side hazard is untouched.

## Target

A stopped engine does no further writes to its root, and status never reports a
driver-owned failure over a disk that contradicts it.

1. **`remove` undeclares first, then deletes.** A ghost that reads the manifest after the
   edit gets `NotDeclared` — a transient error, so the engine re-derives — and never
   reaches `realize`. Refuse, or wait, while `.grove-cloning` exists under the root: the
   marker `adopt` already honours means a clone is mid-flight.
2. **Cancel at the lane head.** `stop` sets a flag the lane job checks before `started()`;
   a queued job that finds it set returns without running. Work already inside grove-ops
   cannot be cancelled, but a queued job need not begin, and the clone permit is never
   taken for an engine that is gone.
3. **Disk wins over a stale failure.** In `apply_reconcile`, a `failed` outcome over a
   `ready` disk publishes the failure on the bus but takes disk for status, mirroring the
   `ReconcileError` arm. `degraded` stays reserved for a root whose disk agrees it is not
   ready.

## Tests

- engine: stop an engine while its reconcile is queued behind a foreground lane op, delete
  the root, undeclare it, assert nothing re-clones and the clone permit is free;
- `roots::remove`: a `reconcile_one` racing the two steps cannot resurrect the root;
- `next_status` / `apply_reconcile`: `failed` with `disk_status == ready` reports `ready`.

## Out of scope

Cancelling grove-ops work already running (a clone mid-transfer); `docs/engine.md`'s
"reports to a mailbox nobody reads" remains true for that case and should say so.
