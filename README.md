# grove

A source-control harness. `manifest.toml` declares desired state; git on disk is actual
state; grove converges the two — additively, never destroying a human's or agent's work.

`docs/` describes what is here: [architecture](docs/architecture.md) (the crates, the
daemon's parts, the seams), [engine](docs/engine.md) (status, the background slot,
terminal vs transient), [worktrees](docs/worktrees.md) (the manifest, the layout,
reconcile, the warm pool), [worktree-environment](docs/worktree-environment.md) (shares),
[api](docs/api.md) (routes, envelope, events, doctor), [updates](docs/updates.md)
(install, flip, gate, release), [deployment](docs/deployment.md) (the posture and what a
host must provide).

## The commands

```sh
grove clone add <url>          # declare a repo, and clone it
grove clone remove <slug> [--force]   # delete it from disk, then undeclare it
grove tree add <slug> <branch> [--base <ref>]   # a worktree, named branch-with-/-as-
grove tree list <slug>         # declared ⋈ actual worktrees
grove tree remove <slug> <name>
grove sync <slug>              # fetch, and fast-forward .trunk — never forced
grove apply                    # realize the whole manifest, locally
grove doctor [slug] [--dry-run] [--fix]   # converge shares; report conflicts + plumbing
grove ok                       # is the server healthy?
grove up [--version <v>] [--channel <c>] [--rollback]   # self-update; see below
grove version                  # what this binary is
```

**One realizer, ever.** Declaring is universal — the manifest is written by whoever
runs the command — but *realizing* belongs to a running server whenever one answers,
and to the CLI only when none does. Each command that *delegates* a state change
probes `GET /api/health` first and reads the answer three ways:

| | server answers | server accepts but stalls | nothing listening |
|---|---|---|---|
| `clone add` · `tree add` | declare, then nudge it to reconcile | declare only — its watcher will | clone/create in-process |
| `clone remove` · `tree remove` | delegate | **refuse** (exit 4) | remove in-process |
| `doctor` | `POST /api/doctor` | error (exit 4) | run in-process |
| `sync` | `POST /api/roots/sync` — accepted, not awaited | **refuse** (exit 4) | fetch in-process, print the report |
| `tree list` | the server's snapshot — status and pool included | read the disk | read the disk |
| `apply` | always local | always local | always local |

A stalled server is the subtle case: it is still a server, and still has a realizer, so
realizing in-process would race it into a dual clone. A destructive op has no
declare-only analogue, so it refuses outright rather than guessing — and neither does a
`sync`, which is a git write on the root rather than a declaration, so it refuses too.

`sync` delegates without waiting: the server's sync is accept-only, so the command prints
that the request was accepted and `grove tree list <slug>` is where the outcome shows up.
Offline there is no realizer to wait on, so the fetch happens here and the report prints.
Either way a `.trunk` carrying local commits or dirty tracked files is *reported*, never
forced.

"A server answers" means *this home's* server. `GROVE_HOME` and `GROVE_BIND` are
independent, so a client scoped to one home routinely finds a stranger's daemon on the
default bind; `GET /api/health` carries the home it realizes, and a daemon serving a
different one reads as "nothing listening" — this home has no realizer, so the CLI
realizes here, against the home you named. Without that, `GROVE_HOME=~/.grove-dev grove
clone remove o/r` deleted `~/.grove/code/o/r` and printed success.

`apply` is the one deliberate exception: it probes nothing and realizes the whole
manifest here, in this process. That makes it the offline realizer exposed as a command,
and it is ungated rather than safe — with a server up it reconciles off any lane while
that root's engine may be doing the same. On a box with a server running, `clone add` and
`tree add` are the gated way to ask for the same convergence.

`tree list` is the row that never fails on the server's account: a read has no realizer
to race, so a snapshot that does not arrive — or one whose row the server marks
`unavailable`, its word for "this root's reads missed their budget" — falls back to the
disk rather than erroring or reporting the server's empty list as an answer.

