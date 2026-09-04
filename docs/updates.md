# Self-update and releases

Grove updates by **versioned directories plus an atomic symlink flip**. Version
directories are immutable once written; only `current` ever moves, and it moves by
`rename(2)`. There is no sentinel state machine, no in-place swap, and no orphan
trampoline — a failed download or extract leaves `current` untouched, so a broken release
cannot take a server down.

There is no update timer. `grove up` is the whole story; grove polls nothing.

## The layout

```
$GROVE_INSTALL/                  # default ~/.local/share/grove
├── versions/0.3.1/bin/grove     # immutable after write — one binary per release
├── versions/0.4.0/bin/grove
├── current  → versions/0.4.0    # the only mutation point
├── previous → versions/0.3.1    # the rollback target
├── channel                      # the release channel this box follows
├── pending                      # a flip whose health gate has not answered
└── update.lock                  # serializes concurrent `grove up`
```

**The install is not the workspace.** Everything above is release state: disposable,
regenerable, owned by `grove up`, and knowing nothing about repos. `$GROVE_HOME`
(default `~/.grove`) is the other root and holds the opposite — `manifest.toml` and its
lock, `grove.lock`, `grove.pid`, `grove.log`, and every checkout under `code/`. Two
lifecycles, two knobs: the install can be deleted and re-installed at will, the
workspace is the most valuable tree on the box. Both are resolved once, in `grove-ops`
(`install_home` and `home`), and the CLI, the launcher and the daemon all read those
resolutions rather than re-deriving either.

`grove` on PATH is a symlink to `$GROVE_INSTALL/current/bin/grove`, so the flip retargets
the whole install at once. A running process keeps its mapped binary, so a flip never
disturbs the daemon that is up — the new version takes effect at the bounce.

The bundle is one file in a `bin/` directory. `grove serve` *is* the daemon, so there is no
second artifact and no embedded runtime; the launcher contract is
`$GROVE_INSTALL/current/bin/grove serve`.

`crates/grove/src/update/layout.rs` is the pure on-disk mechanism — no network, fully unit
tested. `mod.rs` orchestrates a run, with both side effects injected as seams (where
bundles come from, how the server bounces), so the whole flow is testable against local
fixtures. A file lock serializes concurrent `grove up` invocations.

## `grove up`

```
recover any unproven flip
  → resolve the version (--version, else the channel)
  → fetch the bundle + its .sha256 sidecar
  → verify the checksum
  → install into versions/<v>/  (refusing a bundle with no bin/grove)
  → flip current  (writes the pending marker first)
  → bounce the server
  → version-aware health gate
  → clear pending; prune old versions, or roll back
```

Landmark: `stele:landmark self-update-flip` in `Updater::up`.

**A bundle that carries no `bin/grove` never becomes `current`.** `install_bundle` asserts
the binary is there after unpacking and clears the half-installed directory otherwise. The
health gate cannot cover this: it is skipped whenever no server is running — the state of
every first install, and of `install.sh`'s hand-off on a clean machine — so a mis-packed
release flipped `current` onto an empty tree, exited 0, and left the operator's PATH
symlink (`current/bin/grove`) dangling with no `grove` to roll back with.

**The gate is version-aware.** A 200 is not enough — a draining old server also answers,
and a stale process still bound to the port would answer as a different version. The gate
requires `GET /api/health` to return a *ready* 200 whose `version` is exactly the one just
flipped to, polled up to 30 s (`GROVE_HEALTH_GATE_SECS`; its floor is the 30 s `grove on`
ready timeout, because the bounce underneath it is a whole stop-and-start, and a gate
shorter than the start it waits on would roll back every healthy release that merely booted
slowly).

**A failed gate rolls back automatically**, and the outcome is distinguished: a rollback
whose restored version comes back healthy is exit 7 (`update`), while one whose restored
version *also* fails to come back is exit 6 (`unhealthy`) — the box is down, and that must
not read as a routine rollback. A restart that errors outright is treated as a bad boot and
rolls back too, rather than stranding `current`.

Pruning keeps the three newest version directories and is housekeeping only: a prune
failure never fails a landed, healthy update.

### The pending marker

The flip lands before the gate answers, so the owed verdict is written to disk first.
`Layout::flip_to` writes `pending` naming the version **and the direction** before `current`
moves; the updater clears it the moment the gate answers, whatever it answers. A marker
still naming `current` at the start of the next `grove up` is therefore proof that a
previous updater died mid-update.

Gating *before* the flip is not an option: in served mode the bounce hands off to a
supervisor that restarts whatever `current` points at, so a pre-flip gate cannot be
expressed at all.

The direction is not decoration. **Both** movers of `current` mark a flip pending — a
rollback owes a gate exactly as a forward flip does — and a rollback's marker names the
version that was just *proven*. `Updater::recover_unproven_flip` distinguishes three crash
windows:

