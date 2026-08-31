# docs

```stele
kind: container
purpose: >-
  Present-tense description of the shipped system: architecture, engine, worktrees,
  worktree-environment, api, updates, deployment. History is in git.
invariants:
  - claim: "these docs describe the system that exists — no migration copy, no `previously`, no decision narrative — and a change that invalidates a claim fixes the doc in the same commit; the stele gate catches a lost anchor and lock drift outright, and leashes a doc claim by churn, so re-reading is owed on a schedule rather than left to notice"
    anchor: lm:doc-gate
```

<!-- stele:begin router -->
<!-- stele:end -->
