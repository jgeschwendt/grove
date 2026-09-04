# Plan: split the install from the workspace

Status: active · branch `feat/install-home` · authored 2026-09-03

## Problem

`$GROVE_HOME` holds two things with opposite lifecycles under one root:

- **the install** — `versions/`, `current`, `previous`, `channel`, `pending`, `update.lock`. Disposable, regenerable, owned by `grove up`. Knows nothing about repos.
- **the workspace** — `manifest.toml`, its lock, `grove.{lock,pid,log}`, `code/`. The most valuable tree on the box. Knows nothing about releases.

Costs of the blend: `uninstall.sh` does `rm -rf $GROVE_HOME` and takes every checkout with the binary; a second home (`GROVE_HOME=~/.grove-dev`) must carry its own install because one knob names both; a backup or disk move has to reason file-by-file about which half is which.

## Target

```
$GROVE_INSTALL   default $XDG_DATA_HOME/grove → ~/.local/share/grove
├── versions/<v>/bin/grove
├── current  → versions/<v>          relative links — survive a move intact
├── previous → versions/<v>
├── channel · pending · update.lock
~/.local/bin/grove → $GROVE_INSTALL/current/bin/grove

$GROVE_HOME      default ~/.grove — the workspace and nothing else
├── manifest.toml · manifest.toml.lock
├── grove.lock · grove.pid · grove.log
└── code/<owner>/<repo>/{.git,.trunk,.pool,<worktree>…}
```

Two roots, two env vars, one resolution each. Not a nested `$GROVE_HOME/install/`: nesting keeps the one-knob problem.

## Stages

Each stage leaves `cargo test --workspace` green and `stele check && stele emit --check` passing. Alpha within a stage is incidental; order between stages is load-bearing.

### 1 · `grove-ops`: the second resolution

- `crates/grove-ops/src/lib.rs` — add `pub fn install_home() -> PathBuf` beside `home()`: `GROVE_INSTALL`, else `$XDG_DATA_HOME/grove`, else `$HOME/.local/share/grove`, else `./.grove-install`. Same doc-comment contract as `home()`: one resolution, every entry point calls it, a second spelling is a split brain.
- `crates/grove/src/lib.rs` — `grove_install_home()` thin wrapper beside `grove_home()`.
- Unit tests for each fallback rung, env-isolated.

### 2 · updater and launcher read the install root

- `crates/grove/src/update/mod.rs` — `from_env` builds `Layout::new(install_home)`; `read_persisted_channel` reads `$GROVE_INSTALL/channel`; the `update.lock` flock moves to `$GROVE_INSTALL/update.lock`. `layout.rs` is untouched — its `home` parameter is already just "the root the layout lives under".
- `crates/grove/src/server.rs` — `launcher()` takes the install root: `installed = install_home.join("current/bin/grove")`. The child env at the `cmd.env("GROVE_HOME", …)` site also sets `GROVE_INSTALL`, so a served daemon's own `grove up` flips the same install this CLI resolved. Mirror the existing `GROVE_HOME` pass-through test.
- `crates/grove-daemon` — `Config::from_env` carries the install root if the health envelope or `grove version` reports it; otherwise no change. Verify by grep for `current` under `crates/grove-daemon/src`.
- `docs/updates.md` — layout block names `$GROVE_INSTALL`; the "`grove` on PATH" paragraph points at the new root. `docs/deployment.md` — the `$GROVE_HOME` requirement row (line 51) and env row (line 66) split into a workspace row and an install row; `GROVE_INSTALL` joins the env table. The `self-update-flip` landmark claim is re-read and re-stamped with `stele build`.

### 3 · install and uninstall scripts

- `scripts/install.sh` — `grove_install="${GROVE_INSTALL:-${XDG_DATA_HOME:-$HOME/.local/share}/grove}"`; `grove up` is invoked with both envs; the PATH symlink targets `$grove_install/current/bin/grove`; the header comment documents both vars.
- `scripts/uninstall.sh` — always removes the install root and unlinks a PATH symlink that points into it. Removes `$GROVE_HOME` only behind `--purge`, printed as such. This closes the footgun where uninstall deleted every checkout.
- The `install-hands-off-to-grove-up` claim (a churn-leashed shell anchor) is re-read and re-stamped.

### 4 · `grove doctor` flags a stale layout

- `crates/grove-ops/src/doctor.rs` — one whole-home plumbing check: if `$GROVE_HOME/current` or `$GROVE_HOME/versions` exists, report `install-under-home` with the migration below as the fix line. Report-only, like every plumbing finding.
- No automatic move. Single-operator tool; the migration is four lines and the operator should see it happen.

### 5 · release and migrate

Ship as the next `v*` tag through the standard pipeline. Then on each box:

```sh
mkdir -p ~/.local/share/grove
mv ~/.grove/{versions,current,previous,channel,update.lock} ~/.local/share/grove/
ln -sf ~/.local/share/grove/current/bin/grove ~/.local/bin/grove
grove off && grove on
grove doctor
```

Order matters: the old binary is what runs `grove off`; it keeps its mapped image across the move, and the new `current` symlink is relative so it resolves after the `mv`. The `pending` marker, if present, moves with the set. `grove doctor` must come back clean.

## Out of scope

- The shape of `code/` (owner/repo nesting, `.trunk`/`.pool`, the `routines` root's bare-repo layout). Separate conversation.
- Automatic migration inside `grove up`.
- The profile README rows in `~` (`.grove/`, `.local/`) — updated by the operator after stage 5, not by this repo.
