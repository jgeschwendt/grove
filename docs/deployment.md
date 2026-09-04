# Deployment

## Posture: loopback-only, unauthenticated, guarded

The API is **unauthenticated**. Served-mode auth is a later phase, so three compensating
controls stand in its place, and all three retire together when it lands:

1. **The bind gate.** `GROVE_BIND` accepts a **literal** `ip:port` and only a loopback
   address. A hostname is refused rather than resolved — what a name points at is not the
   daemon's to decide, and a gate applied after resolution is a gate applied to whatever
   the resolver said that second. There is no override, and the check runs both in
   `Config::new` and again at `Daemon::bind`, so a hand-built config cannot slip past it.
2. **The readiness gate.** While draining or degraded, every `/api` path but
   `/api/health` and `/api/daemon/shutdown` answers 503.
3. **The mutation guard.** A state-changing request must carry no `Origin` (every
   non-browser client) or one whose host is `localhost`, `127.0.0.1` or `::1` — a fixed
   list, never the request's own `Host`, which is what makes it a DNS-rebinding defense.

Residual and accepted: another process already on a loopback port can present a loopback
origin, and any local user who can reach the port can drive the API. Both need request
authentication to close. Putting grove on a LAN is another service's job; grove refuses to
do it itself.

The CLI's client is deliberately looser than the daemon: it accepts a hostname in
`GROVE_BIND`, because it only has to reach whatever is listening, and it builds its HTTP
client with `no_proxy` so an `HTTP_PROXY` in the operator's environment cannot sit between
it and a daemon on its own loopback address.

## Reactivation is a cold boot

The daemon may be killed at any moment and restarted with only the filesystem intact.
Nothing depends on process continuity:

- desired state is `manifest.toml`, actual state is git on disk, and there is no database;
- engine status, the pool count and the log ring are caches, re-derived or simply empty on
  the next boot;
- an abandoned clone is re-derived from disk — a bare with no `.trunk` is exactly the
  half-realized state `reconcile_one` finishes;
- `roots::adopt` at boot re-declares any on-disk bare that lost its manifest entry.

So a substrate may stop, snapshot, move and restart the process freely. The bounded drain
(`docs/architecture.md`) is a courtesy that lets in-flight git work land, not a
precondition: a daemon killed mid-clone converges on the next boot.

## What the substrate must provide

