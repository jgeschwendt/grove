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
    anchor: lm:wire-error-codes
    enforced_by: crates/grove-api/tests/wire_vocab.rs
  - claim: "an explicit root removal deletes on disk BEFORE it undeclares — undeclaring first lets the watcher's adopt re-find the still-present bare and resurrect the root; a partial delete propagates before the undeclare, so the slug stays declared and visibly broken"
    anchor: lm:delete-before-undeclare
  - claim: "a root removal refuses an UNDECLARED slug before any path reaches remove_dir_all, and refuses a declared one holding uncommitted tracked changes or unpushed commits unless the caller forces — <home>/code/<slug> is a real directory whether or not grove put it there, and the command that deletes N worktrees at once must not protect less than the one that deletes one"
    anchor: crates/grove-ops/src/roots.rs#unsaved_work
  - claim: "worktree reconcile is two-way and additive and never deletes: create declared-but-missing, adopt in-git-but-undeclared, prune only git-level ghosts whose working tree is already gone"
    anchor: lm:reconcile-additive
  - claim: "a share never destroys a human's or agent's file — only a symlink pointing under the trunk checkout (or the legacy .trunk) is grove's own and may be repointed or GC'd; a real file, a directory or a foreign symlink is a reported conflict, and --fix backs it up before linking"
    anchor: lm:never-clobber
  - claim: "a trunk carrying local commits or dirty tracked files is never forced — sync reports diverged/dirty and stops"
    anchor: lm:sync-never-forces
  - claim: "a root whose trunk checkout vanished out of band is REPAIRED by re-adding the worktree from the bare already on disk, never by wiping the code dir; a re-clone is the last resort, refused when any other live worktree hangs off the bare (the pool's slots excluded by path) and refused again when the code dir holds anything at all; and a root carrying a legacy layout, or a code dir holding any entry but .DS_Store, is refused outright — reconcile never clones into an occupied directory"
    anchor: lm:trunk-recovery-guard
  - claim: "worktree depth is load-bearing: every worktree is a direct sibling of the trunk checkout inside the code dir and shares materialize only there, so pool.fill adds a slot under roots/<slug>/pool, outside the code tree, and materializes nothing"
    anchor: lm:worktree-depth
  - claim: "the warm pool is claimed by the realizer, not by a caller: worktrees::create and worktrees::reconcile both promote a slot before cold-checking-out, so every path that realizes a declared worktree redeems the pool; and convergence runs both ways, fill adding a slot below target and reclaim giving the highest one back above it"
    anchor: crates/grove-ops/src/worktrees.rs#create
  - claim: "promote attaches the branch IN the slot before the move, so any worktree that ever reaches the user path carries a branch and an interrupted promote is adoptable rather than a stranded detached orphan"
    anchor: lm:promote-attach-before-move
  - claim: "every wall-clock read goes through the clock seam — Instant::now() appears only in grove_ops::clock, and budgets are absolute Deadlines rather than Durations re-based at each layer"
    anchor: lm:clock-seam
    enforced_by: crates/grove-ops/tests/harness_meta.rs
hazards:
  - claim: "this crate holds NO per-root mutex: its flock serializes the manifest, deliberately not git. Per-root git serialization is the caller's to supply, and a concurrent caller that does not supply it loses an invariant this crate was written against."
    anchor: lm:lane-is-callers
```

<!-- stele:begin router -->

## Anchors in this territory

- lm:clock-seam → tests/harness_meta.rs:354
- lm:delete-before-undeclare → src/roots.rs:186
- lm:files-authoritative → src/manifest.rs:296
- lm:lane-is-callers → src/env.rs:123
- lm:never-clobber → src/env.rs:274
- lm:promote-attach-before-move → src/pool.rs:164
- lm:reconcile-additive → src/worktrees.rs:252
- lm:sync-never-forces → src/roots.rs:658
- lm:test-reachability → tests/harness_meta.rs:152
- lm:trunk-recovery-guard → src/roots.rs:611
- lm:wire-error-codes → src/error.rs:48
- lm:worktree-depth → src/pool.rs:60

<!-- stele:end -->
