# Roots and worktrees

A **root** is a declared repository. A **worktree** is a branch of that root checked out
as a sibling directory. Both are *declared* in `manifest.toml` and *realized* in git, and
convergence between the two is two-way and additive.

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
url = "git@github.com:owner/name.git"

[roots."owner/name".pool]
size = 2                                # warm slots to keep ready; default 0

[roots."owner/name".env]
_.symlink = [".env"]                    # shared live into every worktree
_.copy = ["config/local.json"]          # seeded once per worktree

[roots."owner/name".worktrees.feature-x]
branch = "feature/x"
base = "main"                           # absent for worktrees adopted from git
```

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
| `validate_name` | exactly one `Normal` segment (so no `/`), the slug rules otherwise, and never one of the reserved dirs |
| `validate_share_path` | one or more safe segments (nesting allowed), canonical — no `.`, empty (`//`) or trailing-`/` segments — no segment beginning with `-`, and a first segment that is not reserved |
| `validate_ref_arg` | a `branch`/`base` is non-empty and does not begin with `-`, so a crafted declaration cannot smuggle a git flag |

The reserved set is `.git`, `.trunk`, `.pool` (`worktrees::RESERVED`). Rejecting reserved
names is load-bearing rather than tidy: a worktree declared at `<root>/.pool` sorts ahead
of `<root>/.pool/slot-0`, so `promote` would `git worktree move` the entire warm pool into
the user's new worktree — pool state destroyed, two registered worktrees nested inside a
third. `a_reserved_pool_name_can_never_claim_the_warm_pool` pins it.

The share-path gate is the *string* half of a two-gate posture; the filesystem half is the
`O_NOFOLLOW` descent in `docs/worktree-environment.md`.

## On-disk layout

```
$GROVE_HOME/
├── manifest.toml
├── manifest.toml.lock                     # serializes manifest read-modify-write
├── grove.lock · grove.pid · grove.log     # daemon custody
├── update.lock                            # serializes concurrent `grove up`
├── channel · pending · current · previous · versions/    # the install layout
└── code/
    └── owner/name/
        ├── .git/           # the bare clone
        ├── .trunk/         # the default-branch checkout, and every share's source
        ├── .pool/slot-0/   # warm slots, detached at the default-branch tip
        └── feature-x/      # a user worktree — a direct sibling of .trunk
```

`roots::root_dir`, `bare_dir` and `trunk_dir` are the published definition of those three
joins; nothing else in the tree recomputes them.

**Worktree depth is load-bearing.** Every worktree is a *direct* sibling of `.trunk`, one
level under the root, and shares materialize only at that canonical depth. `grove tree add`
folds a branch name's `/` to `-` (`feature/x` → `feature-x`) for exactly this reason, and
`worktrees::adoptable_name` refuses anything more than one segment below the root. Pool
slots sit one level deeper on purpose and get no shares while they are slots — the relative
link `../.trunk/<p>` would dangle from `.pool/slot-N/`, and the promote move would
invalidate it anyway.

## Reconcile is two-way and additive

`worktrees::reconcile(home, slug)` converges declared against actual and **never deletes**:

- **declared, not in git** → `git worktree add` at `<root>/<name>` on the declared branch
  (from `base` when given). A recreate git refuses is reported as `failed` with git's
  reason, not silently dropped — the operator sees a wedged worktree instead of a silent
  retry loop.
- **in git, not declared** → written into the manifest as `adopted`, with no `base`
  (the fork point is unknown).
- **both** → nothing.

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

`roots::reconcile_one(home, slug)` realizes one declared root: clone the
declared-but-missing bare, check out `.trunk` at the default branch, reconcile that root's
worktrees, then materialize its shares. It is idempotent — a `present` root re-reconciles
to a no-op.

`Err` is reserved for not-declared and lookup faults. A clone or guard failure is an
`Ok(Applied { status: Failed })`, so one wedged root never aborts a sweep;
`grove_ops::apply` (the offline `grove apply`) relies on that to report a failed root
beside its healthy neighbours.

One path is destructive-adjacent and worth stating plainly: the bare exists but `.trunk`
is gone (it vanished out of band — a stray `rm`, a `git worktree prune`; grove never
produces this state itself). Three answers, cheapest and least destructive first:

1. **Refuse** if the bare carries other live worktrees — a user's checkouts, possibly with
   uncommitted work. Reconcile adds and never deletes. The outcome is `failed` with
   "recreate `.trunk`, or `grove clone remove` the root", and the check fails safe: if
   `git worktree list` errors at all, grove assumes worktrees may exist and refuses.
2. **Re-add `.trunk`** from the bare that is already there (prune, then
   `git worktree add`). This is the whole of the real repair — a checkout, not a
   repository, is what went missing — and it touches nothing else under the root.
3. Only when the bare cannot produce a worktree at all is a re-clone the answer, and a
   re-clone means deleting the root directory. That happens **only if the directory holds
   nothing but grove's own entries** (`.git`, `.trunk`, `.pool`, the in-flight-clone
   marker); anything else — a human's notes, a vendored tree, an unrelated clone — makes
   it a `failed` outcome naming what it would have destroyed. This is the one path where
   grove would `rm -rf` a directory a human also writes into, unattended, on a reconcile
   nobody asked for.