| need | why |
|---|---|
| a Unix host — Linux or macOS | `grove-ops` uses `openat`/`renameat`/`symlinkat` through `rustix`; there is no Windows path |
| `git` on `PATH` | worktree, status, prune, fetch and remote reads shell out to git (`grove_ops::git::git_command`, which pins `LC_ALL=C` and scrubs git's local-repo environment). The bare *clone* itself goes through `gix` in-process |
| a writable `$GROVE_HOME` — the workspace | default `~/.grove`; holds `manifest.toml` and its advisory `manifest.toml.lock` (manifest read-modify-write), every checkout under `code/`, and the daemon's `grove.lock`, `grove.pid` and `grove.log`. The valuable half: nothing here is regenerable from a release |
| a writable `$GROVE_INSTALL` — the install | default `~/.local/share/grove`; holds `versions/`, the `current` and `previous` symlinks, `channel`, `pending`, and the advisory `update.lock` (concurrent `grove up`). Disposable: delete it and re-install, and no repo notices |
| a filesystem with symlinks | shares in the workspace, and the whole `current`/`previous` install layout |
| outbound HTTPS/SSH to the git remotes | clone and fetch |
| loopback networking | the API |

No database, no message broker, no second runtime. One executable.

Released platforms are `aarch64-darwin`, `x86_64-linux` and `aarch64-linux`. Intel macOS
(`x86_64-darwin`) is not built — no runner — and both `install.sh` and `grove up` refuse it
up front with that reason rather than 404ing on an asset URL that was never published.

## Configuration

| variable | default | effect |
|---|---|---|
| `GROVE_HOME` | `~/.grove` | the workspace grove realizes — `manifest.toml`, `code/`, `grove.{lock,pid,log}` — resolved once, in `grove_ops::home`, for the CLI, the launcher and the daemon alike |
| `GROVE_INSTALL` | `$XDG_DATA_HOME/grove` when that is set, else `~/.local/share/grove` | the install root `grove up` flips and the launcher runs out of — `versions/`, `current`, `previous`, `channel`, `pending`, `update.lock` — resolved once, in `grove_ops::install_home` |
| `GROVE_BIND` | `127.0.0.1:7777` | where the daemon listens (daemon: literal loopback only) |
| `GROVE_LOG` | `info` | `EnvFilter` directive for what the process writes to stderr (ANSI colour only when stderr is a terminal, so `grove.log` stays greppable) |
| `GROVE_LOG_RING` | `info` | the level at which lines enter the ring `GET /api/events` streams |
| `GROVE_MANIFEST_FS_WATCH` | on | the opportunistic `manifest.toml` filesystem watch |
| `GROVE_CLONE_LIMIT` | 4 | how many roots may hold a clone at once |
| `GROVE_CLONE_TIMEOUT_SECS` | 3600 | clone budget; `0` disables |
| `GROVE_FETCH_TIMEOUT_SECS` | 600 | fetch budget; `0` disables |
| `GROVE_CHANNEL` | — | overrides the persisted release channel for a `grove up` |
| `GROVE_HEALTH_GATE_SECS` | 30 | how long `grove up` waits for the new version to report ready |
| `GROVE_INSTALL_BASE_URL` | — | install from this release base instead of GitHub |
| `GROVE_LINK_DIR` | — | where `install.sh` puts the `grove` PATH symlink (and where `uninstall.sh` looks for it); unset, the script searches `/usr/local/bin` then `~/.local/bin` |
| `GROVE_MODE` | — | `served` makes `grove up` stop rather than restart, handing off to a supervisor |
| `GROVE_OTLP_TRACES_ENDPOINT` | — | the OTLP/HTTP collector the span exporter posts to. Required: there is no default destination, so `GROVE_TELEMETRY` without it stays a no-op |
| `GROVE_TELEMETRY` | off | opts `grove-ops`' OpenTelemetry span exporter in — see the note in `docs/architecture.md` § Telemetry: no binary in this tree initializes it today |

An unparseable `GROVE_BIND` is a loud error in both the daemon and `ServerControl`, never a
silent default — otherwise `on`/`off` would target one address while the API client talked
to another.

**The two are independent, so they are cross-checked.** `GET /api/health` carries the home
the daemon realizes, and the CLI treats a daemon serving a *different* home as absent: this
home has no realizer, so it realizes in-process against the home it was pointed at. Setting
`GROVE_HOME` alone — a second home, a sandbox, a script — leaves `GROVE_BIND` on its own
default, which is exactly how a client scoped to one home ends up talking to another's
daemon; before the check, that delegated `clone remove` deleted from the daemon's home and
printed success.

## Running it

```sh
grove serve    # the launcher contract: run the daemon in this process, in the foreground
grove on       # start it detached; stdout+stderr land in $GROVE_HOME/grove.log
grove off      # drain over the API, then SIGTERM → SIGKILL if it will not go
grove reboot   # off, then on
grove ok       # is it healthy?
```

`grove serve` is the published launcher string: `grove on` spawns `<binary> serve`, and any
supervisor unit should spell it the same way (`serve_is_the_launcher_subcommand` pins it).
For an installed release the launcher is `$GROVE_INSTALL/current/bin/grove`; with nothing
installed it is the running executable itself. The spawned child is handed both roots —
`GROVE_HOME` so it realizes the manifest the operator is looking at, `GROVE_INSTALL` so its
own `grove up` flips the tree it was launched out of.

Under a supervisor, run `grove serve` in the foreground and let the supervisor own restarts:
the process ends by returning from `serve`, never by calling `exit`.

Two daemons are kept off one home by `grove on`, not by `grove serve`: `on` serializes its
liveness-check → spawn → pid-write under an exclusive `flock` on `$GROVE_HOME/grove.lock`
(opened `O_CLOEXEC`, so the launched daemon does not inherit and hold it), refuses when a
pid file names a live process, and refuses when the bind is already held with **no** pid
file rather than adopting a stranger — a spawn there is doomed to `EADDRINUSE`, and worse,
the squatter's 200 could answer grove's own readiness poll. A bare `grove serve` takes no
such lock and simply fails to bind.

For a supervised box set `GROVE_MODE=served`, which makes `grove up` stop the daemon and let
the supervisor bring it back on whatever `current` points at, rather than restarting it
itself.

## Not here

- **Auth / served mode** — the loopback posture above stands until it lands.
- **UI** — a dashboard reads `GET /api/roots` and `GET /api/events` and mutates only
  through the CLI or the API. This repo ships no UI.
- **A remote or multi-tenant deployment** — grove realizes one home on one host.
