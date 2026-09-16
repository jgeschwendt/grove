# Roots and worktrees

A **root** is a declared repository: `[roots."owner/name"]` in the manifest, and
`roots/owner/name/` on disk — the directory holding everything grove owns for it. Its
**code dir** is `code/owner/name/`, which holds checkouts and only checkouts. A
**worktree** is a branch of that root checked out in the code dir, a direct sibling of
the trunk. Roots and worktrees are *declared* in `manifest.toml` and *realized* in git,
and convergence between the two is two-way and additive.

## The manifest

`$GROVE_HOME/manifest.toml` is desired state, and the only durable thing grove writes
besides git itself. `grove_ops::manifest` is its sole owner: it edits format-preservingly
through `toml_edit` under an exclusive advisory `flock` on a sibling `manifest.toml.lock`
— never on the manifest itself, which every write replaces by rename, so a lock on its own
inode would be released by the very write it guards — and so grove's writes never clobber a
human's comments.

```toml
# my repos                              # comments survive every grove write

[roots."owner/name"]
url   = "git@github.com:owner/name.git"
trunk = "canary"                        # integrates here; absent → the remote's HEAD

[roots."owner/name".pool]
size = 2                                # warm slots to keep ready; default 0

[roots."owner/name".env]
_.symlink = [".env"]                    # shared live into every worktree
_.copy = ["config/local.json"]          # seeded once per worktree

[roots."owner/name".worktrees.feature-x]
branch = "feature/x"
base = "canary"                         # absent for worktrees adopted from git
```

`trunk` names the branch grove integrates on: the one its checkout is named after, the one
`sync` fast-forwards, and the one every share reads through. Absent — the common case — it
is the remote's `HEAD`, whatever the clone landed on.

`base` is where a *new* branch forks from, and `grove tree add` without `--base` resolves
it to the trunk branch and records it — named explicitly rather than left to git, whose own
default start point is the bare's `HEAD`. The two agree in the settled state and differ in
exactly the window that matters: between a `trunk` edit and the reconcile that applies it.
A root not yet on disk resolves to nothing and the base stays unset, for the realizer to
fill.

`_.symlink`/`_.copy` are TOML *dotted keys* nesting as `env` → `_` → `symlink`, not
literal `"_.symlink"` keys — mise's `_` directive-namespace convention. `_.setup`, a
per-worktree setup command, is reserved and not implemented.

Structure and spacing are canonicalized on every write and on every hand-edit the watcher
sees: roots as standalone `[roots."slug"]` tables sorted by slug, each root's sub-tables
regrouped, worktrees sorted by name, one blank line between tables. Diffs stay stable and
merges stay clean. Canonicalization is write-if-changed, so it does not loop on its own
file-change event, and a top-level table that is not under `[roots]` is left untouched.

### Validation

Four validators guard the boundary between a declaration and a filesystem path or a git
argument. All four are applied at the declaring API *and* when reading a hand-edited or
git-synced manifest, so neither entrance is the only gate.

| validator | rule |
|---|---|
| `validate_slug` | non-empty relative path, `Normal` components only — no `..`, absolute, `.`, or prefix parts; no backslash; no ASCII control characters |
| `validate_name` | exactly one `Normal` segment (so no `/`), the slug rules otherwise, and never a dotted name — no checkout can be spelled that way |
| `validate_share_path` | one or more safe segments (nesting allowed), canonical — no `.`, empty (`//`) or trailing-`/` segments — no segment beginning with `-`, and no dotted first segment once the path nests (`.env` is a share; a path *under* a dotted entry is not) |
| `validate_ref_arg` | a `branch`, `base` or `trunk` is non-empty and does not begin with `-`, so a crafted declaration cannot smuggle a git flag |

**The dot rule.** Git refuses a ref component beginning with a dot, so no branch — and
therefore no directory `worktrees::name_for` derives from one — is ever spelled that way.
A dotted entry in a code dir is somebody else's artefact: grove neither declares nor
adopts it, and `worktrees::is_reserved` is that one-line predicate, not a list. Rejecting
a dotted *declaration* follows — it would ask reconcile to create a directory no adoption
could ever read back as a checkout.

The share-path gate is the *string* half of a two-gate posture; the filesystem half is the
`O_NOFOLLOW` descent in `docs/worktree-environment.md`.

## On-disk layout

