# Plan: the trunk is a branch, named on disk by that branch

Status: active · branch `feat/trunk-by-branch` · authored 2026-09-04

## Problem

A root today is `<owner>/<repo>/{.git, .trunk, .pool, <worktree>…}`. Two frictions:

- `.trunk` is a hidden directory with a role name, beside visible worktrees with branch names. Two naming schemes; the checkout used most is the one Finder, `ls`, tab completion and the editor explorer hide.
- The bare lives in `.git`, so every tool that walks upward finds a false repo root: `git status` in the root errors as a bare repo, an editor opened there sees no working tree.

And one gap: the trunk is always the remote's default branch. Some repos integrate on another branch (`canary` while GitHub's default stays `main`), and grove has no way to say so.

## Target

```
~/.grove/code/<owner>/<repo>/
├── .bare/          the bare repo — was .git
├── .pool/          warm slots, unchanged
├── canary/         the trunk: the checkout of the trunk branch, named by it
├── main/           an ordinary worktree, if one is kept
└── blog/           user worktrees, unchanged
```

```toml
[roots."o/r"]
url   = "git@github.com:o/r.git"
trunk = "canary"     # absent → the remote's HEAD, today's behaviour
```

Rules that fall out, and are the design:

- **Naming.** Every checkout directory is `name_for(branch)`: the branch with `/` as `-`, the rule `grove tree add` already applies. The trunk is not a special directory; it is the worktree the manifest points at.
- **Reserved set.** Git refuses ref components starting with `.`, so grove-owned entries are exactly the dotted ones (`.bare`, `.pool`, the in-flight-clone marker) and checkouts are exactly the undotted ones. `RESERVED`/`OWNED` become "starts with a dot", not a list.
- **Desired vs actual.** The manifest's `trunk` is desired state. The bare's `HEAD` symbolic-ref is actual state: reconcile sets it (`git symbolic-ref HEAD refs/heads/<trunk>`), and `git::default_branch(bare)` — which already reads that ref — keeps being the one reader every consumer uses. An adopted, undeclared root therefore has a trunk too: whatever its bare HEAD names.
- **One resolution.** `roots::trunk(home, slug) -> Trunk { branch, name, dir }`: manifest `trunk` if declared, else the bare HEAD; `dir = root_dir/name`. `trunk_dir(home, slug)` becomes this lookup. Nothing else spells the trunk path.
- **Changing the trunk is a manifest edit.** Reconcile with `trunk = canary` and a `main/` trunk on disk: ensure a `canary/` checkout exists (create it as a worktree of `canary` if absent; if the manifest already declared a worktree on that branch, that checkout *is* the new trunk and its `worktrees.<name>` entry is dropped), set bare HEAD, repoint share links, and leave `main/` on disk as an ordinary worktree, adopted into `worktrees.main`. No trunk is ever deleted by a trunk change.
- **Shares.** A `_.symlink` share links `../<trunk name>/<p>`, same depth as before, so the pool-promote and sibling-depth invariants hold unchanged. A grove-shaped link is one whose first non-`..` segment is the current trunk name *or* the legacy `.trunk`; the latter is repointed.
- **Sync.** Fetches the trunk branch's refspec and fast-forwards the trunk checkout. Never forces, as today.
- **`tree add --base`** defaults to the trunk branch.
- **Legacy layout is a doctor finding with a fix.** A root with `.git` (bare) or `.trunk` present reports `legacy-layout`. `grove doctor --fix` migrates it in place; see stage 4. Nothing else in grove reads the legacy names.

## Stages

Every stage leaves `mise run check`, `cargo nextest run --workspace` and `stele check && stele emit --check` green. Stage 1 is the core; stages 2a–2c are independent of each other and rebuild against it; 3 and 4 follow.

### 1 · core: manifest field, resolution, reserved rule, `.bare`

