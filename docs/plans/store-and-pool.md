# Plan: a root holds checkouts and nothing else

Status: active · branch `feat/store-and-pool` · authored 2026-09-16 · ships as v0.3.0 · plate: `docs/plates/store-and-pool.png` (`store-and-pool.py` regenerates it)

## Problem

A root today is `.bare`, `.pool`, and one visible checkout per branch. The two dotted entries are grove's, and every tool that lists a directory shows them: VS Code hides only `**/.git` by default, so `.bare` and `.pool` sit in the explorer beside `main/` and `blog/`. The checkouts directory is a container that carries grove's private state next to the operator's checkouts, and every rule about it has to say "except grove's own entries": the reserved-name rule, the occupied-root refusal, the trunk-recovery guard, legacy detection. And `root` means three things in the code — the manifest entry, the checkouts directory, and the thing on disk that owns the bare.

## Target

```
$GROVE_HOME/
├── code/<owner>/<repo>/            checkouts, and only checkouts
│   ├── main/                       the trunk, named by its branch
│   └── blog/                       a worktree
├── roots/<owner>/<repo>/           everything grove owns for that root
│   ├── bare/                       the bare repo
│   ├── pool/slot-N                 warm slots
│   └── cloning                     the in-flight marker, only while a clone runs
├── manifest.toml · grove.{lock,pid,log}
```

Rules that fall out, and are the design:

