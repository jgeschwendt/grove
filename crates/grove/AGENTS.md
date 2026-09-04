# grove

```stele
kind: component
purpose: >-
  The CLI and the single shipped binary: the clap tree, the single-realizer gate, the API
  client, process custody (on/off/reboot), self-update, and `grove serve`, which runs the daemon in-process.
commands:
  test: mise exec -- cargo nextest run -p grove
invariants:
  - claim: "only a connection-level failure means `no daemon`; a timeout or any other post-connect error is a daemon that exists and is slow, and must never trigger in-process realization"
    anchor: lm:reachability-tri-state
  - claim: "a daemon whose GET /api/health names a DIFFERENT home than the client was pointed at is not this home's realizer and reads as offline — GROVE_HOME and GROVE_BIND resolve independently, so without the identity on the wire a client scoped to one home delegates a destructive op to whatever answers the default bind and it is executed against the daemon's home"
    anchor: crates/grove/src/api.rs#serves_another_home
  - claim: "a busy daemon REFUSES rather than guessing, at exit 4, wherever there is no declare-only half to fall back on: a remove, since deleting in-process races the realizer and undeclaring alone leaves the bare for adopt to resurrect, and `grove sync`, which is a git write on the root and not a declaration at all"
    anchor: lm:busy-refuses-destructive
  - claim: "`grove up`/`grove on` poll GET /api/health in a loop until ready — the sanctioned exception to the push-only rule: a one-shot CLI has no channel for the daemon it spawned to push readiness back"
    anchor: lm:cli-health-poll
  - claim: "exit codes 1/3/4/5/6/7 are a published contract, and the grove_ops::Error to CliError map is what makes the offline codes reachable; an error envelope from the server is exit 1 carrying its code, its message AND error.data's detail, since two of the codes say nothing else"
    anchor: crates/grove/src/error.rs#exit_code
  - claim: "the pending marker names the version AND the direction, written before `current` moves and cleared when the gate answers — so the next `grove up` can tell an unproven forward flip (undo it) from an interrupted rollback (already on the proven version; re-run its gate)"
    anchor: lm:pending-marker
    enforced_by: crates/grove/tests/update_e2e.rs
hazards:
  - claim: "`grove apply` is UNGATED, not safe: it is the only realizing command that never probes GET /api/health, so with a daemon up it reconciles every root off any lane while that root's engine may be inside the same call, racing git's locks. The gated path to the same convergence is `grove clone add` / POST /api/roots/reconcile."
    anchor: lm:apply-is-ungated
  - claim: "`grove up` flips the `current` symlink BEFORE the health gate — unavoidable, since served mode restarts whatever `current` points at. A crash in between strands `current` on an unproven version; recovery is the pending marker, read by the next `grove up`."
    anchor: lm:self-update-flip
    enforced_by: crates/grove/tests/update_e2e.rs
edges:
  depends: [crates/grove-api, crates/grove-daemon, crates/grove-ops]
```

<!-- stele:begin router -->

## Anchors in this territory

- lm:apply-is-ungated → src/commands.rs:329
- lm:busy-refuses-destructive → src/commands.rs:69
- lm:cli-health-poll → src/server.rs:489
- lm:pending-marker → src/update/layout.rs:178
- lm:reachability-tri-state → src/api.rs:129
- lm:self-update-flip → src/update/mod.rs:245
- lm:single-realizer → src/commands.rs:35

<!-- stele:end -->