Two trees, one entry per root in each: the code dir carries the checkouts, the root
directory carries everything grove owns.

```
$GROVE_HOME/
├── manifest.toml
├── manifest.toml.lock                     # serializes manifest read-modify-write
├── grove.lock · grove.pid · grove.log     # daemon custody
├── code/
│   └── owner/name/                        # the code dir — checkouts, and only checkouts
│       ├── canary/                        # the trunk: the checkout of the trunk branch
│       └── feature-x/                     # a worktree — a direct sibling of the trunk
└── roots/
    └── owner/name/                        # the root — everything grove owns for it
        ├── bare/                          # the bare clone
        ├── pool/slot-0/                   # warm slots, detached at the trunk tip
        └── cloning                        # the marker, only while a clone is in flight
```

The install layout — `versions/`, `current`, `previous`, `channel`, `pending` and
`update.lock` — is a separate tree under `$GROVE_INSTALL` (`docs/deployment.md`). The
split is what makes the two disposable in opposite directions: the install can be deleted
and re-installed with no repository noticing, and nothing under `$GROVE_HOME` is
regenerable from a release.

`roots::root_dir` and `roots::code_dir` are the published definition of the two joins,
`roots::bare_dir` of the bare, `pool::pool_dir` of the pool and `roots::trunk` of the
trunk; nothing else in the tree recomputes them.

**A code dir holds checkouts and only checkouts.** Every entry is a git worktree of the
root's bare, which is what lets an editor, a `find`, or an operator's eye read the
directory without an "except grove's own" clause. The one tolerated stranger is
`.DS_Store` — a Finder artefact that appears in any directory a Mac has looked at, and the
only name `roots::foreign_entries` skips.

**One root, one directory.** The bare, the warm pool and the in-flight-clone marker all
live under `roots/<slug>/`, so nothing of a root is anywhere but there and its code dir —
which is what lets a removal delete a root completely by deleting two paths. The bare is
the subdirectory `bare/`, never `roots/<slug>` itself: a bare repository at the root
directory would make that directory look like a repository to every tool that walks
upward, so `git status` under it errors and an editor opened there sees no working tree.

**Checkout depth is load-bearing.** Every checkout — the trunk included — is a *direct*
child of the code dir, and shares materialize only at that canonical depth.
`worktrees::name_for` folds a branch name's `/` to `-` (`feature/x` → `feature-x`) for
exactly this reason, and it is the one branch-to-directory rule in grove: `grove tree add`,
the trunk and share targets all read it. `worktrees::adoptable_name` refuses anything more
than one segment below the code dir. A warm-pool slot sits outside the code tree
altogether, so it is not at canonical depth and materializes nothing while it is a slot —
a relative `../<trunk>/<p>` from `roots/<slug>/pool/slot-N/` resolves inside the pool, not
against the trunk. Promote is what puts it right, and it is one `git worktree move` into
the code dir: worktree pointers are absolute, so crossing between the two trees costs a
checkout nothing, and the links materialize correctly the moment it lands.

A root on an earlier layout generation is doctor's `legacy_layout` finding, and
`grove doctor --fix` migrates it in place. Realization refuses such a root rather than
cloning beside it (§ Root realization). `grove_ops::layout` is where the superseded names
are spelled and the generations named, and nothing else in grove reads them; see
`docs/api.md` § Doctor.

## The trunk

The trunk is the checkout of the branch a root integrates on. It is not a special
directory: it is named by `worktrees::name_for(branch)` exactly as every other checkout is
(`canary` → `canary/`, `release/2` → `release-2/`), and what tells it apart from its
siblings is that the manifest points at it.

`roots::trunk(home, slug) -> Trunk { branch, name, dir }` is the one resolution, and
nothing else spells the trunk's path. Desired state is read first — the root's `trunk` key
when it declares one — else the bare's `HEAD`, which reconcile sets from that same
declaration. So a root adopted off disk has a trunk too, whatever it was cloned onto, and a
freshly-declared `trunk` resolves here before any convergence has run.
`git::default_branch` is what reads `HEAD` back, and every consumer already went through
it. `trunk_reads_the_declaration_and_falls_back_to_the_bares_head` pins both halves.

`roots::trunk_dir` is the same lookup for a caller with nowhere to put a failure — a
presence probe, a doctor row. A root whose bare cannot be read falls back to `<code>/main`,
so the probe reports "missing" rather than blanking; anything able to report an error calls
`trunk` instead.