- **Vocabulary.** A *root* is a declared repo: `[roots."o/r"]` in the manifest, `roots/o/r/` on disk. Its *code dir* is `code/o/r/`. In grove-ops `root_dir(home, slug)` returns `roots/<slug>` and `code_dir(home, slug)` returns `code/<slug>`; every current caller of `root_dir` that meant the checkouts directory moves to `code_dir`. One word, one thing.
- **A code dir contains checkouts and only checkouts.** Every entry is a git worktree of the root's bare. "Grove's own entries" stops being a concept there: `is_owned` and the dot rule go away. The one tolerated non-checkout is `.DS_Store`, a Finder artefact, ignored by name.
- **One root, one directory.** `roots/<slug>/` holds the bare (`bare/`), the pool (`pool/slot-N`), and the clone marker (`cloning`). `clone remove` deletes `roots/<slug>` and `code/<slug>`; nothing of a root can be left behind anywhere else. The bare is a subdirectory, never the root directory itself.
- **Worktree pointers are absolute**, so moving the bare costs nothing to a checkout; `git worktree repair` re-points them when it moves. Promote is already a `git worktree move` into the code dir; a cross-tree move is the same call. A slot still sits outside the code tree, so `_.symlink` shares still materialize nothing in a slot and link correctly once promoted — the depth invariant survives at a different address.
- **Occupied means anything.** The clone arm refuses a code dir that exists and holds any entry (except `.DS_Store`). The trunk-recovery guard's "nothing but grove's own" becomes "nothing else".
- **Discovery reads `roots/`.** `adopt` walks `roots/<owner>/<repo>/bare` for undeclared bares, never the code tree. A code dir with checkouts and no root directory is a doctor finding (`orphan-code`), report-only.
- **Three generations of layout, one detector.** `layout::legacy(home, slug)` names what it finds: v1 (`.git` bare, `.trunk` in the code dir), v2 (`.bare`, `.pool` in the code dir), or a mix. Reconcile refuses all of them with the doctor remedy, as it does today. `doctor --fix` migrates any generation to v3 in one idempotent pass: create `roots/<slug>/`, move the bare into `bare/` (`git worktree repair` repairs every checkout's pointer), move each `.pool/slot-N` into `pool/` (`git worktree repair <moved paths>`), then the v1 steps as they exist today. A half-moved root (a `roots/<slug>/bare` *and* a bare in the code dir) is refused rather than guessed.
- **The wire names both directories.** The root view gains `root` (the `roots/<slug>` path) beside `trunk`; `grove status`/`tree list` show it only under `--verbose`. Nothing else on the wire changes.
- **A workspace file falls out of the manifest.** `grove workspace [--out <path>]` writes a `.code-workspace` whose folders are every checkout of every declared root, named `<owner>/<repo> · <branch>`, default `$GROVE_HOME/grove.code-workspace`. On demand only; `tree add`/`remove` print "workspace file is stale: grove workspace" when the default file exists. Severable.

## Stages

Every stage leaves `mise run check`, `NEXTEST_PROFILE=ci cargo nextest run --workspace` and `stele check && stele emit --check` green. Stage 1 is the core; 2a–2c are independent of each other; 3 and 4 follow.

### 1 · core: vocabulary and the two trees

- `crates/grove-ops/src/roots.rs` — `root_dir` → `home/roots/<slug>`; new `code_dir` → `home/code/<slug>`; `bare_dir` → `root_dir/bare`; the clone marker → `root_dir/cloning`; `foreign_entries(code_dir)` → every entry but `.DS_Store`; `is_owned` deleted; the recovery guard, the occupied refusal, `clone_and_trunk` (creates both directories), `remove` (deletes both) read the new paths. Every caller across the workspace that used `root_dir` for the checkouts directory now calls `code_dir` (grep `root_dir(` in every crate and decide per call site — most are code-dir uses).
- `crates/grove-ops/src/pool.rs` — `pool_dir(home, slug)` → `root_dir/pool`; `slots`, `free_slot`, `fill`, `reclaim`, `promote` address it; `worktrees::under_pool` follows.
- `crates/grove-ops/src/worktrees.rs` — `is_reserved` stays (git forbids dotted refs); `actual`/`adoptable_name` treat every undotted entry of the code dir as a checkout and skip only the trunk; `worktree_dir` → under `code_dir`.
- `crates/grove-ops/src/env.rs` — the trunk and worktrees are under `code_dir`; pool under `root_dir/pool`.
- `crates/grove-ops/src/testfix.rs` — fixtures lay the v3 shape; every test that spelled `.bare`, `.pool`, or `code/o/r` as the state location uses the helpers. Daemon, api and cli fixtures that hardcode `/code/o/r/.bare` follow.
- Tests: clone lands `roots/o/r/bare` and `code/o/r/main`; pool fills under `roots/o/r/pool`; promote moves a slot into the code dir; a code dir holding `notes.txt` is refused; a code dir holding only `.DS_Store` clones; `remove` leaves neither directory.

### 2a · discovery, layout, doctor

- `adopt` walks `roots/`. `layout.rs` — `Legacy` grows `V2Bare`, `V2Pool`; `legacy(home, slug)`; `Display` names every half found. `doctor.rs` — `legacy-layout` reports the generation; `migrate_root` handles v2 → v3 and v1 → v3 idempotently; `orphan-code` finding (report-only). `--fix` stays scoped as today.
- Tests: v2 fixture (`.bare` + `.pool/slot-0` + `main/` + a worktree with a share link) → finding names v2; `--fix` → `roots/o/r/{bare,pool/slot-0}`, pointers repaired (`git status` in every checkout), link intact, second run clean; v1 fixture → same end state; half-moved root refuses.

### 2b · daemon, wire, CLI

- `crates/grove-api` / `crates/grove-daemon` — `RootView.root` path; fixtures; `contracts/wire-vocab.json` re-blessed via `BLESS_WIRE=1`. The engine's disk-ready derivation reads `bare_dir` and `trunk_dir`, so it follows by construction; confirm the engine tests' fixtures follow.
- `crates/grove/src/commands.rs`, `render.rs` — `--verbose` on status/tree list prints the root path; `grove workspace [--out]` as specified, with its test (two roots, three checkouts → three folders with the expected names and paths); `tree add`/`remove` print the staleness line when the default file exists.

### 2c · docs

- `docs/worktrees.md`, `docs/worktree-environment.md`, `docs/architecture.md`, `docs/engine.md`, `docs/api.md`, `docs/deployment.md`, `README.md` — the layout diagram, the vocabulary (root, code dir), the depth-invariant paragraph (slot address changes, rule stands), recovery, adoption, `grove workspace`. Present tense.

### 3 · lock and claims

`worktree-depth`, `trunk-recovery-guard`, `never-clobber`, `create`, `promote-attach-before-move`, `files-authoritative` are re-read against the new addresses; claim texts that say "one level deeper", "grove's own entries", or "root directory" meaning the checkouts dir are reworded in `crates/grove-ops/AGENTS.md`; `stele build`; `stele emit`.

### 4 · ship and migrate

Tag `v0.3.0`. On each box: `GROVE_MODE=served grove up` — the daemon boots, every root reads as v2 legacy, reconcile refuses each with the doctor remedy and the view says so; `grove doctor --fix` (delegated to the daemon, on each root's lane) migrates; status re-derives to ready. No stop/start: that is what the v0.2.2 guard bought. Then `grove workspace` and the `~` README row for `.grove/`.

## Out of scope

- jgeschwendt/grove#2 (ghost reconcile), `docs/plans/ghost-reconcile.md`.
- Sharing one root between two homes.
- Any change to what `trunk` means or how shares link.