`clone remove` is guarded: it surveys every worktree under the root for uncommitted
tracked changes and unpushed commits, and refuses — naming them — unless you pass
`--force`. `tree remove` already refuses a dirty checkout (git's own guard), and the
command that deletes N of them at once must not protect less than the one that deletes
one.

Exit codes are a contract:
`1` general/API · `3` not found · `4` server unreachable or busy · `5` conflict · `6`
unhealthy · `7` self-update. `3` and `5` are the in-process realizer's — an undeclared
root or worktree, doctor's unresolved conflicts, a guarded remove's refusal. An error the
*server* returned is always exit 1, carrying its `code: message` and `error.data`'s
detail: the HTTP taxonomy is a separate partition and is not remapped onto these codes.

## The server

One binary: `grove serve` *is* the daemon, and `grove on` launches that same
executable detached, tracks it by `$GROVE_HOME/grove.pid`, and gates readiness on
`GET /api/health`.

```sh
grove serve    # run it in the foreground (the launcher contract)
grove on       # start it detached; logs to $GROVE_HOME/grove.log
grove off      # drain over the API, then SIGTERM → SIGKILL if it won't go
grove reboot   # off, then on
```

A UI reads the same state two ways: `GET /api/roots` answers a snapshot of every
declared root — status, pool, sync note, trunk, worktrees — and `GET /api/events` is
that same snapshot followed by a live SSE stream of what changes, plus a tail of the
server's own log. Both surfaces are pinned in `contracts/wire-vocab.json`: every
vocabulary (statuses, error codes, event names) plus the field names of every payload
shape, all derived from the producing types. The byte-for-byte JSON — which optionals
are omitted, how each event tags itself — is pinned by the round-trip tests beside
those types in `grove-api`.

`GROVE_HOME` (default `~/.grove`) is the home it realizes; `GROVE_BIND` (default
`127.0.0.1:7777`) is where it listens. `GROVE_LOG` sets what the process writes to
its log file, `GROVE_LOG_RING` (default `info`) what the event stream carries. The daemon accepts only a **literal loopback**
address there and refuses anything else at startup: the API is unauthenticated until
served-mode auth lands, so loopback-only plus the cross-origin mutation guard are the
compensating controls, and there is no override.

## Installing and updating

```sh
bash scripts/install.sh                # latest stable
channel=canary bash scripts/install.sh # latest prerelease of a channel
grove up                               # follow the box's channel
grove up --version 0.2.0               # pin a version
grove up --rollback                    # back to the previous one
```

An install is a directory and two symlinks, never an in-place overwrite:

```
$GROVE_HOME/
├── versions/<v>/bin/grove   # immutable once written; one binary per release
├── current  → versions/<v>  # the only thing that moves, by rename(2)
├── previous → versions/<v>  # the rollback target
├── channel                  # the release channel this box follows
└── pending                  # set while a flip's health gate is unanswered
```

`grove` on PATH is a symlink to `current/bin/grove`, and a running process keeps its
mapped binary, so a flip never disturbs the server that is up. `install.sh` is a thin
bootstrap — platform detect, resolve the release, download the bundle and its sha256
sidecar into a staging dir — and then hands off to `grove up`, which owns everything
that touches the layout. Release assets are public, so the installer needs no
credentials; export `GH_TOKEN`/`GITHUB_TOKEN` only to lift GitHub's anonymous rate
limit.

**A flip is not trusted until it is gated.** `grove up` fetches, verifies the
checksum, extracts beside `current`, flips, restarts the server, and then requires
`/api/health` to answer **as the exact version it flipped to** — not merely 200, which
a draining old server or a stale process on the port would also produce. A version
that fails that gate is rolled back automatically, and the daemon is brought back up
on the version that worked. Because the flip lands before the gate answers, the owed
verdict is written to disk first (`pending`): an update killed in between leaves
`current` on an unproven version, and the next `grove up` finds the marker, rolls back
to the proven one, and only then goes forward.

There is no update timer. `grove up` is the whole story — grove polls nothing.

Publishing: `scripts/release.sh` builds and packs this box's platform (and fails if
the version stamp didn't reach the binary — a mislabelled bundle breaks the health
gate on every box that pulls it); a `v*` tag runs `.github/workflows/release.yml` for
all three platforms, holding the release as a **draft** until every platform's bundle
is on it, so no channel can ever resolve a tag whose assets are still compiling;
`scripts/publish-canary.sh` pushes a this-box-only `-canary.N` prerelease with no CI
minutes.

## Working on grove

```sh
mise run setup   # first run in a fresh clone or worktree: installs the git hooks
mise run check   # format + lint, the pre-PR gate CI runs
mise run test    # the whole suite: nextest + doctests
mise run smoke   # install → update → rollback → uninstall, against a fixture release
```

`mise run setup` is not optional in a new checkout — the pre-commit and pre-push gates do
not exist until `lefthook install` has written them.

The pre-push gate also runs `stele check && stele emit --check` over the architecture
graph in `AGENTS.md` + `.stele/`, so `stele` has to be on PATH:

```sh
curl -fsSL https://raw.githubusercontent.com/jgeschwendt/stele/main/scripts/install.sh | bash -s v0.2.0
```

Editing code inside an anchored claim's region stales that claim — anywhere inside it, not
just near the anchor line. Re-read it, satisfy yourself it still holds, then `stele build`
and commit `.stele/graph.lock` with the change. Neither the hook nor CI ever runs `build` —
that would launder staleness into a pass.

Naming an `enforced_by:` target turns that staling *off* for the claim, so this repo names
one only where a distinct test target actually guards the claim. A claim proved by the
inline `mod tests` beside its own anchor names nothing on purpose: the digest is the only
thing watching a region whose tests live inside it.

## License

Copyright Joshua Geschwendt.

Licensed under the [PolyForm Noncommercial License
1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0) — the full text is in
[LICENSE.md](LICENSE.md). Any noncommercial purpose is permitted; commercial use
requires a separate license from the author.

External contributions are not accepted — the chain of title stays with one owner.
