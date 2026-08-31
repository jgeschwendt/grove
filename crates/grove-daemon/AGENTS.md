# grove-daemon

```stele
kind: component
purpose: >-
  The resident half: axum routes, boot state, the readiness and mutation guards, per-root
  lanes, the convergence engines, the manifest watcher, the event bus and log ring, the SSE read surface.
commands:
  test: mise exec -- cargo nextest run -p grove-daemon
invariants:
  - claim: "every git-writing op for a root runs on that root's lane — reconcile (the warm-slot promote included), sync, fill, remove, worktree ops and doctor's converge — with three tiers draining foreground before read before background and no preemption of an op already running"
    anchor: lm:per-root-lane
  - claim: "a clone permit is TRIED at the head of a root's lane and never waited for there: waiting dispatch-side would hold a permit against roots whose lanes are free, and waiting inside the lane job holds the lane, so a handful of stalled clones would shed every other root's snapshot reads as unavailable while health still says ready. Failing the try, the job returns the lane at once and an off-lane waiter pokes the driver when a permit frees"
    anchor: crates/grove-daemon/src/engine/mod.rs#spawn_permit_waiter
  - claim: "POST /api/roots/sync is ACCEPT-ONLY and takes no lane of its own: it answers {sync: accepted} once the root's engine has recorded the request, so a burst coalesces through the engine's single background slot, and completion is observed on root_sync_changed / the snapshot's syncing+sync_note rather than returned. Declaredness is read off the manifest first (404), and a declared root no engine is driving is 503 — acknowledging there would be a request nothing recorded and a root_sync_changed no client ever sees"
    anchor: crates/grove-daemon/src/routes.rs#sync
    enforced_by: crates/grove-daemon/tests/http_contract.rs
  - claim: "the readiness gate 503s every non-whitelisted /api route while draining or degraded; the whitelist is exactly /api/health and /api/daemon/shutdown, so state stays observable and a stop request stays reachable"
    anchor: lm:readiness-whitelist
    enforced_by: crates/grove-daemon/tests/http_contract.rs
  - claim: "the mutation guard allows a state-changing request only with NO Origin (every non-browser client) or an Origin whose host is in a FIXED loopback set — never the request's own Host, which is what makes it a DNS-rebinding defense"
    anchor: lm:mutation-guard
    enforced_by: crates/grove-daemon/tests/http_contract.rs
  - claim: "the daemon binds only a LITERAL loopback address and refuses a hostname rather than resolving it; there is no override, and the gate runs again at bind so a hand-built Config cannot slip past it"
    anchor: lm:loopback-bind-gate
  - claim: "the status transition table is the one place a root's status changes: disk-ready always wins upward, a Derive preserves the driver-owned cloning/degraded transients, and a ReconcileError trusts disk plainly so no stale transient is stranded"
    anchor: crates/grove-daemon/src/engine/status.rs#next_status
  - claim: "the graceful drain is BOUNDED — open responses first, then lane work, on one budget under the CLI's stop grace — because an SSE response finishes only when its body is written and one peer that stopped reading would otherwise hold every stop open until SIGTERM"
    anchor: crates/grove-daemon/src/app.rs#DEFAULT_DRAIN
    enforced_by: crates/grove-daemon/tests/drain.rs
hazards:
  - claim: "a degrade is STICKY: mark_ready never clears it, only a fresh process does. A boot-time degrade must survive the unconditional post-boot ready signal, or the self-update health gate would accept the bundle that broke it."
    anchor: lm:boot-degrade-sticky
  - claim: "the mutating API is UNAUTHENTICATED until served-mode auth lands; the loopback bind gate, the readiness gate and this fixed cross-origin allowlist are the whole of the compensating control, and another app already on a loopback port can still present a loopback origin"
    anchor: lm:unauthenticated-api
edges:
  depends: [crates/grove-api, crates/grove-ops]
```

<!-- stele:begin router -->

## Anchors in this territory

- lm:boot-degrade-sticky → src/boot.rs:68
- lm:loopback-bind-gate → src/config.rs:200
- lm:mutation-guard → src/guard.rs:66
- lm:per-root-lane → src/lane.rs:172
- lm:push-only → src/engine/mod.rs:323
- lm:readiness-whitelist → src/guard.rs:21
- lm:status-is-a-cache → src/engine/status.rs:52
- lm:unauthenticated-api → src/guard.rs:31

<!-- stele:end -->