### Changing the trunk is a manifest edit

Edit `trunk`, reconcile, and `converge_trunk` brings the disk onto it in three moves, each
a no-op once it has run:

- a `worktrees.<name>` entry already on the trunk branch is **dropped** — that checkout
  *is* the trunk now, and leaving it declared would put the root's integration checkout
  where a `tree remove` deletes it;
- the trunk's checkout is **created** when nothing is there, an ordinary `git worktree add`
  from the bare;
- `HEAD` is **moved** onto the trunk branch, so actual state answers the declaration.

**No trunk is ever deleted by a trunk change.** The previous trunk stays checked out where
it is, and the worktree reconcile that follows adopts it into `worktrees.<name>` — an
ordinary worktree from then on.
`a_trunk_change_creates_the_new_checkout_and_adopts_the_old_one` and
`a_declared_worktree_on_the_new_trunk_branch_becomes_the_trunk` pin the two directions.

The branch is proved to exist before anything is written, so a typo'd `trunk` leaves the
root exactly as it was rather than minting the typo as a branch. A *declared* worktree
already occupying the new trunk's directory on a different branch is two checkouts asking
for one path, which no order of operations resolves: reconcile refuses, naming both, rather
than dropping somebody's declaration or leaving the trunk on a branch it does not name.

## Reconcile is two-way and additive

`worktrees::reconcile(home, slug)` converges declared against actual and **never deletes**:

- **declared, not in git** → `git worktree add` at `<code>/<name>` on the declared branch
  (from `base` when the declaration records one). A recreate git refuses is reported as
  `failed` with git's reason, not silently dropped — the operator sees a wedged worktree
  instead of a silent retry loop.
- **in git, not declared** → written into the manifest as `adopted`, with no `base`
  (the fork point is unknown).
- **both** → nothing.

The trunk is never one of the three: `worktrees::adoptable_name` excludes the root's own
checkout by name, so it is neither adopted into `worktrees` nor recreated as one. The
checkout a trunk change left behind is not the trunk any more, so it *is* adopted, like any
other undeclared branch-carrying checkout.

Git-level ghosts are pruned first. A worktree directory removed out of band leaves a
`prunable` registration that makes git report it as still present (masking the
declared-but-missing case, so the recreate never fires) and refuses a re-`add` at that path.
Pruning clears both, and deletes nothing real — the working tree is already gone.

Adoption needs a branch to record, so a **detached** checkout is never adopted. A *declared*
detached worktree is left entirely alone: present, so no recreate; branchless, so no
adoption. `worktrees::actual` deliberately retains detached checkouts with `branch: None`,
because presence is independent of the branch.

`worktrees::list` is the declared ⋈ actual join a UI draws: every declared worktree with
whether git has it, plus every undeclared branch-carrying one, each with its own
`git status` drift. `branch` is what the manifest declares; `status.branch` is what is
actually checked out, and the two disagreeing is drift a dashboard flags and doctor reports
as a `worktree`/`mismatch` check. A `git status` that fails is swallowed to `None` — one
wedged checkout must not blank the whole list.

## Root realization

`roots::reconcile_one(home, slug)` realizes one declared root: converge the trunk, clone
the declared-but-missing bare, check the trunk branch out at `<code>/<trunk name>`,
reconcile that root's worktrees, then materialize its shares. It is idempotent — a
`present` root re-reconciles to a no-op.

Converging the trunk comes first because *which* directory is the trunk is a manifest
question, and a trunk change left unapplied would make the rest of the pass see a root
whose trunk checkout is missing — the trunk recovery below — rather than one whose trunk
simply moved.

`Err` is reserved for not-declared and lookup faults. A clone or guard failure is an
`Ok(Applied { status: Failed })`, so one wedged root never aborts a sweep;
`grove_ops::apply` (the offline `grove apply`) relies on that to report a failed root
beside its healthy neighbours.

A root that is not already present is decided by reading *both* of its directories, not
the manifest: a bare that is not at `roots/<slug>/bare` is not the same as nothing being
there. Four answers, cheapest and least destructive first — only the last two write
anything.

