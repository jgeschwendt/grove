# Hazards

| claim | node | anchor |
| --- | --- | --- |
| `grove apply` is UNGATED, not safe: it is the only realizing command that never probes GET /api/health, so with a daemon up it reconciles every root off any lane while that root's engine may be inside the same call, racing git's locks. The gated path to the same convergence is `grove clone add` / POST /api/roots/reconcile. | crates/grove | lm:apply-is-ungated |
| `grove up` flips the `current` symlink BEFORE the health gate — unavoidable, since served mode restarts whatever `current` points at. A crash in between strands `current` on an unproven version; recovery is the pending marker, read by the next `grove up`. | crates/grove | lm:self-update-flip |
| a degrade is STICKY: mark_ready never clears it, only a fresh process does. A boot-time degrade must survive the unconditional post-boot ready signal, or the self-update health gate would accept the bundle that broke it. | crates/grove-daemon | lm:boot-degrade-sticky |
| the mutating API is UNAUTHENTICATED until served-mode auth lands; the loopback bind gate, the readiness gate and this fixed cross-origin allowlist are the whole of the compensating control, and another app already on a loopback port can still present a loopback origin | crates/grove-daemon | lm:unauthenticated-api |
| this crate holds NO per-root mutex: its flock serializes the manifest, deliberately not git. Per-root git serialization is the caller's to supply, and a concurrent caller that does not supply it loses an invariant this crate was written against. | crates/grove-ops | lm:lane-is-callers |
