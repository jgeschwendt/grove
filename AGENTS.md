# grove

```stele
kind: system
purpose: >-
  Cultivates git worktrees: manifest.toml declares desired state, git on disk is actual,
  grove converges them additively. One Rust workspace, one binary — `grove serve` is the daemon.
commands:
  setup: mise run setup    # toolchain components + git hooks + a warm build
  check: mise run check    # format + lint — the pre-PR gate CI runs
  test: mise run test      # nextest over the workspace + doctests
  smoke: mise run smoke    # hermetic install → update → rollback → uninstall
invariants:
  - claim: "files are authoritative — manifest.toml is desired state, git on disk is actual state, and there is no database; every daemon-held status, pool count and log line is a cache"
    anchor: lm:files-authoritative
  - claim: "reactivation is a cold boot — engine status is never persisted and is re-derived from disk, so the daemon may be killed and restarted with only the filesystem intact"
    anchor: lm:status-is-a-cache
    enforced_by: crates/grove-daemon/tests/engine.rs
  - claim: "push-based convergence, no timers — an engine picks its next background op on an event, and a failure waits for the next event rather than a retry clock (the sanctioned exception carries its own invariant: crates/grove/cli-health-poll)"
    anchor: lm:push-only
    enforced_by: crates/grove-daemon/tests/engine.rs
  - claim: "one realizer, ever — declaration is universal, but realization belongs to a reachable daemon and to the CLI only when none answers; a daemon that connects but does not answer is still a realizer, so the CLI declares and stops rather than racing it into a dual clone"
    anchor: lm:single-realizer
```

<!-- stele:begin router -->

## Hazards (5 active)

- ⚠ `crates/grove`: `grove apply` is UNGATED, not safe: it is the only realizing command that never probes GET /api/health, so with a daemon up it reconciles every root off any lane while that root's engine may be inside the same call, racing git's locks. The gated path to the same convergence is `grove clone add` / POST /api/roots/reconcile. (→ lm:apply-is-ungated)
- ⚠ `crates/grove-daemon`: a degrade is STICKY: mark_ready never clears it, only a fresh process does. A boot-time degrade must survive the unconditional post-boot ready signal, or the self-update health gate would accept the bundle that broke it. (→ lm:boot-degrade-sticky)
- ⚠ `crates/grove-ops`: this crate holds NO per-root mutex: its flock serializes the manifest, deliberately not git. Per-root git serialization is the caller's to supply, and a concurrent caller that does not supply it loses an invariant this crate was written against. (→ lm:lane-is-callers)
- ⚠ `crates/grove`: `grove up` flips the `current` symlink BEFORE the health gate — unavoidable, since served mode restarts whatever `current` points at. A crash in between strands `current` on an unproven version; recovery is the pending marker, read by the next `grove up`. (→ lm:self-update-flip)
- ⚠ `crates/grove-daemon`: the mutating API is UNAUTHENTICATED until served-mode auth lands; the loopback bind gate, the readiness gate and this fixed cross-origin allowlist are the whole of the compensating control, and another app already on a loopback port can still present a loopback origin (→ lm:unauthenticated-api)

## Map

| node      | kind      | purpose                                                                                                                                                                                                  | unfold                                                   |
| --------- | --------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| .config   | container | nextest profiles. `default` (local + pre-commit) excludes the slow tier; `ci` (NEXTEST_PROFILE=ci) runs everything and reports slow tests. The tier is a `slow_` name prefix, not an attribute.          | `stele unfold .config` · or read `.config/AGENTS.md`     |
| contracts | container | wire-vocab.json: every status string, error code, event name and payload field name on the HTTP surface, generated from the producing Rust types. Grove-side drift detection only: no consumer reads it. | `stele unfold contracts` · or read `contracts/AGENTS.md` |
| crates    | container | The Cargo workspace: grove-ops (domain), grove-api (HTTP contract), grove-daemon (tokio/axum resident), grove (CLI + `grove serve` + self-update). Edition 2024, unsafe forbidden.                       | `stele unfold crates` · or read `crates/AGENTS.md`       |
| docs      | container | Present-tense description of the shipped system: architecture, engine, worktrees, worktree-environment, api, updates, deployment. History is in git.                                                     | `stele unfold docs` · or read `docs/AGENTS.md`           |
| scripts   | container | Install, uninstall, and the release line: release.sh packs this box's bundle, publish-canary.sh ships a host-only prerelease, install.sh stages a release and hands off to `grove up`.                   | `stele unfold scripts` · or read `scripts/AGENTS.md`     |
| test      | container | The one test in this tree that is not a cargo target: install_smoke.sh drives install.sh and uninstall.sh as an operator would, hermetically, against a fixture release.                                 | `stele unfold test` · or read `test/AGENTS.md`           |

## Indexes

All invariants: `.stele/index/invariants.md` · all hazards: `.stele/index/hazards.md`

## Engine

`stele` CLI available → `stele root | unfold <id> | invariants --touching <path> | hazards | nodes --kind <k>`. MCP: `stele serve`.
No engine → everything above is complete; nested AGENTS.md files carry the detail (nearest file wins).
<!-- stele:end -->
