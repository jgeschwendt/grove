# Plan: reconcile refuses a legacy or occupied root instead of cloning into it

Status: active · branch `fix/legacy-reconcile-guard` · authored 2026-09-05 · ships as v0.2.2

## Problem

`roots::realize` has three arms: bare and trunk present → present; bare present, trunk missing → guarded recovery; otherwise → `clone_and_trunk`. The third arm is the first-clone path and it checks nothing about the root directory: `create_dir_all` then `git clone --bare` into `.bare`.

On 2026-09-05 a v0.2.1 daemon booted over sixteen roots still in the pre-0.2 layout (bare at `.git`, checkout at `.trunk`). Each read as "bare missing" and was re-cloned from origin beside its legacy layout — a fresh `.bare`, a fresh `main/`, fresh pool slots — and every recreate of a declared worktree failed on "already exists", leaving branch refs at the trunk tip inside the fresh bares. Nothing legacy was destroyed, but the operator had to stop the daemon, remove sixteen fresh clones by hand, and only then run `grove doctor --fix`. The `legacy-layout` doctor finding existed and was correct; reconcile never consulted it.

The same arm would clone into any occupied root: a directory the operator laid out by hand, a root whose bare was deleted while its worktrees remained.

## Rules

- **A legacy root is refused, never re-cloned.** Before the clone arm, `realize` asks `layout::legacy(root_dir)`; a `Some` is a `Failed` outcome whose error names what was found and the remedy (`grove doctor --fix`). No file is created or removed on that path.
- **An occupied root is refused.** If the root directory exists and holds any entry that is not grove's own (`is_owned` is false — an undotted checkout, a stray file), the clone arm returns `Failed` naming the entries. A root holding only `.pool` or the clone marker is still cloned: those are grove's, and the fill pass prunes stale slots.
- **One definition of the legacy layout.** `LEGACY_BARE` / `LEGACY_TRUNK` and the detection predicate move out of `doctor.rs` into a small `layout` module in grove-ops that both doctor and roots read. Doctor's `legacy_check` and `migrate_root` keep their behaviour.
- **The daemon surfaces the refusal and clears it.** A `Failed` reconcile already maps to `Degraded`; the error text must reach the root view (`grove tree list` / status) so the operator reads "legacy layout — grove doctor --fix" rather than a bare "degraded". After a daemon-delegated `doctor --fix` migrates a root on its lane, the route nudges that root's engine to re-derive so it turns `Ready` without waiting for an unrelated event.
- **No automatic migration in reconcile.** Migration stays in `doctor --fix`, where the operator asked for it.

## Stages

### 1 · grove-ops: layout module and the guard

- `crates/grove-ops/src/layout.rs` (new, `pub mod layout` in lib.rs): `pub const LEGACY_BARE`, `pub const LEGACY_TRUNK`, `pub enum Legacy { Bare, Trunk, Both }`, `pub fn legacy(root: &Path) -> Option<Legacy>` (a `.git` directory that is a bare repo — has a `HEAD` file — or a `.trunk` directory). `doctor.rs` imports these instead of its private constants.
- `crates/grove-ops/src/roots.rs` `realize`: between the trunk-recovery arm and the clone arm, two refusals in this order — legacy layout, then occupied directory (`foreign_entries` non-empty). Both return `Applied { status: Failed, error: Some(..) }` and touch nothing. Error texts: `"root is in the legacy layout (<what>): run `grove doctor --fix` to migrate it in place"` and `"root directory holds <entries>; refusing to clone into it (remove them, or `grove clone remove` the root)"`.
- The `trunk-recovery-guard` landmark region gains the new rule; its claim text in `crates/grove-ops/AGENTS.md` is extended: "…and a root carrying the legacy layout, or any entry that is not grove's own, is refused outright — reconcile never clones into an occupied directory."
- Tests in roots.rs: a hand-built legacy root (bare at `.git` with HEAD, `.trunk` dir) → `reconcile_one` returns Failed with the legacy message, `.bare` absent afterwards, `.git`/`.trunk` untouched; a root dir holding a stray `notes.txt` and nothing else → Failed naming `notes.txt`; a root dir holding only `.pool/` → clones. Existing recovery tests still pass.

### 2a · grove-daemon: the refusal is readable and clears after the fix

- The root view / status carries the reconcile error text for a `Degraded` root (field exists or is added — `note`/`error`; follow the wire's existing shape and re-bless `contracts/wire-vocab.json` only through `BLESS_WIRE=1`). `crates/grove/src/commands.rs` tree list and status render it.
- `crates/grove-daemon/src/routes.rs` doctor route: when `fix` ran the legacy migration for a slug (report carries a migrated outcome), nudge that engine (the existing reconcile/derive request) so status re-derives from the now-ready disk.
- Engine test: declared root laid out legacy by hand → engine settles `Degraded` and the view's error text contains "legacy layout"; run `grove_ops::doctor::run(home, Some(slug), false, true)` then nudge → settles `Ready`, `.bare` and `main/` present.

### 2b · docs

- `docs/worktrees.md` recovery section and `docs/architecture.md` (reconcile adds, never deletes; the three arms become four) describe the refusals. `docs/api.md` if the root view gained a field. Present tense, no incident narration; the incident lives in this plan and git.

### 3 · ship

Gates green (`mise run check`, `NEXTEST_PROFILE=ci cargo nextest run --workspace`, `stele check && stele emit --check` after `stele build`), branch pushed, main fast-forwarded, tag `v0.2.2`, release published, then on this box `GROVE_MODE=served grove up` and `grove doctor`.

## Out of scope

- jgeschwendt/grove#2 (ghost reconcile after `roots::remove`; degraded never re-derives) — `docs/plans/ghost-reconcile.md`.
- Automatic migration inside reconcile.
