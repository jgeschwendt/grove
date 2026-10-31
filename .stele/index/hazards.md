# Hazards

| claim | node | anchor |
| --- | --- | --- |
| `grove apply` is UNGATED, not safe: it is the only realizing command that never probes GET /api/health, so with a daemon up it reconciles every root off any lane while that root's engine may be inside the same call, racing git's locks. The gated path to the same convergence is `grove clone add` / POST /api/roots/reconcile. | crates/grove | ※ apply-is-ungated |
| a degrade is STICKY: mark_ready never clears it, only a fresh process does. A boot-time degrade must survive the unconditional post-boot ready signal, or `grove upgrade`'s health smoke would accept the release that broke it. | crates/grove-daemon | ※ boot-degrade-sticky |
| the mutating API is UNAUTHENTICATED until served-mode auth lands; the loopback bind gate, the readiness gate and this fixed loopback allowlist — checked against every request's Host and every mutation's Origin — are the whole of the compensating control, and another app already on a loopback port can still present a loopback origin | crates/grove-daemon | ※ unauthenticated-api |
| this crate holds NO per-root mutex: its flock serializes the manifest, deliberately not git. Per-root git serialization is the caller's to supply, and a concurrent caller that does not supply it loses an invariant this crate was written against. | crates/grove-ops | ※ lane-is-callers |