| marker | what happened | recovery |
|---|---|---|
| names something other than `current` | the process died before the rename; nothing moved | drop the marker, carry on |
| names `current`, direction `forward` | the flip landed, the gate never answered — `current` is unproven | roll back to the proven version (or, on a first install with nowhere to fall back to, keep `current` and give it the gate it was owed) |
| names `current`, direction `rollback` | `current` is already the proven version | do **not** flip; re-run the owed gate |

Reading a rollback marker as a forward flip would undo the rollback, land back on the
release the gate rejected, then report "already on `<v>`" and exit 0 with no bounce at all.

Both landing arms re-run the bounce, because whatever is *running* may still be the version
the interrupted update was leaving behind. They pass `ensure_running = false`: a marker on
disk says nothing about whether this box runs a daemon at all, and force-starting one an
operator deliberately stopped is not recovery.

The idempotence shortcut ("grove is already on `<v>`") is suppressed after a recovery that
moved `current`, for the same reason: exiting 0 there would launder an interrupted update
into a no-op whose reported version is the one nothing has proven.

`grove up --rollback` deliberately does **not** run the recovery first — after an
interrupted update that command *is* the recovery, and rolling back twice would land on the
unproven version it exists to escape. It flips to `previous`, bounces, and maps an unhealthy
or errored restart to exit 6.

## The bounce

Whether the server is restarted at all is decided by three facts: is there a pid file, was
`ensure_running` requested, and does anything answer `/api/health`. Only a fully stopped
server — no pid, not force-starting, nothing answering — short-circuits to `NotRunning` and
skips the gate. A pid-less server that still answers is managed externally, so it is gated
and auto-rolled-back like any other.

`GROVE_MODE=served` changes the restart to a plain stop: the substrate's supervisor brings
the process back on whatever `current` points at. Otherwise the bounce is `grove off` then
`grove on`.

`ServerControl::from_env()` is resolved *before* anything is downloaded, so a bad
`GROVE_BIND` fails as the config error it is instead of being misread from inside the bounce
as "the new version failed to start".

## Channels and sources

The channel a `grove up` follows resolves as `--channel` → `GROVE_CHANNEL` →
`$GROVE_INSTALL/channel` → `stable`. Only `install.sh` writes that file, and it derives the
value from the *resolved version's* prerelease suffix rather than from its own `channel`
input: a pinned `install.sh v0.1.1-canary.2` runs with `channel=stable` but is really a
canary box, and keying off the input would strand it on stable. `grove up --channel` is
per-run and persists nothing.

Against GitHub Releases, `stable` is the `releases/latest`
redirect (which excludes prereleases) and any other channel is the highest
`v<base>-<channel>.<N>` prerelease, walked page by page (100 per page, 20 pages max) —
releases come newest-first, so the highest of a channel lives on the first page that
carries the channel at all.

`GROVE_INSTALL_BASE_URL` overrides the source:

- an `https://` base → a flat `<base>/<version>/<target>.tar.gz` (+ `.sha256`) layout with
  `<base>/latest` naming the current version;
- a plain `http://` base → **refused unless it targets loopback** (`127.0.0.1`,
  `localhost`, `::1`), so a MITM cannot redirect a self-update over cleartext. Any userinfo
  in the authority is refused outright — `http://127.0.0.1@evil.com/x` targets evil.com
  while a naive host parse reads 127.0.0.1, and the loopback test seam never needs
  credentials. **The same rule is re-applied to every redirect hop**, not only to the base
  string: redirects follow (GitHub's `releases/latest` and its asset download both need
  them), so a check on the base's spelling alone constrained the URL rather than where the
  bytes came from — a loopback base could 302 the fetch to any host over cleartext, and an
  `https` base could be downgraded to `http` mid-chain. `install.sh` applies the rule too,
  because it runs before any grove binary exists to apply it;
- anything else → a local fixture directory, which is what the install smoke and
  `install.sh`'s staging use.

Downloads are capped: 256 MiB for a bundle, 8 MiB for a text body (the sidecar, the
`latest` file). The cap is read as `cap + 1` bytes so "exactly at the limit" is
distinguishable from "over", and an oversized body is refused rather than buffered.

`host_target` is `<arch>-<os>`, matching `scripts/release.sh` and `scripts/install.sh`; an
unsupported host is refused up front.

## Installing

```sh
bash scripts/install.sh                 # latest stable
channel=canary bash scripts/install.sh  # latest prerelease of a channel
bash scripts/install.sh v0.1.0          # a pinned tag
```