- `crates/grove-ops/src/manifest.rs` — `Root` gains `trunk: Option<String>`, TOML key `trunk`, validated as a branch ref (`validate_ref_arg` or its sibling). Round-trips through read-modify-write untouched when absent.
- `crates/grove-ops/src/roots.rs` — `bare_dir` → `.bare`. New `pub struct Trunk { branch, name, dir }` and `pub fn trunk(home, slug) -> Result<Trunk>` (manifest, else `git::default_branch(&bare)`). `trunk_dir` delegates to it. `OWNED` → a predicate `is_owned(name)`: starts with `.` or is the clone marker. Bare-HEAD write helper in `git.rs`: `set_head(bare, branch)`.
- `crates/grove-ops/src/worktrees.rs` — `RESERVED` → `is_reserved(name)`: starts with `.`. `pub fn name_for(branch) -> String` is the one branch-to-directory rule; if the CLI already owns that rule, move it here and call it from there. Manifest validation for worktree names and share paths uses the predicate.
- `crates/grove-ops/src/testfix.rs` — fixtures follow: `.bare`, and the trunk directory is `name_for(<fixture default branch>)`, i.e. `main`.
- Every test in grove-ops that spelled `.git` (bare) or `.trunk` is rewritten to the helpers, never to new literals.

### 2a · shares and pool

- `crates/grove-ops/src/env.rs` — the source directory is `trunk(home, slug)?.dir`; link targets are `../…/<trunk name>/<p>`; the grove-shaped test accepts the current trunk name or `.trunk` and repoints both.
- `crates/grove-ops/src/pool.rs` — promote and slot seeding read the trunk through `roots::trunk`; the depth invariant comment names `<trunk>` rather than `.trunk`.

### 2b · reconcile, sync, tree add

- `crates/grove-ops/src/roots.rs` — `reconcile_one`: clone lays `.bare` + `<trunk name>/`; the re-add-missing-trunk path recreates `<trunk name>/`; the destructive-adjacent guard counts undotted entries as worktrees. New step: converge bare HEAD and the on-disk trunk onto the manifest's `trunk` per the rule above. `sync`: trunk refspec + trunk dir.
- `crates/grove-ops/src/worktrees.rs` — `create` default base = trunk branch; `reconcile` never treats the trunk directory as a worktree, and adopts an ex-trunk left behind by a trunk change.
- `crates/grove/src/commands.rs` — `tree add` derives the name via `worktrees::name_for`; the offline status probe (`.git` && `.trunk` → ready) reads `.bare` and the trunk dir.

### 2c · wire, API, render

- `crates/grove-ops/src/wire.rs`, `crates/grove-api`, `crates/grove-daemon` — the root view carries `trunk` (path, already there) and gains `trunk_branch`. Fixtures that spell `/.trunk` become `/main`. `contracts/wire-vocab.json` re-blessed only through `BLESS_WIRE=1`, never by hand.
- `crates/grove/src/render.rs` — `tree list` marks the trunk row.

### 3 · doctor: `legacy-layout` and `--fix`

- `crates/grove-ops/src/doctor.rs` — per-root check `legacy-layout`: `.git` present as a bare, or `.trunk` present. Report-only unless `--fix`. With `--fix`, migrate in place, in this order, each step idempotent so a crash resumes: rename `.git` → `.bare` and rewrite every worktree's `.git` pointer file (`gitdir: <root>/.bare/worktrees/<n>`); rename `.trunk` → `<trunk name>` and rewrite `.bare/worktrees/-trunk/gitdir` to the new path; set bare HEAD; run the share materialize pass, which repoints `../.trunk/<p>` links. Verify with `git -C <trunk dir> status` before reporting `fixed`. `--fix` stays scoped to shares plus this one migration; the other plumbing findings remain report-only.
- The `.git` half must also handle the case where `.bare` already exists (a half-done migration) by refusing with a clear finding rather than guessing.

### 4 · docs and lock

- `docs/worktrees.md`, `docs/worktree-environment.md`, `docs/architecture.md`, `docs/engine.md`, `README.md` — describe the system that now exists: `.bare`, the trunk named by branch, the `trunk` manifest key, the dot rule, the `legacy-layout` fix. No migration history in prose.
- Claims anchored in edited regions are re-read; `stele build` re-stamps; `stele emit` refreshes projections. Both `check` and `emit --check` green.

## Rollout on this box

After the release: `grove doctor` lists every root as `legacy-layout`; `grove doctor --fix` migrates them. Then set `trunk = "canary"` on the roots that integrate there and reconcile. The two `.repair-backup` leftovers and the third `grove` checkout are unrelated cleanup.

## Out of scope

- The install/workspace split (`feat/install-home`, its own branch). Both touch `doctor.rs` and the docs; whichever lands second rebases.
- Shortening `~/.grove/code` itself.
- Automatic migration outside `doctor --fix`.
