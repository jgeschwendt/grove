# Plan · grove

Status: as of 2026-10-12 (shift s-20261012T0155Z) · v0.3.0 shipped at 99b7299 · four feature plans done, one open (#2 ghost reconcile) · the s-20261012T0155Z sweep's open findings are below. Locations are symbols and `※` anchors, never line numbers.

The per-feature plans live in `docs/plans/` (stele-ignored). This file is the index a cold session reads first: what is open, in order, and where each item is detailed.

## Next

1. **Settle the `remove` ordering before touching it.** Ghost-reconcile target 1 (undeclare first, then delete) inverts the live invariant `※ delete-before-undeclare` (`roots::remove`, claim in `crates/grove-ops/AGENTS.md`, asserted by `crates/grove-daemon/tests/scenario.rs`), whose rationale is that undeclaring first lets `adopt` re-declare a still-present bare. Pick one ordering and rewrite the claim with it; the plan's marker is now `roots/<slug>/cloning` (`CLONING_MARKER`, flock-held while a clone is live), not `.grove-cloning`. → `docs/plans/ghost-reconcile.md` §Target 1.
2. **Cancel a stopped engine's queued work at the lane head.** `Engine::stop` only sends `Msg::Stop`, and the reconcile lane job in `Driver::dispatch` calls `reconcile_one` with no stop check (a permit is taken only when `disk_status` is `Missing`), so a queued ghost still clones a root being removed. → `docs/plans/ghost-reconcile.md` §Target 2.
3. **Let disk win over a stale `failed`.** `apply_reconcile` maps `Applied{status: failed}` to `Degraded` without reading disk. A later `Derive(Ready)` heals it (`engine/status.rs` `next_status`), but under push-only nothing fires that event, so `degraded` over a `ready` disk persists until an unrelated event arrives. → `docs/plans/ghost-reconcile.md` §Target 3.
4. **Land the three ghost-reconcile tests**: a stopped engine's queued reconcile does not clone and frees its permit, `remove` racing `reconcile_one` cannot resurrect, and `failed` over a ready disk reports `ready`. None exists today. Also reword the cancellation paragraph of `docs/engine.md` to say that grove-ops work already running cannot be cancelled. → `docs/plans/ghost-reconcile.md` §Tests, §Out of scope.
5. **Work the open sweep findings below**, highest first.

## Shipped

| plan                                   | sha / tag        | undelivered                                                                                                                                                                       |
| -------------------------------------- | ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `docs/plans/install-home.md`           | 0dd3281 · v0.2.0 | none: `grove_ops::install_home`, the launcher passes `GROVE_INSTALL` (`server.rs`), `uninstall.sh --purge`, doctor `install-under-home`                                           |
| `docs/plans/trunk-by-branch.md`        | ea9629a · v0.2.0 | none: manifest `trunk`, `roots::trunk`, `git::set_head`, `worktrees::name_for`, wire `trunk_branch`, doctor `legacy-layout --fix`; its `.bare`-in-code-dir layout was superseded |
| `docs/plans/legacy-reconcile-guard.md` | b43745e · v0.2.2 | none: `realize` refuses legacy and occupied roots, `RootView.error` is rendered by `grove tree list`, the doctor route re-derives migrated roots                                    |
| `docs/plans/store-and-pool.md`         | 99b7299 · v0.3.0 | none: `root_dir`/`code_dir`/`bare_dir`, `Legacy::V2Bare/V2Pool`, doctor `orphan-code`, `RootView.root`, `tree list --verbose`, `grove workspace`                                   |

## Sandbox notes for unattended shifts

- `cargo nextest` fails ~118 tests in the shift sandbox: those binding loopback ports (EPERM), the `ps`-based `process_start_time_is_stable_for_a_live_pid`, and on the `ci` profile the FSEvents-backed `watcher::tests::slow_an_fs_write_to_the_manifest_announces`. These are sandbox denials, not regressions; gate them on a host run.
- Coverage is not measured in the sandbox (`cargo-llvm-cov` is not installed), so a coverage claim needs a host run.
- `test/install_smoke.sh` needs a `mktemp` shim on `PATH` that roots templates under `$TMPDIR`: macOS `mktemp -d` ignores `TMPDIR` and the sandbox denies `/var/folders`.

## Sweep findings

The s-20261012T0155Z gigasweep (report and ledger kept outside the repo) confirmed 21 defects by probe; fixed on trunk: shares blocking `tree remove`, doctor on an undeclared or non-canonical slug, the clone-remove guard missing branches/stash/foreign entries, the cloning marker not telling a live clone from a crashed one, DNS-rebinding reads, ready roots starved of clone permits, the self-update rollback and pending-marker windows, folded worktree-name collisions, a malformed `GROVE_BIND`, and the canary/install script gaps. Open, highest first:

1. **Clone-permit FIFO churn** (confirmed, medium). `spawn_permit_waiter` drops the permit it acquired, so under contention a freed permit circles every parked waiter before a driver takes it. Ready roots no longer defer, which narrows it to first clones. Handing the permit over conflicts with `a_reconcile_parked_on_a_busy_lane_holds_no_clone_permit`; the fix is one shared waiter registry or a lane try-admit. A redesign, so it needs a proposal.
2. **`install_bundle` is not atomic** (confirmed, low). It runs `remove_dir_all(versions/<v>)` then extracts in place, even when `<v>` is the version `current` points at.
3. **`is_under_trunk` forgets the old trunk** (confirmed, low). After a trunk change (main→canary), share links laid through `../main/` read as foreign.
4. **`RootView.url` and `error` are served unredacted** (plausible). The Host guard closes the remote read path; local readers still see clone URLs, including any embedded credentials.
5. **`grove off` checks for any grove daemon, not this home's** (plausible, medium). It can shut down another home's daemon on the same bind.
6. **An in-flight back-off is never retried** (plausible, low). A daemon booting during an offline CLI clone reports `failed: … in flight`; the CLI finishing raises no watched event, so status stays `failed` until the next manifest change. Same shape as Next #3.
7. **Manifest writes don't fsync** (plausible, low). `write_doc` renames `manifest.toml.tmp` without fsyncing the file or its directory.