`install.sh` is deliberately thin: platform detect, resolve the release on
`jgeschwendt/grove`, download the bundle, its checksum sidecar, a bootstrap `grove` **and
the bootstrap's own sidecar** into a staging directory, then hand off to `grove up` with
that directory as `GROVE_INSTALL_BASE_URL`. The bootstrap's checksum is verified before it
is made executable: it is the one asset the script *runs*, and it used to be the one asset
with no sidecar at all — the tarball's checksum was verified by that unverified binary. Everything that touches the layout is unit-tested Rust; the
GitHub fetch is the only logic that lives in bash, and it stages into the same local-dir
source the smoke test and `grove up` already understand.

The releases are public, so the installer needs no credentials: it resolves and downloads
anonymously. `GH_TOKEN`/`GITHUB_TOKEN`, when exported, is used only to lift GitHub's
anonymous rate limit — the script then resolves and fetches through the REST API's asset
endpoint instead of the plain `releases/download/…` URL, which is the same bundle by
another route.

`GROVE_LINK_DIR` names where the PATH symlink goes; unset, the script searches
`/usr/local/bin` then `~/.local/bin`. `scripts/uninstall.sh` stops the server first —
through the installed binary, since the roots about to be deleted hold that binary and,
in the workspace, the pid file and the lock the daemon runs under — then removes the
symlink and the install root. `$GROVE_HOME` survives unless `--purge` asks for it: the
install is regenerable by one `grove up`, the checkouts under `$GROVE_HOME` are not.

## Publishing

| path | what it does |
|---|---|
| `scripts/release.sh` | builds and packs **this box's** platform into `dist/<version>/`: `<target>.tar.gz`, its `.sha256`, the standalone bootstrap `grove-<target>` and *its* `.sha256`, with `dist/latest` naming the version. Fails if the version stamp did not reach the binary — a mislabelled bundle breaks the health gate on every box that pulls it. |
| `.github/workflows/release.yml` | on a `v*` tag: open a **draft** release, build the bundle for every supported platform, upload, and publish only once the whole matrix has landed. |
| `scripts/publish-canary.sh` | builds the current tree for this box only and uploads it as a `-canary.N` prerelease directly with `gh` — zero CI minutes, and `release.yml` never fires. Host-only: `grove up --channel canary` on another OS or arch will 404. |

The draft is the point. A published bare-semver tag becomes `releases/latest` the instant it
exists, while the bundles are still compiling under `lto = true` / `codegen-units = 1` — and
for that whole window both stable resolvers (the Rust one and `install.sh`) would land on
the new tag and then 404 on its assets. A draft is invisible to `releases/latest` and to the
asset listing, uploads to it work normally, and a partial matrix simply never promotes.

A bare semver tag (`v0.1.0`) publishes as the latest stable release; a suffixed tag
(`v0.1.0-canary.1`) publishes as a prerelease, which is what channel resolution walks.

Shell completions and a man page are not in the bundle: clap can generate both, but nothing
in the shipped binary's dependency graph produces them yet.

## Command surface and exit codes

```sh
grove up                       # follow the box's channel
grove up --version 0.2.0       # pin a version
grove up --channel canary      # follow a channel for this run
grove up --rollback            # back to the previous version
```

`--rollback` ignores `--version`/`--channel` by construction: it flips to whatever
`previous` names, which is the one version a rollback can mean.

Exit codes are a contract:

| code | meaning |
|---|---|
| 1 | general failure or an API error envelope (with `error.data`'s detail appended) |
| 3 | not found — an undeclared root or worktree, from the in-process realizer |
| 4 | the server is unreachable or busy |
| 5 | conflict — doctor's unresolved conflicts, or a guarded remove refusing over unsaved work (in-process; through the server the same refusal is exit 1 with `remove_failed` and its reason) |
| 6 | unhealthy — including a rollback whose restored version did not come back |
| 7 | self-update failed (and rolled back cleanly) |

Code 2 is skipped: it is clap's own exit for an unparseable command line.

## Tests

`crates/grove/src/update/layout.rs` and `source.rs` carry the unit suites (flip, rollback,
pending marker, pruning, version ordering, channel resolution, the `http://` and userinfo
refusals, the size caps). `crates/grove/tests/update_e2e.rs` drives the round trip against a
real daemon: `slow_the_update_round_trip_gates_and_rolls_back_against_a_real_daemon` and
`slow_an_interrupted_flip_is_undone_by_the_next_up`.

`mise run smoke` (`test/install_smoke.sh`) drives `scripts/install.sh` and
`scripts/uninstall.sh` as an operator would, hermetically, against a fixture release in a
temp directory — its own `GROVE_HOME` and `GROVE_INSTALL`, its own `GROVE_BIND`, no
network. It gets its own mise task and its own CI job because no cargo target can reach a
shell script, and `harness_meta` pins both ends of that wiring:
`the_install_smoke_is_reachable_from_a_gate` and
`the_install_smoke_links_only_inside_its_own_home`.