1. **Refuse a root on an earlier layout generation.** Its bare sits inside the code dir,
   so every probe the clone arm makes reads *missing* against it — cloning would lay a
   whole second root down beside the first, both left to be untangled by hand. The outcome
   is `failed` with "run `grove doctor --fix` to migrate it in place", and nothing under
   the root is created or removed: migration stays in doctor, where an operator asked for
   it. `grove_ops::layout` is the one spelling of the superseded names, read by doctor's
   `legacy_layout` finding and by the realizer, so the two cannot disagree about what a
   legacy root is (`docs/api.md` § Doctor names the generations).
2. **Refuse an occupied code dir.** A code dir holds checkouts and only checkouts, so
   anything already in it is somebody's and a clone would interleave grove's layout with
   theirs: the outcome is `failed` naming the entries. Occupied means *anything* but
   `.DS_Store` — there is no "except grove's own" clause left to carve out, because grove
   owns nothing here. The legacy check comes first, and that order is load-bearing: a
   legacy root's own entries would trip the occupancy check too, and "holds `.git`" is the
   less useful of the two answers.
3. **Re-add the trunk** when the bare is there and its checkout is gone — it vanished out
   of band (a stray `rm`, a `git worktree prune`); grove never produces this state itself.
   Prune, then `git worktree add` from the bare that is already there: a checkout, not a
   repository, is what went missing, so this is the whole of the real repair and it touches
   nothing else under the root. It is **refused** when the bare carries other live
   worktrees — a user's checkouts, possibly with uncommitted work — as `failed` with
   "recreate it, or `grove clone remove` the root". That check fails safe: if
   `git worktree list` errors at all, grove assumes worktrees may exist and refuses. Two
   exclusions keep the count honest, and they need different tests: the trunk being
   recovered, matched by the basename that names it in the code dir, and the warm-pool
   slots, matched by path — a slot is registered under `roots/<slug>/pool/`, where its
   `slot-N` basename names nothing in the code dir.
4. **Clone**, which is a root's first realization and, when the bare cannot produce a
   worktree at all, its re-clone. A re-clone deletes both of the root's directories first,
   so it happens **only if the code dir holds nothing but `.DS_Store`**, and names what it
   would have destroyed otherwise. This is the one path where grove would `rm -rf` a
   directory a human also writes into, unattended, on a reconcile nobody asked for — which
   is why the root directory, grove's alone, is the only one it deletes without asking.

A refusal is an `Applied` carrying its reason, so it reads the same on both realizers:
`grove apply` prints `failed  <slug>: <reason>` beside the roots that converged, and the
daemon degrades that root, where it waits for an operator rather than a retry clock.
`grove doctor --fix` is what clears a legacy root — it migrates the layout in place, and
the engine re-derives the root as `ready` from disk on the next event.

A clone in flight writes a `cloning` marker at `roots/<slug>/cloning` for the length of
the network fetch: `gix` materializes the bare with its `origin` before a byte of the
transfer lands, and adoption must not read that as a discovery — otherwise an operator's
undeclare is silently undone and the root re-cloned on the next manifest event. It sits
beside the bare rather than in the code dir, which holds checkouts and nothing else; a
crashed clone leaves it behind on purpose, so the leftover is visibly grove's.

Worktree reconcile and share materialization are best-effort inside `reconcile_one` — a
share hiccup must not fail the root — but a worktree the reconcile *cannot* recreate is
surfaced on stderr, which is grove-ops' operator channel (the CLI prints it, the daemon's
log pipeline picks it up).

## Adoption

Adoption is discovery, and it **never clones**.

`roots::adopt(home)` walks `$GROVE_HOME/roots/<org>/<repo>` — the tree grove owns, never
the code tree — and declares any slug whose `bare/` has an `origin` remote and is not
already declared. It:

- skips dotfile directories (a `.Trash` under `roots/` is not an org);
- skips a name that is not UTF-8, reporting it as `skipped` with a lossy rendering for the
  operator's eyes only — a U+FFFD-laced slug would not round-trip to the real directory,
  so it is never minted;
- skips a repo with no `origin` (no URL to record);
- skips a clone still in flight, which the `cloning` marker names;
- **never overwrites a declared URL**;
- is best-effort per repo — one failure does not abort the rest;
- canonicalizes the manifest at the end, so a pure hand-edit reorder is normalized even
  when the pass declared nothing.

The watcher runs `adopt` on every manifest save and at boot, then publishes
`roots_changed`. The engines do the realizing.

