# Plan · grove

Status: as of 2026-10-12 (shift s-20261012T0155Z) · v0.3.0 shipped at 99b7299 · four feature plans done, one open: #2 ghost reconcile · `rg 'TODO|FIXME|XXX' crates scripts` is empty.

The per-feature plans live in `docs/plans/` (stele-ignored). This file is the index a cold session reads first: what is open, in order, and where each item is detailed.

## Next

1. **Settle the `remove` ordering before touching it.** Ghost-reconcile target 1 (undeclare first, then delete) inverts the live invariant `※ delete-before-undeclare` (`crates/grove-ops/src/roots.rs:186`, claim in `crates/grove-ops/AGENTS.md:15`, asserted by `crates/grove-daemon/tests/scenario.rs:310`), whose rationale is that undeclaring first lets `adopt` re-declare a still-present bare. Pick one ordering and rewrite the claim with it; the plan's marker is now `roots/<slug>/cloning` (`roots.rs:556`), not `.grove-cloning`. → `docs/plans/ghost-reconcile.md` §Target 1.
2. **Cancel a stopped engine's queued work at the lane head.** `Engine::stop` only sends `Msg::Stop` (`crates/grove-daemon/src/engine/mod.rs:270`), and the reconcile lane job takes the clone permit and calls `reconcile_one` with no stop check (`mod.rs:448-454`), so a queued ghost still clones a root being removed. → `docs/plans/ghost-reconcile.md` §Target 2.
3. **Let disk win over a stale `failed`.** `apply_reconcile` maps `Applied{status: failed}` to `Degraded` without reading disk (`mod.rs:578-583`). A later `Derive(Ready)` now heals it (`crates/grove-daemon/src/engine/status.rs:59`), but under push-only nothing fires that event, so `degraded` over a `ready` disk persists until an unrelated event arrives. → `docs/plans/ghost-reconcile.md` §Target 3.
4. **Land the three ghost-reconcile tests**: a stopped engine's queued reconcile does not clone and frees its permit, `remove` racing `reconcile_one` cannot resurrect, and `failed` over a ready disk reports `ready`. None exists today. Also reword `docs/engine.md:263-264` to say that grove-ops work already running cannot be cancelled. → `docs/plans/ghost-reconcile.md` §Tests, §Out of scope.

## Shipped

| plan                                      | sha / tag          | undelivered                                                                                                                                                                                                                 |
| ----------------------------------------- | ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `docs/plans/install-home.md`              | 0dd3281 · v0.2.0   | none. Stages 1–4 are in the tree: `install_home` (`grove-ops/src/lib.rs:71`), launcher passes `GROVE_INSTALL` (`grove/src/server.rs:474`), `uninstall.sh --purge`, doctor `install-under-home`. The box has `~/.local/share/grove` and no `~/.grove/{current,versions}` |
| `docs/plans/trunk-by-branch.md`           | ea9629a · v0.2.0   | none. Manifest `trunk` (`manifest.rs:29`), `roots::trunk` (`roots.rs:76`), `git::set_head`, `worktrees::name_for`, wire `trunk_branch`, doctor `legacy-layout --fix` are all present. Its `.bare`-in-code-dir layout was superseded by store-and-pool |
| `docs/plans/legacy-reconcile-guard.md`    | b43745e · v0.2.2   | none. Refusal texts are at `roots.rs:486,498`, `RootView.error` at `grove-api/src/routes.rs:264`, rendered at `grove/src/render.rs:147`, and the doctor route re-derives migrated roots (`grove-daemon/src/routes.rs:387-416`) |
| `docs/plans/store-and-pool.md`            | 99b7299 · v0.3.0   | none. `root_dir`/`code_dir`/`bare_dir` are at `roots.rs:32-51`, `Legacy::V2Bare/V2Pool` in `layout.rs`, doctor reports `orphan-code`, `RootView.root` exists, `tree list --verbose` and `grove workspace` (`commands.rs:408`) work. The box has `~/.grove/roots` and `grove.code-workspace` |

## Sandbox notes for unattended shifts

- `cargo nextest` fails about 117 tests with EPERM in the shift sandbox: the tests that bind loopback ports, plus the `ps`-based `process_start_time` test in `crates/grove/src/server.rs`. These are sandbox denials (local binding and process inspection), not regressions. Gate those tests on a host run or after `sandbox.network.allowLocalBinding: true`.
- Coverage is not measured in the sandbox because `cargo-llvm-cov` is not installed. A shift cannot show that coverage held, so a coverage claim needs a host run.

## Sweep findings

Filled by s-20261012T0155Z after verification.
