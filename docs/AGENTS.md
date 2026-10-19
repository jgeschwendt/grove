# docs

```stele
kind: container
purpose: >-
  index.html: the Pages page, sole design + plan source (figures, decisions, Next). The
  .md files: present-tense reference — architecture, engine, worktrees, api, updates,
  deployment.
invariants:
  - claim: "these docs describe the system that exists — no migration copy, no `previously`, no decision narrative in the .md reference files (decisions live on index.html) — and a change that invalidates a claim fixes the doc in the same commit; the stele gate catches a lost anchor and lock drift outright, and leashes a doc claim by churn, so re-reading is owed on a schedule rather than left to notice"
    anchor: ※ doc-gate
```

<!-- @stele -->
<!-- @end -->
