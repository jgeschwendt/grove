# grove-ops

```stele
kind: component
purpose: >-
  The domain layer, library only: manifest, git, roots, worktrees, pool, shares (env),
  doctor, the clock seam and the wire vocabulary. Everyone with local filesystem access calls it in-process.
commands:
  test: mise exec -- cargo nextest run -p grove-ops
invariants:
  - claim: "every op failure carries one of seven stable snake_case codes (not_declared | not_ready | invalid_input | conflict | network | git | io); the daemon's engine reads the category to tell a terminal failure from a transient one"
    anchor: ※ wire-error-codes
    enforced_by: crates/grove-api/tests/wire_vocab.rs
  - claim: "an explicit root removal deletes on disk BEFORE it undeclares, and that ordering is settled: undeclaring first lets the watcher's adopt, which runs off the lane, re-find the still-present bare and resurrect the root, while the declared-but-missing window it would close is invisible to reconcile — remove holds the root's lane throughout, and reconcile_one re-reads the declaration under that lane; a partial delete propagates before the undeclare, so the slug stays declared and visibly broken"
    anchor: ※ delete-before-undeclare
  - claim: "a root removal refuses an UNDECLARED slug before any path reaches remove_dir_all, and refuses a declared one holding uncommitted tracked changes, unpushed commits on any local branch, a stash, or a code-dir entry that is no worktree of its bare unless the caller forces — <home>/code/<slug> is a real directory whether or not grove put it there, and the command that deletes N worktrees at once must not protect less than the one that deletes one"
    anchor: crates/grove-ops/src/roots.rs#unsaved_work
  - claim: "worktree reconcile is two-way and additive and never deletes: create declared-but-missing, adopt in-git-but-undeclared, prune only git-level ghosts whose working tree is already gone"
    anchor: ※ reconcile-additive
  - claim: "a share never destroys a human's or agent's file — only a symlink climbing to the trunk checkout exactly as grove writes it (or the legacy .trunk, or the old trunk in the one pass a trunk change runs before moving HEAD) is grove's own and may be repointed or GC'd; a real file, a directory or a foreign symlink is a reported conflict, and --fix backs it up before linking"
    anchor: ※ never-clobber
  - claim: "a trunk carrying local commits or dirty tracked files is never forced — sync reports diverged/dirty and stops"
    anchor: ※ sync-never-forces
  - claim: "a root whose trunk checkout vanished out of band is REPAIRED by re-adding the worktree from the bare already on disk, never by wiping the code dir; a re-clone is the last resort, refused when any other live worktree hangs off the bare (the pool's slots excluded by path) and refused again when the code dir holds anything at all or the bare holds an unpushed branch or a stash; and a root carrying a legacy layout, or a code dir holding any entry but .DS_Store, is refused outright — reconcile never clones into an occupied directory"
    anchor: ※ trunk-recovery-guard
  - claim: "worktree depth is load-bearing: every worktree is a direct sibling of the trunk checkout inside the code dir and shares materialize only there, so pool.fill adds a slot under roots/<slug>/pool, outside the code tree, and materializes nothing"
    anchor: ※ worktree-depth
  - claim: "the warm pool is claimed by the realizer, not by a caller: worktrees::create and worktrees::reconcile both promote a slot before cold-checking-out, so every path that realizes a declared worktree redeems the pool; and convergence runs both ways, fill adding a slot below target and reclaim giving the highest one back above it"
    anchor: crates/grove-ops/src/worktrees.rs#create
  - claim: "promote attaches the branch IN the slot before the move, so any worktree that ever reaches the user path carries a branch and an interrupted promote is adoptable rather than a stranded detached orphan"
    anchor: ※ promote-attach-before-move
  - claim: "every wall-clock read goes through the clock seam — Instant::now() appears only in grove_ops::clock, and budgets are absolute Deadlines rather than Durations re-based at each layer"
    anchor: ※ clock-seam
    enforced_by: crates/grove-ops/tests/harness_meta.rs
hazards:
  - claim: "this crate holds NO per-root mutex: its flock serializes the manifest, deliberately not git. Per-root git serialization is the caller's to supply, and a concurrent caller that does not supply it loses an invariant this crate was written against."
    anchor: ※ lane-is-callers
```

<!-- @stele -->

## Anchors in this territory

- ※ clock-seam → tests/harness_meta.rs:354
- ※ delete-before-undeclare → src/roots.rs:211
- ※ files-authoritative → src/manifest.rs:331
- ※ lane-is-callers → src/env.rs:124
- ※ never-clobber → src/env.rs:441
- ※ promote-attach-before-move → src/pool.rs:163
- ※ reconcile-additive → src/worktrees.rs:290
- ※ sync-never-forces → src/roots.rs:896
- ※ test-reachability → tests/harness_meta.rs:152
- ※ trunk-recovery-guard → src/roots.rs:831
- ※ wire-error-codes → src/error.rs:48
- ※ worktree-depth → src/pool.rs:60

<!-- @end -->