A code dir that holds something with no root directory behind it is the mirror case, and
it is **not** adoptable — there is no bare to read a URL from, so there is nothing to
declare. Doctor reports it as `orphan_code`, report-only (`docs/api.md` § Doctor).

## Removal: declared first, then delete on disk, then undeclare

`roots::remove` refuses a slug that is **not declared** before it builds any path it would
hand to `remove_dir_all`. `<home>/code/<slug>` is a real directory whether or not grove put
it there, so a valid-shaped typo used to obliterate somebody else's checkout and report
success — the same typo the daemon route answers with a 404. Undeclared is `not_declared`
(exit 3) on both paths.

It then surveys the root for **unsaved work** — uncommitted tracked changes, or commits no
remote has, in any worktree under it (warm-pool slots excluded; untracked files are not the
signal, since grove materializes shares as untracked entries) — and refuses with a
`conflict` naming them unless the caller passed `--force` (`Removal::Forced`, `force: true`
on the route). `grove tree remove` already refuses one dirty checkout because git does; the
command that deletes N of them at once must not be the one that protects least.

Past both guards it deletes **both** of the root's directories — `roots/<slug>` and
`code/<slug>` — and *then* removes the manifest entry. Both, because nothing of a root may
survive either tree: taking only one would leave half a root for `adopt` or `doctor` to
find. Each is deleted only if it is there, since a declared-but-unrealized root has
neither. `remove_undeclares_and_deletes` pins both halves gone.

The order is the whole point: undeclaring first opens a window in which the watcher's
`adopt` — which runs on the manifest save — re-finds the still-present bare and re-declares
the slug, and the engine then re-clones the root the user just deleted. Deleting first
closes it; adopt cannot resurrect a bare that is gone.

A partial `remove_dir_all` failure propagates *before* the undeclare, so the slug stays
declared and doctor/reconcile report a broken root rather than adopt bringing it back.

`worktrees::remove` refuses an undeclared name for the same reason, then inverts the order:
**git first, then undeclare**. Removing the manifest entry first would let the next reconcile
re-adopt the still-registered worktree. When the directory is already gone out of band,
grove prunes the stale git registration so the removal is git-visible before the undeclare.

Removing a root through the API pokes the watcher afterwards: a removed root's engine is
torn down by the reconcile pass that follows, never by an explicit stop call. Removing a
worktree sends no nudge — a worktree has no engine.

## Sync

`roots::sync(home, slug)` requires a ready root and does four things, as one lane call:
fetch the trunk branch (bounded, single refspec), fast-forward the trunk checkout onto the
tracking ref, prune warm-pool slots stranded at a pre-sync tip, and report.

The branch is the trunk's own, not the bare's `HEAD`. Reconcile converges the two, but a
sync in the window between a `trunk` edit and the reconcile that applies it must fetch and
fast-forward the branch the manifest names, never the one being left behind
(`sync_fetches_and_fast_forwards_the_declared_trunk`).

**It never forces.** A trunk carrying local commits or dirty tracked files is a
*reported* outcome, not a reset: `SyncReport::trunk` is `updated`, `already_current`,
`diverged`, or `dirty`, and the daemon turns the latter two into the snapshot's `sync_note`.
Syncing a root with nothing on disk is `not_ready` — a transient code, so the engine retries
on the next event rather than degrading.

Slot pruning compares each slot to the trunk's tip *after* the op, so it recycles exactly
the slots a fresh `pool.fill` would no longer produce, whichever way the trunk outcome went.
The engine's fill loop restores the target at the new tip.

## The warm pool

Warm slots are pre-checked-out worktrees under `roots/<slug>/pool/` that realizing a
declared worktree claims near-instantly, skipping a cold checkout. They sit in the tree
grove owns, not in the code dir, so an operator's view of their checkouts never carries
them. The declared target is `[roots."<slug>".pool] size` (default 0 — opt-in); the
observed count is `worktrees::pool_count`, which counts registered worktrees whose path
sits *strictly* under `pool::pool_dir` — strictly, because a worktree registered at the
pool directory itself would otherwise count as a slot and, being the lowest path, be the
one `promote` picked up and moved.

**Who claims a slot:** the realizer, not a caller. `worktrees::create` (the offline
`grove tree add`) and `worktrees::reconcile` (what the engine runs after a
declare-and-nudge, and over a hand-edited manifest) both try `pool::promote` first and
cold-create on `Cold`. That is deliberate placement: a pool claimed from only one entry
point is a pool that is filled, paid for on every clone, reported as `pool n/n` readiness,
and redeemed by nothing.

