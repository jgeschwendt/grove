# grove

```stele
kind: component
purpose: >-
  The CLI and the single shipped binary: the clap tree, the single-realizer gate, the API
  client, process custody (on/off/reboot), setup/upgrade on hotswap, and `grove serve`, the daemon in-process.
commands:
  test: mise exec -- cargo nextest run -p grove
invariants:
  - claim: "only a connection-level failure means `no daemon`; a timeout or any other post-connect error is a daemon that exists and is slow, and must never trigger in-process realization"
    anchor: ※ reachability-tri-state
  - claim: "a daemon whose GET /api/health names a DIFFERENT home than the client was pointed at is not this home's realizer and reads as offline — GROVE_HOME and GROVE_BIND resolve independently, so without the identity on the wire a client scoped to one home delegates a destructive op to whatever answers the default bind and it is executed against the daemon's home"
    anchor: crates/grove/src/api.rs#serves_another_home
  - claim: "a busy daemon REFUSES rather than guessing, at exit 4, wherever there is no declare-only half to fall back on: a remove, since deleting in-process races the realizer and undeclaring alone leaves the bare for adopt to resurrect, and `grove sync`, which is a git write on the root and not a declaration at all"
    anchor: ※ busy-refuses-destructive
  - claim: "`grove on` polls GET /api/health in a loop until ready — the sanctioned exception to the push-only rule: a one-shot CLI has no channel for the daemon it spawned to push readiness back"
    anchor: ※ cli-health-poll
  - claim: "exit codes 1/3/4/5/6 are a published contract, and the grove_ops::Error to CliError map is what makes the offline codes reachable; an error envelope from the server is exit 1 carrying its code, its message AND error.data's detail, since two of the codes say nothing else"
    anchor: crates/grove/src/error.rs#exit_code
  - claim: "`grove setup` reloads a loaded agent whose plist changed, never the upgrade agent's: setup runs inside the swap that agent started, and its bootout would kill that swap"
    anchor: ※ upgrade-agent-kept
    enforced_by: crates/grove/tests/setup_e2e.rs
hazards:
  - claim: "`grove apply` is UNGATED, not safe: it is the only realizing command that never probes GET /api/health, so with a daemon up it reconciles every root off any lane while that root's engine may be inside the same call, racing git's locks. The gated path to the same convergence is `grove clone add` / POST /api/roots/reconcile."
    anchor: ※ apply-is-ungated
edges:
  depends: [crates/grove-api, crates/grove-daemon, crates/grove-ops]
```

<!-- @stele -->

## Anchors in this territory

- ※ apply-is-ungated → src/commands.rs:554
- ※ busy-refuses-destructive → src/commands.rs:71
- ※ cli-health-poll → src/server.rs:470
- ※ reachability-tri-state → src/api.rs:136
- ※ single-realizer → src/commands.rs:35
- ※ upgrade-agent-kept → src/setup.rs:103

<!-- @end -->