A clone in flight writes a `.grove-cloning` marker in the root for the length of the
network fetch: `gix` materializes `<root>/.git` with its `origin` before a byte of the
transfer lands, and adoption must not read that as a discovery — otherwise an operator's
undeclare is silently undone and the root re-cloned on the next manifest event.

Worktree reconcile and share materialization are best-effort inside `reconcile_one` — a
share hiccup must not fail the root — but a worktree the reconcile *cannot* recreate is
surfaced on stderr, which is grove-ops' operator channel (the CLI prints it, the daemon's
log pipeline picks it up).

## Adoption

Adoption is discovery, and it **never clones**.

`roots::adopt(home)` walks `$GROVE_HOME/code/<org>/<repo>` and declares any directory that
holds a `.git` bare with an `origin` remote and is not already declared. It:

- skips dotfile directories (a `.Trash` under `code/` is not an org);
- skips a name that is not UTF-8, reporting it as `skipped` with a lossy rendering for the
  operator's eyes only — a U+FFFD-laced slug would not round-trip to the real directory,
  so it is never minted;
- skips a repo with no `origin` (no URL to record);
- **never overwrites a declared URL**;
- is best-effort per repo — one failure does not abort the rest;
- canonicalizes the manifest at the end, so a pure hand-edit reorder is normalized even
  when the pass declared nothing.

The watcher runs `adopt` on every manifest save and at boot, then publishes
`roots_changed`. The engines do the realizing.

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

Past both guards it deletes `<root>/` and *then* removes the manifest entry. The order is the
whole point: undeclaring first opens a window in which the watcher's `adopt` — which runs
on the manifest save — re-finds the still-present bare and re-declares the slug, and the
engine then re-clones the root the user just deleted. Deleting first closes it; adopt
cannot resurrect a bare that is gone.

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
fetch the default branch (bounded, single refspec), fast-forward `.trunk` onto the tracking
ref, prune warm-pool slots stranded at a pre-sync tip, and report.

**It never forces.** A `.trunk` carrying local commits or dirty tracked files is a
*reported* outcome, not a reset: `SyncReport::trunk` is `updated`, `already_current`,
`diverged`, or `dirty`, and the daemon turns the latter two into the snapshot's `sync_note`.
Syncing a root with nothing on disk is `not_ready` — a transient code, so the engine retries
on the next event rather than degrading.

Slot pruning compares each slot to the trunk's tip *after* the op, so it recycles exactly
the slots a fresh `pool.fill` would no longer produce, whichever way the trunk outcome went.
The engine's fill loop restores the target at the new tip.

## The warm pool

Warm slots are pre-checked-out worktrees under `<root>/.pool/` that realizing a declared
worktree claims near-instantly, skipping a cold checkout. The declared target is
`[roots."<slug>".pool] size` (default 0 — opt-in); the observed count is
`worktrees::pool_count`, which counts registered worktrees whose path sits under `.pool`.

**Who claims a slot:** the realizer, not a caller. `worktrees::create` (the offline
`grove tree add`) and `worktrees::reconcile` (what the engine runs after a
declare-and-nudge, and over a hand-edited manifest) both try `pool::promote` first and
cold-create on `Cold`. That is deliberate placement: a pool claimed from only one entry
point is a pool that is filled, paid for on every clone, reported as `pool n/n` readiness,
and redeemed by nothing.

`pool::fill` adds **one** detached slot at the default-branch tip, and nothing else. N
calls converge to N slots. `pool::reclaim` is its mirror — it removes the highest-indexed
slot, non-forced — so a target that *drops* converges too rather than leaving the
checkouts on disk forever with nothing able to reclaim them. The engine's fill task picks
whichever direction the observed count needs. It prunes git-level ghosts first, for a specific failure: a slot
directory removed out of band leaves a `prunable` registration, `free_slot` picks the now-
empty path, and the detached add then fails with "missing but already registered" — wedging
every future fill. It errors when the bare is missing, because the engine only fills a
`ready` root, so a missing bare is a real fault rather than a steady state.

Slots get **no shares**. They sit one level deeper than a canonical worktree, so
`env::materialize`'s depth invariant does not hold there, and the promote move would
invalidate the links regardless. `worktrees::actual`'s path-based exclusion keeps slots out
of the declared set entirely, regardless of branch or detachment.

`pool::promote(home, slug, name, branch, base)`:

1. attaches the branch **in the slot** (DWIM, like `worktree_add`);
2. claims it with `git worktree move` — not a bare rename, which would strip git's gitdir
   pointers;
3. declares it in the manifest;
4. materializes the root's shares, now that it is at canonical depth.

**Attach before move** is what makes promote convergent under interruption. Any worktree
that ever reaches `<root>/<name>` already carries a branch, so a moved-but-undeclared one
is adopted by `worktrees::reconcile` — there is no window in which a *detached* orphan is
stranded at the user path, which reconcile could neither adopt nor recreate. An attach
failure (the branch is checked out elsewhere) leaves the slot cleanly detached and reusable.

A promote never clobbers an existing `<root>/<name>`: that is `Promotion::Cold(Conflict)`.
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
`a_pool_slot_is_excluded_from_list_and_reconcile_even_with_a_branch`,
`promote_attach_failure_strands_no_orphan_and_keeps_the_slot`,
`promote_never_clobbers_an_existing_worktree` and
`sync_reports_a_diverged_trunk_and_keeps_its_slots`. The end-to-end path — declare, realize,
promote, sync, remove — is `crates/grove-daemon/tests/scenario.rs`.