`pool::fill` adds **one** detached slot at the trunk tip — resolved through `roots::trunk`,
so a root that has declared a new trunk seeds onto that branch and not the one it is
leaving — and nothing else. N calls converge to N slots. `pool::reclaim` is its mirror — it
removes the highest-indexed slot, non-forced — so a target that *drops* converges too
rather than leaving the checkouts on disk forever with nothing able to reclaim them. The
engine's fill task picks whichever direction the observed count needs. It prunes git-level
ghosts first, for a specific failure: a slot directory removed out of band leaves a
`prunable` registration, `free_slot` picks the now-empty path, and the detached add then
fails with "missing but already registered" — wedging every future fill. It errors when the
bare is missing, because the engine only fills a `ready` root, so a missing bare is a real
fault rather than a steady state.

Slots get **no shares**. A slot sits outside the code tree, so `env::materialize`'s
depth invariant does not hold there — a relative link would resolve inside the pool — and
the promote move would invalidate the links regardless. `worktrees::actual`'s path-based
exclusion keeps slots out of the declared set entirely, regardless of branch or
detachment: a slot path does not strip under the code dir, so no basename of it is ever
read as a checkout.

`pool::promote(home, slug, name, branch, base)`:

1. attaches the branch **in the slot** (DWIM, like `worktree_add`), from `base` when the
   caller named one and the trunk branch otherwise — named explicitly, so a promote is not
   silently forked from whatever commit `fill` seeded the slot at;
2. claims it into the code dir with `git worktree move` — not a bare rename, which would
   strip git's gitdir pointers. The move crosses from `roots/<slug>/pool/` to
   `code/<slug>/`, and costs nothing extra for it: worktree pointers are absolute, so a
   cross-tree move is the same call a within-tree one would be;
3. declares it in the manifest;
4. materializes the root's shares, now that it is at canonical depth.

**Attach before move** is what makes promote convergent under interruption. Any worktree
that ever reaches `<code>/<name>` already carries a branch, so a moved-but-undeclared one
is adopted by `worktrees::reconcile` — there is no window in which a *detached* orphan is
stranded at the user path, which reconcile could neither adopt nor recreate. An attach
failure (the branch is checked out elsewhere) leaves the slot cleanly detached and reusable.

A promote never clobbers an existing `<code>/<name>`: that is `Promotion::Cold(Conflict)`.
An empty pool is `Promotion::Cold(Empty)`. Neither is an error — both tell the realizer to
cold-create, which is what makes the warm and cold paths interchangeable.

## Serialization

Every per-root git writer runs on that root's lane in the daemon — reconcile (promote
included), sync, fill, remove, worktree ops, and doctor's converge. `grove-ops` does not enforce this: its
`flock` serializes the manifest, deliberately not git, and two concurrent `git worktree add`s
on one root race an index lock. **The lane is the caller's to supply**; a concurrent caller
that does not serialize per root loses an invariant this crate was written against. See
`docs/architecture.md` § Lanes.

## Where this is exercised

`crates/grove-ops/src/roots.rs`, `worktrees.rs` and `pool.rs` carry the unit suites —
including `remove_then_adopt_declares_nothing`, `remove_keeps_the_root_declared_on_a_partial_delete`,
`reconcile_one_refuses_to_wipe_live_worktrees_when_trunk_is_missing`,
`reconcile_one_refuses_a_root_in_the_legacy_layout`,
`reconcile_one_refuses_to_clone_into_an_occupied_root`,
`reconcile_one_clones_into_a_code_dir_holding_only_a_ds_store`,
`reconcile_one_recovers_a_missing_trunk_past_the_roots_warm_pool`,
`remove_undeclares_and_deletes`,
`a_trunk_change_creates_the_new_checkout_and_adopts_the_old_one`,
`create_without_a_base_forks_the_new_branch_from_the_trunk`,
`a_pool_slot_is_excluded_from_list_and_reconcile_even_with_a_branch`,
`promote_attach_failure_strands_no_orphan_and_keeps_the_slot`,
`promote_never_clobbers_an_existing_worktree` and
`sync_reports_a_diverged_trunk_and_keeps_its_slots`. The end-to-end path — declare, realize,
promote, sync, remove — is `crates/grove-daemon/tests/scenario.rs`.
