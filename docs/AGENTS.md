# docs

```stele
kind: container
purpose: >-
  index.html: the Pages page, sole design + plan source (figures, decisions, Next). Beside
  it, one .html page per reference: api, architecture, deployment, engine, updates,
  worktrees.
invariants:
  - claim: "these docs describe the system that exists — no migration copy, no `previously`, no decision narrative in the reference pages (decisions live on index.html) — and a change that invalidates a claim fixes the doc in the same commit; the stele gate catches a lost anchor and lock drift outright, and leashes a doc claim by churn, so re-reading is owed on a schedule rather than left to notice"
    anchor: ※ doc-gate
```

<!-- @stele -->
<!-- @end -->
