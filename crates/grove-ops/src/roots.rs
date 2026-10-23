//! Root lifecycle: declare a repo in the manifest, then realize it on disk.
//! Manifest is desired state, git on disk is actual state, and reconcile drives
//! actual → desired (clone what's declared-but-missing). Two reconciler halves:
//! `adopt` (global discovery — declare undeclared bares, no clone) and
//! `reconcile_one` (per-root realize — clone + worktree reconcile + materialize
//! shares). The Watcher calls `adopt` and the per-root engines call `reconcile_one`;
//! `add` declares a root then realizes just that one via `reconcile_one`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::wire::{AdoptStatus, ReconcileStatus};
use crate::{Error, git, manifest};

#[must_use]
pub fn manifest_path(home: &Path) -> PathBuf {
    home.join("manifest.toml")
}

/// Everything grove owns for a root — `roots/<slug>`: the bare (`bare/`), the warm
/// pool (`pool/`), and the in-flight-clone marker. Nothing a human writes belongs
/// here, and nothing of a root lives anywhere else but here and its [`code_dir`].
///
/// Published rather than crate-private: the daemon's engine derives a root's status
/// by asking whether the bare and the trunk are both there, and v1 duplicated exactly
/// these joins on the far side of its process boundary (`paths.ex`) — a second copy of
/// the layout that a rename would have split silently. One process now, so one
/// definition.
#[must_use]
pub fn root_dir(home: &Path, slug: &str) -> PathBuf {
    home.join("roots").join(slug)
}

/// A root's checkouts, and only checkouts — `code/<slug>`. Every entry is a git
/// worktree of the root's bare, which is what lets an editor, a `find`, or an
/// operator's eye read the directory without an "except grove's own" clause. The one
/// tolerated stranger is `.DS_Store`; see [`foreign_entries`].
#[must_use]
pub fn code_dir(home: &Path, slug: &str) -> PathBuf {
    home.join("code").join(slug)
}

/// The root's bare clone — `<root>/bare`.
///
/// A subdirectory, never the root directory itself: a bare repo at `roots/<slug>`
/// would make that directory look like a repository to every tool that walks upward,
/// so `git status` under it errors and an editor opened there sees no working tree.
#[must_use]
pub fn bare_dir(home: &Path, slug: &str) -> PathBuf {
    root_dir(home, slug).join("bare")
}

/// The root's trunk: the branch grove integrates on, the directory that branch
/// checks out under, and the absolute path of that checkout.
///
/// One value rather than three lookups, because the three must never disagree — the
/// directory name is [`crate::worktrees::name_for`] of the branch, and every consumer
/// that opens the trunk, links a share through it or fast-forwards it needs both
/// halves.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Trunk {
    pub branch: String,
    pub name: String,
    pub dir: PathBuf,
}

/// Resolve a root's trunk: the manifest's `trunk` when the root declares one, else
/// the bare's `HEAD` — which reconcile sets from that same declaration, so an
/// adopted or undeclared root has a trunk too (whatever it was cloned onto).
///
/// The manifest is desired state and `HEAD` is actual state; reading desired first
/// means a freshly-declared `trunk` resolves here before any convergence has run.
/// This is the one place the trunk is resolved — nothing else spells its path.
pub fn trunk(home: &Path, slug: &str) -> Result<Trunk> {
    let declared = manifest::list(&manifest_path(home))
        .ok()
        .and_then(|roots| roots.into_iter().find(|r| r.slug == slug))
        .and_then(|r| r.trunk);
    let branch = match declared {
        Some(branch) => branch,
        None => git::default_branch(&bare_dir(home, slug))?,
    };
    let name = crate::worktrees::name_for(&branch);
    Ok(Trunk {
        dir: code_dir(home, slug).join(&name),
        branch,
        name,
    })
}

/// The trunk checkout's path, best-effort — for callers that only want a path and
/// have nowhere to put a failure (a presence probe, a doctor row). A root whose bare
/// cannot be read falls back to `<code>/main`, the overwhelmingly common trunk name,
/// so the probe reports "missing" rather than blanking.
///
/// **Anything that can report an error should call [`trunk`] instead** — it is the
/// same lookup without the guess.
#[must_use]
pub fn trunk_dir(home: &Path, slug: &str) -> PathBuf {
    trunk(home, slug).map_or_else(|_| code_dir(home, slug).join("main"), |t| t.dir)
}

/// Per-root outcome of a `reconcile_one` or an `adopt`. The two halves report
/// different vocabularies over the same shape, so the status is the type parameter:
/// `Applied` (the default) is a reconcile outcome, `Applied<AdoptStatus>` a discovery
/// one. That is what lets the daemon's engine classify a reconcile result with a
/// *total* match — `adopted`/`skipped` cannot reach it to need a catch-all arm.
#[derive(Debug, PartialEq, Serialize)]
pub struct Applied<S = ReconcileStatus> {
    pub slug: String,
    pub status: S,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Declared roots (desired state).
pub fn list(home: &Path) -> Result<Vec<manifest::Root>, Error> {
    manifest::list(&manifest_path(home)).map_err(Error::io)
}

/// The declared root `slug` names, looked up before any caller builds a path from it:
/// an invalid or non-canonical slug is `InvalidInput`, a valid one no root carries is
/// `NotDeclared`.
pub fn declared(home: &Path, slug: &str) -> Result<manifest::Root, Error> {
    manifest::validate_slug(slug).map_err(Error::invalid_input)?;
    list(home)?
        .into_iter()
        .find(|r| r.slug == slug)
        .ok_or_else(|| Error::NotDeclared(format!("root not declared: {slug}")))
}

/// The git status of a root's trunk checkout — what the dashboard draws next to
/// the repo name.
///
/// `None` rather than an error when the trunk isn't on disk: a root that is
/// declared-but-unrealized (or mid-clone) is an ordinary, transient state the
/// dashboard already renders via the engine's own `cloning`/`missing` status, so
/// failing the whole read for it would blank the worktree list too.
pub fn trunk_status(home: &Path, slug: &str) -> Result<Option<git::Status>, Error> {
    let trunk = trunk_dir(home, slug);
    if !trunk.is_dir() {
        return Ok(None);
    }
    git::status(&trunk).map(Some).map_err(Error::git)
}

/// Declare a root (slug → url), then realize **just that root** (clone the one new
/// bare via `reconcile_one`) — not a global reconcile. Returns a single-entry
/// `Vec<Applied>` for the declared slug.
pub fn add(home: &Path, slug: &str, url: &str) -> Result<Vec<Applied>, Error> {
    manifest::add_root(&manifest_path(home), slug, url).map_err(Error::io)?;
    Ok(vec![reconcile_one(home, slug)?])
}

/// Whether a [`remove`] surveys the root for unsaved work before deleting it.
///
/// The default is [`Removal::Guarded`]. `grove tree remove` inherits git's own
/// refusal on a dirty checkout, and a root remove — whose blast radius is *every*
/// worktree under it — must not be the one destructive command that protects less
/// than the smaller one beside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Removal {
    /// Refuse with a [`Error::Conflict`] naming every worktree that holds
    /// uncommitted tracked changes or unpushed commits, every unpushed branch, a
    /// stash, and any code-dir entry that is not a worktree of the root.
    Guarded,
    /// Delete regardless — the operator said `--force`, which is the gesture that
    /// distinguishes "I know what is in there" from "I didn't".
    Forced,
}

/// Delete a root's on-disk repository **then** undeclare it — delete first, so the
/// removal sticks (D4).
///
/// Undeclaring first opens a race: the Watcher's `adopt` runs on the manifest save,
/// re-finds the still-present bare, and re-declares the slug — the engine then
/// re-clones a root the user just deleted. Deleting the directory before the manifest
/// entry closes that window (adopt can't resurrect a bare that's gone). And a partial
/// `remove_dir_all` failure propagates *before* `remove_root`, so the slug stays
/// declared: doctor/reconcile report a broken root rather than adopt bringing it back.
///
/// **Both trees, and nothing of the root survives either.** `roots/<slug>` carries the
/// bare and the pool, `code/<slug>` the checkouts; a remove that took only one would
/// leave a root half on disk for `adopt` or `doctor` to find. Either being absent is
/// ordinary (a declared-but-unrealized root has neither), so each is deleted only if
/// it is there.
///
/// **Declared first, deleted second.** The slug is looked up in the manifest before
/// any path is handed to `remove_dir_all`: `<home>/code/<slug>` is a real directory
/// whether or not grove put it there, and an offline `grove clone remove` used to
/// obliterate a valid-shaped slug that named somebody else's checkout — the same
/// typo the daemon-up path answers with a 404. Undeclared is [`Error::NotDeclared`]
/// (exit 3), on both paths.
///
/// **Never another root's tree.** A hand-edited or git-synced manifest can declare
/// slugs that nest (`o/r` and `o/r/bare`), which `add_root` refuses; then one root's
/// directories sit inside the other's, and its delete is the other's loss. Any declared
/// root nesting with `slug` is an [`Error::Conflict`] in both modes, since `--force`
/// discards the operator's own unsaved work, never a neighbour.
// ※ delete-before-undeclare
pub fn remove(home: &Path, slug: &str, removal: Removal) -> Result<(), Error> {
    // Validate before building a path we `remove_dir_all` — a traversal slug
    // must never reach the filesystem here.
    declared(home, slug)?;
    if let Some(why) = nesting(home, slug)? {
        return Err(Error::Conflict(why));
    }
    if removal == Removal::Guarded {
        let unsaved = unsaved_work(home, slug);
        if !unsaved.is_empty() {
            return Err(Error::Conflict(format!(
                "{slug} has unsaved work in {}; commit and push it, or re-run with \
                 `--force` to delete it anyway",
                unsaved.join(", ")
            )));
        }
    }
    for dir in [code_dir(home, slug), root_dir(home, slug)] {
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("remove {}", dir.display()))
                .map_err(Error::io)?;
        }
    }
    manifest::remove_root(&manifest_path(home), slug).map_err(Error::io)
}

/// Why `slug` must not touch disk, when another declared root nests with it
/// ([`manifest::nests`]): one's directories sit inside the other's.
fn nesting(home: &Path, slug: &str) -> Result<Option<String>, Error> {
    Ok(list(home)?
        .into_iter()
        .find(|r| r.slug != slug && manifest::nests(&r.slug, slug))
        .map(|other| {
            format!(
                "{slug} nests with declared root {}, so one's directories sit inside the \
                 other's; undeclare one by editing the manifest",
                other.slug
            )
        }))
}

/// The worktrees under `slug` that hold work a `remove_dir_all` would destroy:
/// uncommitted **tracked** changes, or commits no remote has.
///
/// Untracked files are deliberately not the signal — grove materializes declared
/// shares as untracked entries inside every worktree, so counting them would refuse
/// every remove on a root that declares a share (the same reasoning as
/// [`git::fast_forward`]'s gate). Warm-pool slots are excluded: a slot is grove's own
/// detached checkout at the trunk tip, carrying nothing a human wrote.
///
/// The bare is surveyed too, since deleting it destroys what no checkout shows: a
/// `branch` whose worktree was removed while its commits stayed unpushed (a bare keeps
/// no reflog), and `refs/stash`, which every worktree shares. So is the code dir: an
/// entry that is not a registered worktree of this bare is a human's — the same
/// [`foreign_entries`] answer the realizer refuses to clone over — and is named
/// `(not grove's)`. Unreadable registrations make every entry foreign: fail safe.
///
/// Best-effort by construction: a worktree git cannot read contributes nothing
/// rather than blocking the remove — the guard exists to catch the ordinary "I
/// forgot what was in there", not to be a lock.
fn unsaved_work(home: &Path, slug: &str) -> Vec<String> {
    let bare = bare_dir(home, slug);
    let name = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_owned()
    };
    let registered = git::worktree_list(&bare).unwrap_or_default();
    let live: Vec<_> = registered
        .iter()
        .filter(|wt| !crate::worktrees::under_pool(home, slug, &wt.path) && wt.path.is_dir())
        .collect();
    let mut unsaved: Vec<String> = live
        .iter()
        .filter(|wt| {
            let dirty =
                git::status(&wt.path).is_ok_and(|s| s.staged + s.unstaged + s.conflicted > 0);
            dirty || git::unpushed_count(&wt.path).is_ok_and(|n| n > 0)
        })
        .map(|wt| name(&wt.path))
        .collect();
    // A branch a live worktree has checked out was answered above, by the tree's name.
    let checked_out = |b: &str| live.iter().any(|wt| wt.branch.as_deref() == Some(b));
    unsaved.extend(
        git::unpushed_branches(&bare)
            .unwrap_or_default()
            .into_iter()
            .filter(|b| !checked_out(b))
            .map(|b| format!("branch {b}")),
    );
    if git::rev_parse(&bare, "refs/stash").is_ok() {
        unsaved.push("refs/stash".into());
    }
    unsaved.extend(
        foreign_entries(&code_dir(home, slug))
            .into_iter()
            .filter(|e| !registered.iter().any(|wt| name(&wt.path) == *e))
            .map(|e| format!("{e} (not grove's)")),
    );
    unsaved
}

/// Per-root realize for one **declared** root: clone the declared-but-missing bare
/// (gix, the bounded-timeout path), reconcile that root's worktrees (two-way
/// additive), materialize its shares. Idempotent: a `present` root re-reconciles
/// to a no-op (no re-clone). The bare-but-trunk-missing re-clone guard (re-clone
/// only if no other live worktrees, else `failed`) is preserved.
///
/// `Err` is reserved for not-declared / lookup faults; a clone or guard failure is
/// an `Ok(Applied{status: Failed})` so one wedged root never aborts the rest.
///
/// A root nesting with another declared one — a hand edit `add_root` never saw — is
/// such a failure before anything is claimed, created or deleted: its directories are
/// the other's, so a clone would write into the other's bare and a failed realize's
/// cleanup would delete it.
pub fn reconcile_one(home: &Path, slug: &str) -> Result<Applied, Error> {
    let root = declared(home, slug)?;
    if let Some(why) = nesting(home, slug)? {
        return Ok(failed(slug, why));
    }

    // One realizer per root for the whole of this call, not only its clone: the
    // `cloning` marker's lock, once taken — on a crashed clone's leftover here, by a
    // trunk change, or by the clone itself — is held through the worktree and share
    // passes below, and a marker another realizer holds means its clone, or the passes
    // that follow it, are still running in this root's git. A free marker is a crashed
    // clone's, and the claim on it keeps a third realizer out while the root is repaired.
    let marker = root_dir(home, &root.slug).join(CLONING_MARKER);
    let mut claim = match claim_root(&root.slug, &marker, true) {
        Ok(claim) => claim,
        Err(refused) => return Ok(refused),
    };
    let leftover = claim.is_some();

    // Converge the trunk before realizing: which directory *is* the trunk is a
    // manifest question, and `realize` below reads the answer as settled. A pending
    // trunk change left unapplied would make `realize` see a root whose trunk
    // checkout is missing while its worktrees are live — the refuse-to-re-clone
    // guard — and report a wedged root for what is an ordinary manifest edit.
    // Skipped when the bare isn't there yet: the clone lays down a converged root.
    // A failed converge keeps a leftover's marker, and sheds one it laid itself below.
    // One it laid but did not write under — a racing realizer converged first — is
    // dropped unremoved for `realize` to reclaim as its own, never read as a leftover.
    let (applied, claim) = match converge_trunk(home, &root.slug, &mut claim) {
        Ok(()) => realize(
            home,
            &root,
            claim.filter(|_| leftover || trunk_dir(home, &root.slug).exists()),
        ),
        Err(e) => (
            failed(&root.slug, format!("{e:#}")),
            claim.filter(|_| !leftover),
        ),
    };
    // Converge this root's worktrees once it's on disk (create declared-but-
    // missing, adopt undeclared), then materialize the worktree environment
    // (declared shares) over them. Best-effort: a worktree or share hiccup must
    // not fail the root's reconcile — but a worktree the reconcile *cannot* recreate
    // (a failure prune can't fix, e.g. its branch is checked out elsewhere) is
    // surfaced to stderr, not silently dropped. stderr is grove-ops's operator
    // channel: the CLI prints it, and the daemon's log pipeline picks it up.
    // Kept off the wire — `Applied` is unchanged.
    if applied.status != ReconcileStatus::Failed {
        match crate::worktrees::reconcile(home, slug) {
            Ok(report) => {
                for line in reconcile_warnings(slug, &report) {
                    eprintln!("{line}");
                }
            }
            Err(e) => eprintln!("grove-ops WARN reconcile: failed for slug={slug}: {e:#}"),
        }
        let _ = crate::env::materialize(home, Some(slug), crate::env::Fix::Safe);
    }
    // The root is whole, so the marker goes — before the lock, which drops with
    // `claim`, so a realizer waiting on it wakes to a root with no marker at all.
    if claim.is_some() {
        let _ = std::fs::remove_file(&marker);
    }
    Ok(applied)
}

/// One operator-facing warning line per worktree the reconcile could not recreate
/// (`status == Failed`) — the observability half of the reconcile report. Pure and
/// separate from the emission so the surfaced content (slug, worktree, git error) is
/// testable; the caller writes each line to stderr. A healthy report yields none.
fn reconcile_warnings(slug: &str, report: &[crate::worktrees::WorktreeOutcome]) -> Vec<String> {
    report
        .iter()
        .filter(|o| o.status == crate::wire::WorktreeOutcomeStatus::Failed)
        .map(|o| {
            format!(
                "grove-ops WARN reconcile: could not recreate worktree slug={slug} name={} err={}",
                o.name,
                o.error.as_deref().unwrap_or("")
            )
        })
        .collect()
}

/// Bring the root's disk onto the manifest's `trunk`: the bare's `HEAD`, the checkout
/// the trunk branch lives in, and the manifest's own worktree set.
///
/// Changing the trunk is a manifest edit, so this is where the edit becomes disk.
/// Three moves, each a no-op once it has run:
///
/// - a `worktrees.<name>` entry already on the trunk branch is **dropped** — that
///   checkout *is* the trunk now, and leaving it declared would put the root's
///   integration checkout where a `tree remove` deletes it;
/// - the trunk's checkout is **created** when nothing is there, an ordinary
///   `git worktree add` from the bare;
/// - `HEAD` is **moved** onto the trunk branch, so [`git::default_branch`] — the
///   reader every consumer goes through — agrees with the declaration.
///
/// **Nothing is deleted.** The previous trunk stays checked out where it is, and the
/// worktree reconcile that follows adopts it into `worktrees.<name>`: the branch the
/// root integrated on yesterday is still on disk, now as an ordinary worktree.
///
/// The branch is proved to exist *before* anything is written, so a typo'd `trunk`
/// leaves the root exactly as it was rather than minting the typo as a branch (see
/// [`git::branch_exists`]).
///
/// Returns early — leaving a missing trunk checkout to [`realize`]'s recovery — when
/// `HEAD` already names the wanted branch and no declared worktree occupies the
/// trunk's directory. A trunk that *vanished* is a repair, not a convergence, and the
/// two answers differ: repair may re-clone, this may not.
///
/// Every write runs under `claim`: with none held, one is taken on a fresh marker and
/// the look repeated under it — a racing realizer may have converged meanwhile — and
/// it is left in `claim` for [`reconcile_one`] to hold through its passes. A no-op
/// converge claims nothing, and a held marker is [`IN_FLIGHT`] with nothing touched.
fn converge_trunk(home: &Path, slug: &str, claim: &mut Option<std::fs::File>) -> Result<()> {
    let bare = bare_dir(home, slug);
    if !bare.exists() {
        return Ok(());
    }
    let want = trunk(home, slug)?;
    let head = git::default_branch(&bare)?;
    let occupant = manifest::list_worktrees(&manifest_path(home), slug)
        .unwrap_or_default()
        .into_iter()
        .find(|w| w.name == want.name);
    if head == want.branch && occupant.is_none() {
        return Ok(());
    }
    if claim.is_none() {
        *claim = claim_marker(&root_dir(home, slug).join(CLONING_MARKER))?;
        ensure!(claim.is_some(), IN_FLIGHT);
        return converge_trunk(home, slug, claim);
    }

    // A declared worktree at the trunk's directory but on a *different* branch is two
    // checkouts asking for one path, which no order of operations resolves — refuse,
    // naming both, rather than drop somebody's declaration or leave the trunk sitting
    // on a branch it does not name.
    if let Some(w) = &occupant
        && w.branch != want.branch
    {
        bail!(
            "cannot move the trunk of {slug} to `{}`: worktree `{}` already occupies \
             that directory on branch `{}`; rename or remove it first",
            want.branch,
            w.name,
            w.branch
        );
    }
    if !git::branch_exists(&bare, &want.branch) {
        bail!(
            "trunk branch `{}` is in neither the bare nor origin; push it, or correct \
             `trunk` for {slug} in the manifest",
            want.branch
        );
    }
    if occupant.is_some() {
        manifest::remove_worktree(&manifest_path(home), slug, &want.name)?;
    }
    if !want.dir.exists() {
        // The same prune `recover_trunk` opens with: a directory removed out-of-band
        // leaves a registration git refuses to re-add over.
        let _ = git::worktree_prune(&bare);
        git::worktree_add(&bare, &want.dir, &want.branch, None)?;
    }
    if head != want.branch {
        // Shares first, while `HEAD` still names the old trunk: it is the one record of
        // which links and sources were grove's, and moving it forgets them — so a pass
        // with an error row leaves `HEAD` where it is, for the next event to retry.
        if let Some(o) = crate::env::carry(home, slug, &crate::worktrees::name_for(&head))
            .into_iter()
            .find(|o| o.status.is_error())
        {
            bail!(
                "cannot move the trunk of {slug} to `{}`: share `{}` in {} failed to carry: {}",
                want.branch,
                o.path,
                o.worktree.as_deref().unwrap_or("the trunk"),
                o.reason.unwrap_or_default()
            );
        }
        git::set_head(&bare, &want.branch)?;
    }
    Ok(())
}

/// Bring a declared root onto disk, under the `claim` [`reconcile_one`] took on a
/// leftover `cloning` marker or for a trunk change — or, with none, one of its own
/// before a repair or a clone writes anything. The claim comes back only with a root
/// made whole, for `reconcile_one` to hold through its passes and then shed the marker;
/// a refusal drops it and leaves the marker, so a crashed clone — or a root whose
/// repair failed under it — stays visibly grove's.
fn realize(
    home: &Path,
    root: &manifest::Root,
    mut claim: Option<std::fs::File>,
) -> (Applied, Option<std::fs::File>) {
    let bare = bare_dir(home, &root.slug);
    let trunk = trunk_dir(home, &root.slug);

    if bare.exists() && trunk.exists() {
        return (present(&root.slug), claim);
    }

    // The trunk checkout is gone but the bare is here — it vanished out-of-band (a
    // stray `rm`, a `git worktree prune`), or a clone crashed between the two.
    //
    // Three answers, cheapest and least destructive first:
    //
    // 1. **Refuse** when the bare carries other live worktrees (a user's checkouts,
    //    possibly with uncommitted work). Reconcile *adds, never deletes*.
    // 2. **Re-add the trunk** from the bare that is already there. This is the whole
    //    of the real repair — a worktree is the only thing missing — and it touches
    //    nothing else under the root.
    // 3. Only when the bare cannot produce a worktree at all (it is itself broken)
    //    is a re-clone the answer, and a re-clone means deleting the root directory.
    //    That is done ONLY if the code dir holds nothing at all: `remove_dir_all` on a
    //    path a human also writes into is how carried law 5 fails on the one code path
    //    that names it. The root dir goes with it — it is grove's alone, and the bare
    //    inside it is the thing being replaced.
    if bare.exists() {
        // Claim the root before reading it, on a fresh marker when `reconcile_one`
        // found none: a clone that began since claimed its marker before writing this
        // bare, and two realizers that both found the trunk missing would both prune
        // and re-add it — the loser re-cloning over the winner's repair.
        let marker = root_dir(home, &root.slug).join(CLONING_MARKER);
        let fresh = claim.is_none();
        if fresh {
            match claim_root(&root.slug, &marker, false) {
                Ok(held) => claim = held,
                Err(refused) => return (refused, None),
            }
            // A racing realizer's repair may have landed since the look above.
            if trunk.exists() {
                let _ = std::fs::remove_file(&marker);
                return (present(&root.slug), None);
            }
        }
        if has_live_worktrees(home, &root.slug, &bare) {
            // A human's checkouts are no clone's leftover: a marker this call laid goes.
            if fresh {
                let _ = std::fs::remove_file(&marker);
            }
            return (
                failed(
                    &root.slug,
                    "root has live worktrees but its trunk checkout is missing; refusing \
                     to re-clone (recreate it, or `grove clone remove` the root)"
                        .into(),
                ),
                None,
            );
        }
        match recover_trunk(home, &root.slug, &bare) {
            Ok(branch) => return (cloned(&root.slug, branch), claim),
            Err(e) => {
                let foreign = foreign_entries(&code_dir(home, &root.slug));
                if !foreign.is_empty() {
                    return (
                        failed(
                            &root.slug,
                            format!(
                                "could not recreate the trunk ({e:#}), and re-cloning would \
                                 delete {} under the code dir; move it aside, or \
                                 `grove clone remove` the root",
                                foreign.join(", ")
                            ),
                        ),
                        None,
                    );
                }
                let _ = std::fs::remove_dir_all(code_dir(home, &root.slug));
                let _ = std::fs::remove_dir_all(root_dir(home, &root.slug));
                // The marker went with the root dir; the clone claims a fresh one.
                claim = None;
            }
        }
    }

    // The clone arm below is the only path here that writes into a root grove has
    // never realized, and it reads the absence of the bare as "nothing is here" —
    // which is false in two shapes, each guarded once, in this order:
    //
    // 1. **A legacy root.** Its bare and its trunk checkout sit *inside* the code dir,
    //    so every probe the arm makes says missing and it would clone a whole second
    //    root beside the first, leaving both to be untangled by hand. The order is
    //    load-bearing: the occupancy check below would fire on a legacy root too, and
    //    "holds .git" is the less useful of the two answers.
    // 2. **An occupied code dir.** A code dir holds checkouts and only checkouts, so
    //    anything already there is a human's, and cloning into it interleaves grove's
    //    layout with theirs.
    //
    // Both refuse rather than migrate or move: reconcile adds, never deletes, and the
    // migration is `grove doctor --fix`'s, where the operator asked for it.
    let dir = code_dir(home, &root.slug);
    if let Some(legacy) = crate::layout::legacy(home, &root.slug) {
        return (
            failed(
                &root.slug,
                format!(
                    "root is in the legacy layout ({legacy}): run `grove doctor --fix` to \
                     migrate it in place"
                ),
            ),
            None,
        );
    }
    let foreign = foreign_entries(&dir);
    if !foreign.is_empty() {
        return (
            failed(
                &root.slug,
                format!(
                    "code directory holds {}; refusing to clone into it (remove them, or \
                     `grove clone remove` the root)",
                    foreign.join(", ")
                ),
            ),
            None,
        );
    }

    match clone_and_trunk(home, root, &bare, claim) {
        Ok((branch, claim)) => (cloned(&root.slug, branch), Some(claim)),
        Err(e) => (failed(&root.slug, format!("{e:#}")), None),
    }
}

/// Lay both halves of a root down: the bare under `roots/<slug>`, the trunk checkout
/// under `code/<slug>`. Both directories are created here, because a root that exists
/// at all exists in both trees.
fn clone_and_trunk(
    home: &Path,
    root: &manifest::Root,
    bare: &Path,
    claim: Option<std::fs::File>,
) -> Result<(String, std::fs::File)> {
    let dir = root_dir(home, &root.slug);
    let code = code_dir(home, &root.slug);
    for d in [&dir, &code] {
        std::fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
    }

    // A marker for the whole of the network clone, so `adopt` does not declare the
    // half-materialized bare a clone in flight has already written (gix creates the
    // bare with its `origin` before a byte of the transfer lands). Its flock is what
    // tells that clone from a crashed one: a lock dies with its process, so a second
    // realizer — another CLI, a daemon booted mid-clone — finds it held only while
    // the clone is live, and backs off. A failed clone removes it here; a successful
    // one hands the claim back still held, and `reconcile_one` removes the marker
    // after the passes that follow. A *crashed* clone leaves it behind on purpose —
    // the leftover is then visibly grove's, and undeclaring the root keeps it
    // undeclared instead of adopt resurrecting it on the next manifest event, until
    // `reconcile_one` makes the root whole. It lives beside the bare rather than in
    // the code dir, which holds checkouts and nothing else.
    let marker = dir.join(CLONING_MARKER);
    let Some(claim) = claim.map_or_else(|| claim_marker(&marker), |c| Ok(Some(c)))? else {
        bail!(IN_FLIGHT);
    };
    let cloned = git::clone_bare(&root.url, bare).and_then(|cloned_onto| {
        // Desired state wins the moment the bare exists. The manifest's `trunk` is
        // what this root integrates on; `HEAD` is where `git::default_branch` — the
        // one reader every consumer goes through — reads it back from. Setting it
        // here means an undeclared root still has a trunk (whatever it cloned onto)
        // and a declared one never spends a moment on the wrong branch.
        let branch = root.trunk.clone().unwrap_or(cloned_onto);
        git::set_head(bare, &branch)?;
        let trunk = code.join(crate::worktrees::name_for(&branch));
        git::worktree_add(bare, &trunk, &branch, None).map(|()| branch)
    });
    if cloned.is_err() {
        let _ = std::fs::remove_file(&marker);
    }
    cloned.map(|branch| (branch, claim))
}

/// Written under `roots/<slug>` for the length of a clone, a repair or a trunk change
/// and the passes after it; see [`clone_and_trunk`] and [`reconcile_one`].
const CLONING_MARKER: &str = "cloning";

/// The refusal a second realizer reports while the marker's lock is held.
pub const IN_FLIGHT: &str =
    "a clone or trunk change of this root is in flight in another realizer; nothing touched";

/// Block until the clone or trunk change in flight on `slug` ends — its marker gone, or its lock
/// released, a free leftover the retry then claims — and answer `true`; `false` at
/// once only when the marker cannot be opened or locked at all, so a caller retrying
/// on `true` never spins on a refusal no clone will clear.
#[must_use]
pub fn await_clone(home: &Path, slug: &str) -> bool {
    match std::fs::File::open(root_dir(home, slug).join(CLONING_MARKER)) {
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        Ok(marker) => match marker.try_lock() {
            Ok(()) => true,
            Err(e) => matches!(e, std::fs::TryLockError::WouldBlock) && marker.lock().is_ok(),
        },
    }
}

/// Claim the marker — only a leftover one when `leftover`, answering `Ok(None)` when
/// there is none — or the refusal to report while another realizer holds it, or,
/// naming the file, when it cannot be opened, which no other realizer's exit will clear.
fn claim_root(slug: &str, marker: &Path, leftover: bool) -> Result<Option<std::fs::File>, Applied> {
    match (!leftover || marker.exists()).then(|| claim_marker(marker)) {
        None => Ok(None),
        Some(Ok(Some(claim))) => Ok(Some(claim)),
        Some(Ok(None)) => Err(failed(slug, IN_FLIGHT.into())),
        Some(Err(e)) => Err(failed(slug, format!("{e:#}"))),
    }
}

/// Create `marker` and take its exclusive flock without blocking: `None` while another
/// realizer holds it (any lock error reads as held — backing off is the safe answer).
/// The lock lives as long as the returned handle, and std opens it `O_CLOEXEC`, so no
/// spawned git outlives it holding the lock. A lock on an inode the holder unlinked
/// between our open and our lock is no claim: the next realizer creates a fresh marker
/// and locks that, so the lock counts only while `marker` still names it.
fn claim_marker(marker: &Path) -> Result<Option<std::fs::File>> {
    let file =
        std::fs::File::create(marker).with_context(|| format!("create {}", marker.display()))?;
    Ok(file
        .try_lock()
        .ok()
        .filter(|()| names(marker, &file))
        .map(|()| file))
}

/// Does `path` still name the inode `file` has open?
fn names(path: &Path, file: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    matches!((path.metadata(), file.metadata()),
        (Ok(p), Ok(f)) if (p.dev(), p.ino()) == (f.dev(), f.ino()))
}

/// Re-add the missing trunk checkout from the bare that is already on disk — the
/// whole of the repair when a checkout, not a repository, is what went missing.
///
/// Pruning first is what makes it work at all: a checkout deleted out-of-band leaves
/// a `prunable` registration under the bare's `worktrees/`, and git refuses to re-add
/// a path it still believes is registered.
fn recover_trunk(home: &Path, slug: &str, bare: &Path) -> Result<String> {
    let _ = git::worktree_prune(bare);
    let trunk = trunk(home, slug)?;
    git::worktree_add(bare, &trunk.dir, &trunk.branch, None)?;
    Ok(trunk.branch)
}

/// Names under a **code dir** that are not checkouts grove laid down, sorted — what a
/// clone or a re-clone would have to write over. A code dir holds checkouts and only
/// checkouts, so this is every entry in it bar one: `.DS_Store`, a Finder artefact
/// that appears in any directory a Mac has looked at and that no operator chose to put
/// there. An unreadable directory reports none: it is about to be created or
/// re-created anyway, and a read error is not evidence of a human's files.
///
/// `pub(crate)` for [`crate::doctor`]'s orphan-code walk, which asks the same question
/// of the same directory — "is anything in there?" — and must get the realizer's
/// answer, `.DS_Store` tolerance included.
pub(crate) fn foreign_entries(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| name != ".DS_Store")
        .collect();
    names.sort();
    names
}

/// Does `bare` have any linked worktree that is somebody's — i.e. not one of
/// grove's own disposable checkouts? Guards the destructive partial-clone
/// recovery: a user's checkout must never be wiped to recover a missing trunk.
///
/// Two exclusions, and they need different tests. The trunk is grove's checkout of the
/// branch this root integrates on and is exactly the thing being recovered here, so it
/// is matched by *basename* — which is what names a checkout in the code dir. A
/// warm-pool slot is registered at `roots/<slug>/pool/slot-N`, outside the code tree
/// entirely and carrying a basename (`slot-N`) that names nothing here; it is excluded
/// by path, through the same `worktrees::under_pool` prefix test `pool::slots` counts
/// with. Wiping slots is correct: a slot is a detached checkout at the trunk tip
/// carrying no user data, the bare is re-cloned from scratch so its registrations go
/// with it, and `pool::fill` restores the declared target at the new tip.
///
/// **Fail safe** — if `git worktree list` errors (a corrupt/unreadable bare, which
/// can still own live checkouts), assume worktrees may exist and refuse the wipe. A
/// genuinely partial clone has a valid bare, so this still re-clones cleanly.
// ※ trunk-recovery-guard
fn has_live_worktrees(home: &Path, slug: &str, bare: &Path) -> bool {
    let trunk = trunk(home, slug).ok();
    let trunk = trunk.as_ref().map(|t| t.name.as_str());
    match git::worktree_list(bare) {
        Ok(wts) => wts.iter().any(|wt| {
            let is_trunk = wt
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| trunk == Some(n));
            !is_trunk && !crate::worktrees::under_pool(home, slug, &wt.path)
        }),
        Err(_) => true,
    }
}

fn cloned(slug: &str, branch: String) -> Applied {
    Applied {
        slug: slug.to_string(),
        status: ReconcileStatus::Cloned,
        default_branch: Some(branch),
        error: None,
    }
}

fn failed(slug: &str, error: String) -> Applied {
    Applied {
        slug: slug.to_string(),
        status: ReconcileStatus::Failed,
        default_branch: None,
        error: Some(error),
    }
}

fn present(slug: &str) -> Applied {
    Applied {
        slug: slug.to_string(),
        status: ReconcileStatus::Present,
        default_branch: None,
        error: None,
    }
}

/// Report of a [`sync`]: the fetch landed (or the op errored), the trunk outcome,
/// the tip the root now sits at, and how many stale warm slots were recycled.
#[derive(Debug, Serialize)]
pub struct SyncReport {
    pub fetched: bool,
    /// `updated | already_current | diverged | dirty` — see `git::FastForward`.
    pub trunk: git::FastForward,
    /// The trunk's HEAD after the op — the tip new pool slots should sit at.
    pub tip: String,
    pub stale_slots_pruned: usize,
}

/// Sync one **ready** root with its remote: fetch the default branch (bounded,
/// single-refspec), fast-forward the trunk checkout onto the tracking ref, then prune any
/// warm-pool slot stranded at a pre-sync tip (a stale slot defeats the pool's
/// purpose; the engine's fill loop restores the target at the new tip). One lane
/// call = atomic per root, like `reconcile_one`. Never forces: a diverged or
/// dirty trunk is a *reported* outcome ([`SyncReport::trunk`]), not a reset.
/// Pruning compares each slot to the trunk's tip *after* the op — it recycles
/// exactly the slots a fresh `pool.fill` would no longer produce, whichever way
/// the trunk outcome went.
// ※ sync-never-forces
pub fn sync(home: &Path, slug: &str) -> Result<SyncReport, Error> {
    manifest::validate_slug(slug).map_err(Error::invalid_input)?;
    // Both halves of "not ready" name the path that is missing: the two are different
    // repairs (a re-clone versus a `worktree add`), and a message that named neither
    // sent the operator looking for whichever they last touched.
    let not_ready = |missing: &Path| {
        Error::NotReady(format!(
            "cannot sync {slug}: root not ready ({} is missing)",
            missing.display()
        ))
    };
    let bare = bare_dir(home, slug);
    if !bare.exists() {
        return Err(not_ready(&bare));
    }
    let trunk = trunk(home, slug).map_err(Error::git)?;
    if !trunk.dir.exists() {
        return Err(not_ready(&trunk.dir));
    }

    // The trunk's own branch, not the bare's `HEAD`: reconcile converges the two, but
    // a sync between a `trunk` edit and the reconcile that applies it must fetch and
    // fast-forward the branch the manifest names, never the one being left behind.
    let branch = &trunk.branch;
    git::fetch(&bare, "origin", branch).map_err(Error::network)?;
    let outcome = git::fast_forward(&trunk.dir, &format!("origin/{branch}")).map_err(Error::git)?;
    let tip = git::rev_parse(&trunk.dir, "HEAD").map_err(Error::git)?;

    // Recycle slots detached at any other commit. Best-effort per slot — a locked
    // slot is skipped, not fatal; the count reports what actually happened.
    let mut pruned = 0;
    for slot in crate::pool::slots(home, slug).map_err(Error::git)? {
        let Ok(head) = git::rev_parse(&slot, "HEAD") else {
            continue;
        };
        if head != tip && git::worktree_remove(&bare, &slot).is_ok() {
            pruned += 1;
        }
    }

    Ok(SyncReport {
        fetched: true,
        trunk: outcome,
        tip,
        stale_slots_pruned: pruned,
    })
}

/// Derive an `owner/repo` slug from a clone URL (https, ssh, or scp-style).
pub fn slug_from_url(url: &str) -> Result<String> {
    let s = url.trim();

    let rest = if let Some((_, after)) = s.split_once("://") {
        after.split_once('/').map_or("", |x| x.1)
    } else if s
        .split_once(':')
        .is_some_and(|(host, _)| !host.contains('/'))
    {
        // scp-style, user or not: git's rule is a colon before any slash.
        s.rsplit_once(':').map_or(s, |(_, p)| p)
    } else {
        s
    };

    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let segs: Vec<&str> = rest.split('/').filter(|x| !x.is_empty()).collect();

    match segs.as_slice() {
        [.., owner, repo] => Ok(format!("{owner}/{repo}")),
        [repo] => Ok((*repo).to_string()),
        [] => bail!("cannot derive slug from url: {url}"),
    }
}

/// Global discovery pass: adopt undeclared on-disk bare repos into the manifest,
/// **no cloning, no worktree/share work** (that's `reconcile_one`'s job). Walks
/// `roots/<org>/<name>/` (depth-2, dotfile dirs like `.trash` skipped at both levels)
/// for the slugs, reads the origin URL of each slug's bare under [`bare_dir`], and
/// declares any slug not already in the manifest. Idempotent and additive — never
/// auto-deletes a declared root,
/// never overwrites a declared URL. Repos without an `origin` remote are skipped
/// (no URL to record). Returns one `Applied{status: Adopted}` per newly-declared
/// slug.
///
/// The `roots/` tree and never the code tree: a root *is* its root directory, so the
/// bare is what proves one exists. A code dir proves nothing — it holds checkouts, and
/// checkouts of what is exactly the question — so a code dir with no root behind it is
/// a doctor finding ([`crate::doctor::orphan_code_checks`]) rather than something to
/// declare a root from.
pub fn adopt(home: &Path) -> Result<Vec<Applied<AdoptStatus>>, Error> {
    let tree = home.join("roots");
    if !tree.is_dir() {
        return Ok(vec![]);
    }

    let declared: std::collections::HashSet<String> =
        list(home)?.into_iter().map(|r| r.slug).collect();
    let manifest = manifest_path(home);
    let mut adopted = Vec::new();

    for org_entry in std::fs::read_dir(&tree)
        .with_context(|| format!("read {}", tree.display()))
        .map_err(Error::io)?
    {
        let Ok(org_entry) = org_entry else { continue };
        if !is_adoptable_dir(&org_entry) {
            continue;
        }
        // A non-UTF-8 directory name can't become a valid slug — see slug_segment.
        let org = match slug_segment(&org_entry.file_name(), None) {
            Ok(org) => org,
            Err(report) => {
                adopted.push(report);
                continue;
            }
        };

        let Ok(repos) = std::fs::read_dir(org_entry.path()) else {
            continue;
        };
        for repo_entry in repos {
            let Ok(repo_entry) = repo_entry else { continue };
            if !is_adoptable_dir(&repo_entry) {
                continue;
            }
            let repo = match slug_segment(&repo_entry.file_name(), Some(&org)) {
                Ok(repo) => repo,
                Err(report) => {
                    adopted.push(report);
                    continue;
                }
            };

            let slug = format!("{org}/{repo}");
            if declared.contains(&slug) || manifest::validate_slug(&slug).is_err() {
                continue;
            }

            let bare = bare_dir(home, &slug);
            // A clone in flight (or one that died mid-transfer) has a bare with an
            // `origin` and nothing else. Adopting it re-declares a root the operator
            // may have just undeclared, and the engine then re-clones what they
            // asked to be rid of — so the marker `clone_and_trunk` writes is a
            // "not mine to discover" sign whether or not its lock is held: a crashed
            // clone's leftover stays undeclared until `reconcile_one` makes it whole.
            if root_dir(home, &slug).join(CLONING_MARKER).exists() {
                continue;
            }
            // Skip silently with no bare, or one with no `origin`: there is no URL to
            // record, so adoption isn't possible.
            let Ok(url) = git::remote_url(&bare, "origin") else {
                continue;
            };

            // Best-effort: a single adoption failure mustn't abort the rest.
            if manifest::add_root(&manifest, &slug, &url).is_ok() {
                adopted.push(Applied {
                    slug,
                    status: AdoptStatus::Adopted,
                    default_branch: None,
                    error: None,
                });
            }
        }
    }

    // The watcher runs adopt on every manifest *save*, so this is where a pure
    // hand-edit reorder gets canonicalized (the declares above already sort via
    // `write_doc`; this catches a save that declared nothing). Write-if-changed, so
    // it doesn't loop on its own file-change event. Best-effort — a malformed save
    // mustn't fail the adopt.
    let _ = manifest::canonicalize(&manifest);
    Ok(adopted)
}

/// A directory name as a slug segment: UTF-8 passes through; a non-UTF-8 name is
/// the `skipped` report line (a U+FFFD-laced `to_string_lossy` slug would no
/// longer round-trip to the real directory, so it must never be minted). `org`
/// prefixes the lossy rendering for a repo-level skip. Pure — the decision is
/// unit-testable with a synthetic `OsStr`, since APFS won't host an invalid-UTF-8
/// name to exercise the filesystem path.
fn slug_segment(
    name: &std::ffi::OsStr,
    org: Option<&str>,
) -> std::result::Result<String, Applied<AdoptStatus>> {
    if let Some(s) = name.to_str() {
        Ok(s.to_string())
    } else {
        let lossy = name.to_string_lossy();
        Err(skipped(&org.map_or_else(
            || lossy.to_string(),
            |org| format!("{org}/{lossy}"),
        )))
    }
}

/// A report line for a directory adoption skipped because its name isn't UTF-8.
/// `name` is the lossy rendering — for the operator's eyes only, never used as a slug.
fn skipped(name: &str) -> Applied<AdoptStatus> {
    Applied {
        slug: name.to_string(),
        status: AdoptStatus::Skipped,
        default_branch: None,
        error: Some("non-UTF-8 directory name; cannot form a slug".into()),
    }
}

fn is_adoptable_dir(entry: &std::fs::DirEntry) -> bool {
    let name = entry.file_name();
    let name = name.to_string_lossy();
    !name.starts_with('.') && entry.file_type().is_ok_and(|t| t.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture_repo(dir: &Path) {
        crate::testfix::fixture_repo(dir);
    }

    #[test]
    fn slug_from_url_handles_common_shapes() {
        assert_eq!(slug_from_url("https://github.com/o/r.git").unwrap(), "o/r");
        assert_eq!(slug_from_url("https://github.com/o/r").unwrap(), "o/r");
        assert_eq!(slug_from_url("git@github.com:o/r.git").unwrap(), "o/r");
        // git reads `host:path` as scp-style ssh with or without a user — the
        // ~/.ssh/config Host-alias shape.
        assert_eq!(slug_from_url("gh:o/r.git").unwrap(), "o/r");
        assert_eq!(slug_from_url("github.com:o/r").unwrap(), "o/r");
    }

    #[test]
    fn slug_from_url_handles_edge_shapes() {
        // trailing slash, deep path (last two segments win), bare owner/repo
        assert_eq!(slug_from_url("https://example.com/o/r/").unwrap(), "o/r");
        assert_eq!(
            slug_from_url("https://host/group/sub/o/r.git").unwrap(),
            "o/r"
        );
        assert_eq!(slug_from_url("o/r").unwrap(), "o/r");
        // single segment is kept as-is (the manifest key)
        assert_eq!(slug_from_url("r").unwrap(), "r");
        // nothing to derive
        assert!(slug_from_url("").is_err());
        assert!(slug_from_url("https://host/").is_err());
    }

    #[test]
    fn add_declares_and_clones_just_its_own_root() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);

        let applied = add(&home, "o/r", src.to_str().unwrap()).unwrap();
        assert_eq!(applied.len(), 1, "add realizes only the root it declared");
        assert_eq!(applied[0].slug, "o/r");
        assert_eq!(applied[0].status, ReconcileStatus::Cloned);
        assert_eq!(applied[0].default_branch.as_deref(), Some("main"));
        assert_eq!(bare_dir(&home, "o/r"), home.join("roots/o/r/bare"));
        assert!(bare_dir(&home, "o/r").join("HEAD").exists());
        assert_eq!(trunk_dir(&home, "o/r"), home.join("code/o/r/main"));
        assert!(trunk_dir(&home, "o/r").join("README.md").exists());
        assert!(
            !home.join("code/o/r/bare").exists(),
            "the bare is the root dir's, never the code dir's"
        );

        // Declaring a second root realizes only it — the first is untouched.
        let other = tmp.path().join("src2");
        fixture_repo(&other);
        let applied = add(&home, "o/r2", other.to_str().unwrap()).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].slug, "o/r2");
        assert_eq!(applied[0].status, ReconcileStatus::Cloned);
    }

    /// Declare `trunk` the way a hand-edit or a git-sync would. There is no mutator
    /// for it yet — the resolution is what this slice owns, and the manifest is its
    /// only input.
    fn declare_trunk(home: &Path, slug: &str, branch: &str) {
        let path = manifest_path(home);
        let mut doc: toml_edit::DocumentMut =
            std::fs::read_to_string(&path).unwrap().parse().unwrap();
        doc["roots"][slug]["trunk"] = toml_edit::value(branch);
        std::fs::write(&path, doc.to_string()).unwrap();
    }

    /// The one resolution, both ways round: a declared `trunk` names the branch and
    /// so the directory, and an undeclared one falls back to the bare's HEAD — which
    /// is what makes an adopted root, which declares nothing, have a trunk too.
    #[test]
    fn trunk_reads_the_declaration_and_falls_back_to_the_bares_head() {
        let tmp = TempDir::new().unwrap();
        let home = crate::testfix::home_with_root(&tmp);
        let code = code_dir(&home, "o/r");
        crate::testfix::git(&bare_dir(&home, "o/r"), &["branch", "canary", "main"]);

        let fallback = trunk(&home, "o/r").unwrap();
        assert_eq!(fallback.branch, "main");
        assert_eq!(fallback.name, "main");
        assert_eq!(fallback.dir, code.join("main"));

        declare_trunk(&home, "o/r", "canary");
        let declared = trunk(&home, "o/r").unwrap();
        assert_eq!(declared.branch, "canary");
        assert_eq!(declared.name, "canary");
        assert_eq!(declared.dir, code.join("canary"));

        // The directory is `name_for` of the branch, never the branch verbatim.
        declare_trunk(&home, "o/r", "release/2.0");
        let slashed = trunk(&home, "o/r").unwrap();
        assert_eq!(slashed.branch, "release/2.0");
        assert_eq!(slashed.dir, code.join("release-2.0"));
    }

    /// A declared trunk is where the clone checks out and what the bare's HEAD is set
    /// to — desired state applied at the one moment the root comes into existence, so
    /// every consumer that reads HEAD agrees with the manifest from the start.
    #[test]
    fn a_declared_trunk_is_what_a_clone_checks_out_and_points_head_at() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        crate::testfix::git(&src, &["branch", "canary", "main"]);

        manifest::add_root(&manifest_path(&home), "o/r", src.to_str().unwrap()).unwrap();
        declare_trunk(&home, "o/r", "canary");

        let applied = reconcile_one(&home, "o/r").unwrap();
        assert_eq!(applied.status, ReconcileStatus::Cloned);
        assert_eq!(applied.default_branch.as_deref(), Some("canary"));
        assert!(code_dir(&home, "o/r").join("canary/README.md").is_file());
        assert!(
            !code_dir(&home, "o/r").join("main").exists(),
            "the trunk is the declared branch's checkout, and the only one"
        );
        assert_eq!(
            git::default_branch(&bare_dir(&home, "o/r")).unwrap(),
            "canary",
            "the bare's HEAD carries the declaration for every reader of it"
        );
    }

    /// A source repo on `main` with a second branch `canary` off it — the shape a
    /// trunk change needs: two branches the root can integrate on, one at a time.
    fn fixture_repo_with_canary(dir: &Path) {
        fixture_repo(dir);
        crate::testfix::git(dir, &["branch", "canary", "main"]);
    }

    fn declared_worktrees(home: &Path, slug: &str) -> Vec<manifest::Worktree> {
        manifest::list_worktrees(&manifest_path(home), slug).unwrap()
    }

    /// A root on `main` sharing `.env` into worktree `feat`, a link through `../main/`.
    fn shared_root(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo_with_canary(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        manifest::add_share(&manifest_path(&home), "o/r", "symlink", &[".env"]).unwrap();
        crate::worktrees::create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();
        let code = code_dir(&home, "o/r");
        (home, code)
    }

    /// While a trunk change is refused, an ordinary share pass knows only the declared
    /// trunk: a link through the old one is a conflict, never repointed into the
    /// checkout occupying the new trunk's directory.
    #[test]
    fn a_refused_trunk_change_leaves_links_through_the_old_trunk_alone() {
        let tmp = TempDir::new().unwrap();
        let (home, code) = shared_root(&tmp);
        crate::worktrees::create(&home, "o/r", "canary", "feature/y", Some("main")).unwrap();
        declare_trunk(&home, "o/r", "canary");
        assert!(reconcile_one(&home, "o/r").unwrap().error.is_some());

        let out = crate::env::materialize(&home, Some("o/r"), crate::env::Fix::Safe).unwrap();
        assert!(
            out.iter()
                .any(|o| o.worktree.as_deref() == Some("feat") && o.status.is_conflict()),
            "{out:?}"
        );
        assert_eq!(
            std::fs::read_link(code.join("feat/.env")).unwrap(),
            Path::new("../main/.env")
        );
    }

    /// An old-trunk source the pass cannot read — or a symlink source pointing inside the
    /// old trunk, which carried verbatim would dangle — is an error, not an absent source,
    /// and it holds the whole carry: no share is seeded and no link repointed or laid —
    /// not by an ordinary pass in between either — and `HEAD` stays put so the next event
    /// retries.
    #[test]
    fn an_unreadable_old_source_holds_the_trunk_change() {
        for share in [".other", ".k1"] {
            let tmp = TempDir::new().unwrap();
            let (home, code) = shared_root(&tmp);
            manifest::add_share(&manifest_path(&home), "o/r", "symlink", &[share]).unwrap();
            if share == ".k1" {
                std::fs::write(code.join("main/.real"), "K").unwrap();
                std::os::unix::fs::symlink(".real", code.join("main/.k1")).unwrap();
            } else {
                std::fs::create_dir(code.join("main/.other")).unwrap();
            }
            crate::env::materialize(&home, Some("o/r"), crate::env::Fix::Safe).unwrap();
            declare_trunk(&home, "o/r", "canary");
            let error = reconcile_one(&home, "o/r").unwrap().error.unwrap();
            assert!(error.contains(share), "{error}");
            crate::worktrees::create(&home, "o/r", "late", "feature/z", Some("main")).unwrap();
            assert!(reconcile_one(&home, "o/r").unwrap().error.is_some());
            for absent in ["canary/.env", "late/.env", &format!("canary/{share}")] {
                assert!(
                    std::fs::symlink_metadata(code.join(absent)).is_err(),
                    "{absent}"
                );
            }
            for p in [".env", share] {
                assert_eq!(
                    std::fs::read_link(code.join("feat").join(p)).unwrap(),
                    Path::new("../main").join(p)
                );
            }
            assert_eq!(
                git::default_branch(&bare_dir(&home, "o/r")).unwrap(),
                "main"
            );
        }
    }

    /// An old trunk the carry cannot open — here a symlink, which `NOFOLLOW` refuses —
    /// is no absent one: seeding empty sources from it would repoint every link onto
    /// them, so it holds the carry like an unreadable source. A missing one, with
    /// nothing to seed, lets it through.
    #[test]
    fn an_unopenable_old_trunk_holds_the_trunk_change() {
        for held in [true, false] {
            let tmp = TempDir::new().unwrap();
            let (home, code) = shared_root(&tmp);
            std::fs::rename(code.join("main"), tmp.path().join("moved")).unwrap();
            if held {
                std::os::unix::fs::symlink(tmp.path().join("moved"), code.join("main")).unwrap();
            }
            declare_trunk(&home, "o/r", "canary");
            assert_eq!(reconcile_one(&home, "o/r").unwrap().error.is_some(), held);
            let trunk = if held { "main" } else { "canary" };
            assert_eq!(git::default_branch(&bare_dir(&home, "o/r")).unwrap(), trunk);
            assert_eq!(
                std::fs::read_link(code.join("feat/.env")).unwrap(),
                Path::new("..").join(trunk).join(".env")
            );
        }
    }

    /// A worktree promoted to trunk carries grove's link through the old trunk in its
    /// source slot — never the user's source: the carry seeds over it, so the bytes
    /// outlive the ex-trunk once that ordinary worktree is removed.
    #[test]
    fn a_promoted_worktree_seeds_its_source_over_its_share_link() {
        let tmp = TempDir::new().unwrap();
        let (home, code) = shared_root(&tmp);
        std::fs::write(code.join("main/.env"), "SECRET").unwrap();
        std::fs::create_dir(code.join("main/data")).unwrap();
        let shares = [".k", ".env.local", "data"];
        manifest::add_share(&manifest_path(&home), "o/r", "symlink", &shares).unwrap();
        crate::worktrees::create(&home, "o/r", "canary", "canary", None).unwrap();
        // an absent old source seeds empty; a link not the one grove lays here, or one
        // reading a directory, stays the link it is
        std::fs::remove_file(code.join("main/.k")).unwrap();
        std::fs::remove_file(code.join("canary/.env.local")).unwrap();
        std::os::unix::fs::symlink("../main/.env", code.join("canary/.env.local")).unwrap();
        declare_trunk(&home, "o/r", "canary");
        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Present
        );
        for (p, file) in [
            (".env", true),
            (".k", true),
            (".env.local", false),
            ("data", false),
        ] {
            let meta = std::fs::symlink_metadata(code.join("canary").join(p)).unwrap();
            assert_eq!(meta.is_file(), file, "{p}");
        }
        let info = bare_dir(&home, "o/r").join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("exclude"), ".env*\n").unwrap();
        crate::worktrees::remove(&home, "o/r", "main").unwrap();
        assert_eq!(
            std::fs::read_to_string(code.join("feat/.env")).unwrap(),
            "SECRET"
        );
    }

    /// A share link laid through the old trunk is grove's own: the trunk change moves
    /// it, and the source it reads, onto the new trunk rather than leaving a conflict —
    /// a symlink source, the user's own choice, as the same link.
    #[test]
    fn a_trunk_change_carries_share_links_and_sources_onto_the_new_trunk() {
        let tmp = TempDir::new().unwrap();
        let (home, code) = shared_root(&tmp);
        std::fs::write(code.join("main/.env"), "SECRET").unwrap();
        manifest::add_share(&manifest_path(&home), "o/r", "symlink", &[".key"]).unwrap();
        std::fs::write(code.join("secret.key"), "KEY").unwrap();
        std::os::unix::fs::symlink("../secret.key", code.join("main/.key")).unwrap();

        declare_trunk(&home, "o/r", "canary");
        reconcile_one(&home, "o/r").unwrap();
        let out = crate::env::diagnose(&home, Some("o/r")).unwrap();
        assert!(
            !out.iter()
                .any(|o| o.worktree.as_deref() == Some("feat") && o.status.is_conflict()),
            "a link through the ex-trunk is grove's own: {out:?}"
        );
        assert_eq!(
            std::fs::read_link(code.join("feat/.env")).unwrap(),
            Path::new("../canary/.env")
        );
        assert_eq!(
            std::fs::read_to_string(code.join("feat/.env")).unwrap(),
            "SECRET",
            "the new trunk's source is seeded from the old one"
        );
        assert_eq!(
            std::fs::read_link(code.join("canary/.key")).unwrap(),
            Path::new("../secret.key")
        );
        assert_eq!(
            std::fs::read_to_string(code.join("feat/.key")).unwrap(),
            "KEY"
        );
    }

    /// Changing the trunk is a manifest edit and reconcile is what makes it disk: the
    /// new branch's checkout appears, `HEAD` follows it, and the branch the root used
    /// to integrate on stays exactly where it is — adopted into `worktrees.main`
    /// rather than deleted. No trunk is ever deleted by a trunk change.
    #[test]
    fn a_trunk_change_creates_the_new_checkout_and_adopts_the_old_one() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo_with_canary(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        let code = code_dir(&home, "o/r");
        assert!(
            code.join("main/README.md").is_file(),
            "trunk starts at main"
        );

        declare_trunk(&home, "o/r", "canary");
        let applied = reconcile_one(&home, "o/r").unwrap();
        assert_eq!(applied.status, ReconcileStatus::Present, "{applied:?}");

        assert!(
            code.join("canary/README.md").is_file(),
            "the declared trunk is checked out under its own branch name"
        );
        assert_eq!(
            git::default_branch(&bare_dir(&home, "o/r")).unwrap(),
            "canary",
            "HEAD carries the declaration for every reader of it"
        );
        assert!(
            code.join("main/README.md").is_file(),
            "the ex-trunk is left on disk"
        );
        let declared = declared_worktrees(&home, "o/r");
        assert!(
            declared
                .iter()
                .any(|w| w.name == "main" && w.branch == "main"),
            "…and adopted as an ordinary worktree: {declared:?}"
        );
        assert!(
            !declared.iter().any(|w| w.name == "canary"),
            "the trunk is never a declared worktree: {declared:?}"
        );
    }

    /// A trunk change is a realizer's write like a repair: one that looked before
    /// another claimed the marker meets the claim at the converge and touches nothing,
    /// and one that converges unclaimed lays a marker of its own and sheds it.
    #[test]
    fn a_trunk_change_converges_under_a_claim() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo_with_canary(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        declare_trunk(&home, "o/r", "canary");
        let manifest = std::fs::read_to_string(manifest_path(&home)).unwrap();
        let marker = root_dir(&home, "o/r").join(CLONING_MARKER);
        let held = claim_marker(&marker).unwrap();

        let err = converge_trunk(&home, "o/r", &mut None).unwrap_err();
        assert_eq!(err.to_string(), IN_FLIGHT);
        assert_eq!(
            git::default_branch(&bare_dir(&home, "o/r")).unwrap(),
            "main"
        );
        assert!(!code_dir(&home, "o/r").join("canary").exists());
        assert_eq!(
            std::fs::read_to_string(manifest_path(&home)).unwrap(),
            manifest
        );

        drop(held);
        std::fs::remove_file(&marker).unwrap();
        let applied = reconcile_one(&home, "o/r").unwrap();
        assert_eq!(applied.status, ReconcileStatus::Present, "{applied:?}");
        assert!(!marker.exists());
    }

    /// When the branch being promoted to trunk is already checked out as a declared
    /// worktree, that checkout *is* the new trunk: its `worktrees.<name>` entry is
    /// dropped, and the directory (with whatever is in it) is not touched. Leaving the
    /// entry would put grove's own integration checkout where a `tree remove` deletes
    /// it.
    #[test]
    fn a_declared_worktree_on_the_new_trunk_branch_becomes_the_trunk() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo_with_canary(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        crate::worktrees::create(&home, "o/r", "canary", "canary", None).unwrap();
        let code = code_dir(&home, "o/r");
        std::fs::write(code.join("canary/IN-FLIGHT.txt"), "not grove's").unwrap();
        assert!(
            declared_worktrees(&home, "o/r")
                .iter()
                .any(|w| w.name == "canary"),
            "precondition: canary is a declared worktree"
        );

        declare_trunk(&home, "o/r", "canary");
        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Present
        );

        let declared = declared_worktrees(&home, "o/r");
        assert!(
            !declared.iter().any(|w| w.name == "canary"),
            "the entry is dropped — that checkout is the trunk now: {declared:?}"
        );
        assert!(
            code.join("canary/IN-FLIGHT.txt").exists(),
            "the same checkout, not a fresh one beside it"
        );
        assert_eq!(
            git::default_branch(&bare_dir(&home, "o/r")).unwrap(),
            "canary"
        );
        assert!(
            declared
                .iter()
                .any(|w| w.name == "main" && w.branch == "main"),
            "and the ex-trunk is adopted: {declared:?}"
        );
    }

    /// A `trunk` naming a branch neither the bare nor origin has is a reported
    /// failure, not a branch grove invents: `git worktree add`'s `-b` fallback would
    /// mint the typo off `HEAD` and the root would integrate on it forever after.
    /// Nothing is written — HEAD, the old trunk and the manifest are as they were.
    #[test]
    fn a_trunk_naming_a_missing_branch_fails_without_minting_it() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();

        declare_trunk(&home, "o/r", "canry");
        let applied = reconcile_one(&home, "o/r").unwrap();
        assert_eq!(applied.status, ReconcileStatus::Failed, "{applied:?}");
        assert!(
            applied.error.as_deref().unwrap_or("").contains("canry"),
            "the failure names the branch: {applied:?}"
        );

        let code = code_dir(&home, "o/r");
        assert!(!code.join("canry").exists(), "no checkout, no branch");
        assert_eq!(
            git::default_branch(&bare_dir(&home, "o/r")).unwrap(),
            "main",
            "HEAD is left where it was"
        );
        assert!(code.join("main/README.md").is_file(), "so is the trunk");
        assert!(!root_dir(&home, "o/r").join(CLONING_MARKER).exists());
    }

    /// Two checkouts asking for one directory — a declared worktree named exactly for
    /// the incoming trunk's directory, but on another branch. Dropping the entry would
    /// hand the trunk a checkout of the wrong branch, so the reconcile refuses and
    /// names both sides.
    #[test]
    fn a_worktree_holding_the_trunks_directory_on_another_branch_is_refused() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        // `release/2.0` and `release-2.0` fold to the same directory name.
        crate::worktrees::create(&home, "o/r", "release-2.0", "release-2.0", None).unwrap();

        declare_trunk(&home, "o/r", "release/2.0");
        let applied = reconcile_one(&home, "o/r").unwrap();
        assert_eq!(applied.status, ReconcileStatus::Failed, "{applied:?}");
        let error = applied.error.unwrap_or_default();
        assert!(
            error.contains("release/2.0") && error.contains("release-2.0"),
            "the refusal names both sides: {error}"
        );
        assert!(
            declared_worktrees(&home, "o/r")
                .iter()
                .any(|w| w.name == "release-2.0"),
            "nobody's declaration is dropped to make room"
        );
    }

    #[test]
    fn remove_rejects_a_traversal_slug() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        assert!(remove(&home, "../../etc", Removal::Guarded).is_err());
    }

    /// An undeclared slug is a 404, never a delete. `<home>/code/<slug>` is a real
    /// directory whether or not grove put it there: the offline arm used to
    /// `remove_dir_all` any valid-shaped slug and report success, so the same typo
    /// the daemon refuses destroyed a stranger's checkout when the daemon was down.
    #[test]
    fn remove_refuses_an_undeclared_slug_and_deletes_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let mine = home.join("code/mine/notes");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(mine.join("IMPORTANT.txt"), "not grove's").unwrap();

        let err = remove(&home, "mine/notes", Removal::Guarded).unwrap_err();
        assert_eq!(err.code(), "not_declared", "{err}");
        assert!(mine.join("IMPORTANT.txt").exists(), "nothing was deleted");
        // …and a slug that names nothing at all is still a refusal, not a cheerful
        // exit 0 over a no-op.
        assert!(remove(&home, "o/typo", Removal::Guarded).is_err());
    }

    /// A hand edit can declare `o/r/bare` beside `o/r`, whose root dir IS `o/r`'s
    /// bare: the guard would survey a bare that isn't there, find nothing, and delete
    /// `o/r`'s. Refused in both modes — `--force` is never consent to a neighbour.
    #[test]
    fn remove_refuses_a_slug_nesting_with_another_declared_root() {
        let tmp = TempDir::new().unwrap();
        let home = crate::testfix::home_with_root(&tmp);
        let path = manifest_path(&home);
        let edited =
            std::fs::read_to_string(&path).unwrap() + "[roots.\"o/r/bare\"]\nurl = \"u\"\n";
        std::fs::write(&path, edited).unwrap();
        assert_eq!(root_dir(&home, "o/r/bare"), bare_dir(&home, "o/r"));

        for removal in [Removal::Guarded, Removal::Forced] {
            let err = remove(&home, "o/r/bare", removal).unwrap_err();
            assert_eq!(err.code(), "conflict", "{removal:?}: {err}");
            assert!(err.to_string().contains("declared root o/r,"), "{err}");
            assert!(
                bare_dir(&home, "o/r").exists(),
                "{removal:?} kept o/r's bare"
            );
        }
    }

    /// The same hand edit, met by the realizer: `o/r/bare`'s clone would land inside
    /// `o/r`'s bare and a failed one's cleanup would delete it. Both slugs refuse
    /// before anything is claimed, created or deleted.
    #[test]
    fn reconcile_one_refuses_a_slug_pair_that_nests_and_touches_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = crate::testfix::home_with_root(&tmp);
        let path = manifest_path(&home);
        let url = tmp.path().join("src");
        let edited = std::fs::read_to_string(&path).unwrap()
            + &format!("[roots.\"o/r/bare\"]\nurl = {:?}\n", url.to_str().unwrap());
        std::fs::write(&path, edited).unwrap();
        let bare = bare_dir(&home, "o/r");
        let snapshot = || {
            let mut names: Vec<_> = std::fs::read_dir(&bare)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            names.sort();
            let tip = git::rev_parse(&bare, "refs/heads/main").unwrap();
            (names, std::fs::read(bare.join("HEAD")).unwrap(), tip)
        };
        let before = snapshot();

        for (slug, other) in [("o/r/bare", "o/r"), ("o/r", "o/r/bare")] {
            let applied = reconcile_one(&home, slug).unwrap();
            assert_eq!(applied.status, ReconcileStatus::Failed, "{slug}");
            let error = applied.error.unwrap();
            assert!(
                error.contains(&format!("declared root {other},")),
                "{error}"
            );
        }
        assert_eq!(snapshot(), before, "o/r's bare is untouched");
        assert!(!code_dir(&home, "o/r/bare").exists(), "nothing was created");
    }

    /// `tree remove` refuses a dirty checkout (git's own guard); a root remove wipes
    /// N of them at once and must not protect less than the smaller command beside
    /// it. `--force` ([`Removal::Forced`]) is the gesture that says "I know".
    #[test]
    fn remove_refuses_a_root_holding_unsaved_work_until_forced() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        crate::worktrees::create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();
        // A tracked file, edited and not committed.
        std::fs::write(home.join("code/o/r/feat/README.md"), "in flight").unwrap();

        let err = remove(&home, "o/r", Removal::Guarded).unwrap_err();
        assert_eq!(err.code(), "conflict", "{err}");
        assert!(err.to_string().contains("feat"), "names the tree: {err}");
        assert!(home.join("code/o/r/feat/README.md").exists());

        remove(&home, "o/r", Removal::Forced).unwrap();
        assert!(!home.join("code/o/r").exists(), "--force still deletes");
        assert!(!home.join("roots/o/r").exists());
    }

    /// An untracked file is NOT the dirty signal: grove materializes declared shares
    /// as untracked entries inside every worktree, so counting them would refuse
    /// every remove on a root that declares one.
    #[test]
    fn remove_is_not_blocked_by_untracked_files() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        std::fs::write(trunk_dir(&home, "o/r").join(".env"), "SHARED=1").unwrap();

        remove(&home, "o/r", Removal::Guarded).unwrap();
        assert!(!home.join("code/o/r").exists());
        assert!(!home.join("roots/o/r").exists());
    }

    /// Deleting the bare and the code dir destroys more than the live worktrees show:
    /// a branch whose tree was removed with its commits unpushed (a bare keeps no
    /// reflog), the shared `refs/stash`, and a human's clone or notes beside the
    /// checkouts. The guard used to see none of them.
    #[test]
    fn remove_refuses_unpushed_branches_stashes_and_foreign_entries_until_forced() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        let (code, trunk) = (code_dir(&home, "o/r"), trunk_dir(&home, "o/r"));
        let commit = [
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qam",
            "w",
        ];
        crate::worktrees::create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();
        // A tag spelling the branch makes `%(refname:short)` read `heads/feature/x`.
        crate::testfix::git(&bare_dir(&home, "o/r"), &["tag", "feature/x", "HEAD"]);
        std::fs::write(code.join("feat/README.md"), "precious").unwrap();
        crate::testfix::git(&code.join("feat"), &commit);
        let refusal = || {
            remove(&home, "o/r", Removal::Guarded)
                .unwrap_err()
                .to_string()
        };
        assert!(
            refusal().contains("in feat;"),
            "a checked-out branch is its tree's"
        );

        crate::worktrees::remove(&home, "o/r", "feat").unwrap();
        std::fs::write(trunk.join("README.md"), "stashed").unwrap();
        crate::testfix::git(
            &trunk,
            &["-c", "user.name=t", "-c", "user.email=t@t", "stash"],
        );
        fixture_repo(&code.join("myfork"));
        std::fs::write(code.join("TODO.md"), "notes").unwrap();
        let r = refusal();
        assert!(
            r.contains(
                "in branch feature/x, refs/stash, TODO.md (not grove's), myfork (not grove's);"
            ),
            "{r}"
        );
        assert!(code.join("myfork/.git").exists());

        remove(&home, "o/r", Removal::Forced).unwrap();
        assert!(!code.exists() && !home.join("roots/o/r").exists());
    }

    /// Discovery reads the root directory and nothing else: a bare under `roots/` is a
    /// root whether or not anything has been checked out of it, and a code dir is not
    /// one however full it is — its entries are checkouts, and checkouts *of what* is
    /// exactly the question adoption has to answer from the bare's `origin`.
    #[test]
    fn adopt_reads_the_roots_tree_and_never_the_code_tree() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);

        // A bare with no code dir at all…
        let bare = bare_dir(&home, "o/r");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git::clone_bare(src.to_str().unwrap(), &bare).unwrap();
        // …and a code dir with no root behind it, which doctor reports as orphan code.
        std::fs::create_dir_all(code_dir(&home, "x/y").join("main")).unwrap();

        let adopted = adopt(&home).unwrap();
        assert_eq!(adopted.len(), 1, "{adopted:?}");
        assert_eq!(adopted[0].slug, "o/r");
        assert_eq!(adopted[0].status, AdoptStatus::Adopted);
        assert_eq!(
            list(&home)
                .unwrap()
                .into_iter()
                .map(|r| r.slug)
                .collect::<Vec<_>>(),
            vec!["o/r"],
            "the code dir declared nothing"
        );
    }

    /// Plant a fully-formed grove root on disk *without* touching the manifest —
    /// the shape `adopt` expects to find (bare under the root dir, trunk checkout
    /// under the code dir).
    fn plant_bare(home: &Path, slug: &str, src: &Path) {
        let bare = bare_dir(home, slug);
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        std::fs::create_dir_all(code_dir(home, slug)).unwrap();
        git::clone_bare(src.to_str().unwrap(), &bare).unwrap();
        git::worktree_add(&bare, &trunk_dir(home, slug), "main", None).unwrap();
    }

    #[test]
    fn adopt_then_reconcile_realizes_an_undeclared_on_disk_root() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        plant_bare(&home, "o/r", &src);

        assert!(
            list(&home).unwrap().is_empty(),
            "precondition: not declared"
        );

        // The Watcher's discovery half declares the bare; an engine then realizes it.
        let adopted = adopt(&home).unwrap();
        assert_eq!(adopted.len(), 1, "adoption surfaced");
        assert_eq!(adopted[0].slug, "o/r");
        assert_eq!(adopted[0].status, AdoptStatus::Adopted);

        let applied = reconcile_one(&home, "o/r").unwrap();
        // bare + trunk already on disk, so `present`.
        assert_eq!(applied.status, ReconcileStatus::Present);

        let declared = list(&home).unwrap();
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].slug, "o/r");
        // Canonicalize both sides — on macOS, `/var/...` symlinks to
        // `/private/var/...`, and gix records the canonical form.
        assert_eq!(
            std::path::Path::new(&declared[0].url)
                .canonicalize()
                .unwrap(),
            src.canonicalize().unwrap(),
            "url recovered via `git remote get-url origin`"
        );
    }

    #[test]
    fn adopt_is_idempotent_and_does_not_overwrite_a_declared_url() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        // Declare only — `roots::add` would realize it, i.e. hand this URL to gix
        // and depend on the resolver refusing it. The assertion is about the
        // *declared* string, so the URL never needs to be reached. The bare planted
        // below carries the src url, which differs from the declared one — so an
        // overwriting adopt would be visible.
        manifest::add_root(
            &manifest_path(&home),
            "o/r",
            "https://example.invalid/o/r.git",
        )
        .unwrap();
        plant_bare(&home, "o/r", &src);

        let before = std::fs::read_to_string(manifest_path(&home)).unwrap();
        adopt(&home).unwrap();
        let after = std::fs::read_to_string(manifest_path(&home)).unwrap();
        assert_eq!(before, after, "declared root is left alone by adopt");
        assert_eq!(
            list(&home).unwrap()[0].url,
            "https://example.invalid/o/r.git",
            "declared url preserved"
        );
    }

    /// Only a readable `roots/<org>/<repo>/bare` with an `origin` is adopted: dotfile
    /// dirs at either level, a file, a dir with no bare, a bare with no `origin` and an
    /// org dir that cannot be read are each skipped without failing the walk.
    #[test]
    fn adopt_skips_all_but_a_readable_bare_with_an_origin() {
        use std::os::unix::fs::PermissionsExt;
        struct Unlock(PathBuf);
        impl Drop for Unlock {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);

        // Bares under `.trash/old/r/` and `o/.old/` — the code dir beside the first is
        // what the walk would have to refuse.
        for slug in [".trash/old/r", "o/.old"] {
            let bare = bare_dir(&home, slug);
            std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
            std::fs::create_dir_all(code_dir(&home, slug)).unwrap();
            git::clone_bare(src.to_str().unwrap(), &bare).unwrap();
        }
        std::fs::write(root_dir(&home, "o/notes"), "").unwrap();
        std::fs::create_dir_all(root_dir(&home, "o/empty")).unwrap();
        std::fs::create_dir_all(bare_dir(&home, "o/local")).unwrap();
        crate::testfix::git(&bare_dir(&home, "o/local"), &["init", "-q", "--bare"]);
        let locked = home.join("roots/locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _unlock = Unlock(locked);

        assert!(adopt(&home).unwrap().is_empty());
        assert!(list(&home).unwrap().is_empty(), "nothing adoptable");
    }

    /// A bare or a trunk that is a directory but no repository fails as itself, never
    /// through git's discovery into a repo enclosing the home — the operator's `~` may
    /// be one. The enclosing repo here sits detached beside a `dev` it does not check
    /// out, its origin the src, which carries `dev` too — so every escape would succeed
    /// and show in its refs, worktrees or `HEAD`: adopt declaring its origin,
    /// sync fetching `dev` into it, a status read of it, and reconcile adding the
    /// declared `dev` trunk to it and pointing its `HEAD` there.
    #[test]
    fn a_hollow_bare_or_trunk_never_reaches_an_enclosing_repo() {
        let tmp = TempDir::new().unwrap();
        // `:` splits GIT_CEILING_DIRECTORIES into entries that fence nothing
        let (enc, home, src) = (
            tmp.path(),
            tmp.path().join("x:y/home"),
            tmp.path().join("src"),
        );
        fixture_repo(&src);
        crate::testfix::git(&src, &["branch", "dev"]);
        let id = "-c user.email=t@grove -c user.name=grove";
        for args in [
            "init -q -b dev",
            &format!("{id} commit -q --allow-empty -m ~"),
            "checkout -q --detach",
            &format!("remote add origin {}", src.display()),
        ] {
            crate::testfix::git(enc, &args.split(' ').collect::<Vec<_>>());
        }
        let snapshot = || {
            let read = |args: &str| {
                let args = args.split(' ').collect::<Vec<_>>();
                let out = git::git_command().arg("-C").arg(enc).args(args).output();
                String::from_utf8_lossy(&out.unwrap().stdout).into_owned()
            };
            ["for-each-ref", "worktree list", "symbolic-ref -q HEAD"].map(read)
        };
        let before = snapshot();

        let slug = "o/hollow";
        std::fs::create_dir_all(bare_dir(&home, slug)).unwrap();
        // a bare linked to a hollow dir deeper in the enclosing repo is fenced there too
        std::fs::create_dir_all(enc.join("deep/hollow")).unwrap();
        std::fs::create_dir_all(root_dir(&home, "o/linked")).unwrap();
        std::os::unix::fs::symlink(enc.join("deep/hollow"), bare_dir(&home, "o/linked")).unwrap();
        assert!(adopt(&home).unwrap().is_empty(), "no origin of its own");
        manifest::add_root(&manifest_path(&home), slug, src.to_str().unwrap()).unwrap();
        declare_trunk(&home, slug, "dev");
        std::fs::create_dir_all(trunk_dir(&home, slug)).unwrap();
        assert!(sync(&home, slug).is_err(), "nothing to fetch into");
        assert!(trunk_status(&home, slug).is_err(), "no checkout to read");
        std::fs::remove_dir(trunk_dir(&home, slug)).unwrap();
        let applied = reconcile_one(&home, slug).unwrap();
        assert_eq!(applied.status, ReconcileStatus::Failed, "{applied:?}");

        assert_eq!(snapshot(), before, "the enclosing repo is untouched");
    }

    /// A3: a non-UTF-8 directory name is skipped-with-report, never minted into a
    /// U+FFFD slug. Tested on the pure decision with a synthetic `OsStr` — APFS
    /// (the dev/CI filesystem) refuses invalid-UTF-8 names, so the on-disk shape
    /// cannot be constructed here.
    #[test]
    fn a_non_utf8_dir_name_is_skipped_not_minted_as_a_slug() {
        use std::os::unix::ffi::OsStrExt;
        let bad = std::ffi::OsStr::from_bytes(b"acme\xff");

        let report = slug_segment(bad, None).unwrap_err();
        assert_eq!(report.status, AdoptStatus::Skipped);
        assert!(
            report.slug.contains('\u{FFFD}'),
            "lossy rendering, eyes-only"
        );
        assert!(report.error.as_deref().unwrap_or("").contains("non-UTF-8"));

        // Repo-level: the (valid) org prefixes the report line.
        let report = slug_segment(bad, Some("acme")).unwrap_err();
        assert!(
            report.slug.starts_with("acme/"),
            "org-prefixed: {}",
            report.slug
        );

        // The happy path passes through untouched.
        assert_eq!(
            slug_segment(std::ffi::OsStr::new("widgets"), Some("acme")).unwrap(),
            "widgets"
        );
    }

    #[test]
    fn reconcile_one_clones_a_declared_missing_root_then_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        // Declare without realizing (manifest-only).
        manifest::add_root(&manifest_path(&home), "o/r", src.to_str().unwrap()).unwrap();

        let applied = reconcile_one(&home, "o/r").unwrap();
        assert_eq!(applied.status, ReconcileStatus::Cloned);
        assert_eq!(applied.default_branch.as_deref(), Some("main"));
        assert!(bare_dir(&home, "o/r").join("HEAD").exists());
        assert!(trunk_dir(&home, "o/r").join("README.md").exists());

        // Idempotent: a present root re-reconciles to `present`, no re-clone.
        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Present
        );
    }

    /// A root declared without realizing it, so the clone arm is what runs.
    fn declared_unrealized(tmp: &TempDir) -> PathBuf {
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        manifest::add_root(&manifest_path(&home), "o/r", src.to_str().unwrap()).unwrap();
        home
    }

    /// The pre-0.2 layout reads as "no bare here" to every probe the clone arm makes,
    /// so an un-guarded reconcile lays a whole second root down beside the first and
    /// leaves the operator to remove it by hand. The refusal names the remedy, and —
    /// because a legacy root is one `grove doctor --fix` away from working — writes
    /// nothing at all.
    #[test]
    fn reconcile_one_refuses_a_root_in_the_legacy_layout() {
        let tmp = TempDir::new().unwrap();
        let home = declared_unrealized(&tmp);
        let code = code_dir(&home, "o/r");
        let legacy_bare = code.join(crate::layout::LEGACY_BARE);
        let legacy_trunk = code.join(crate::layout::LEGACY_TRUNK);
        std::fs::create_dir_all(&legacy_bare).unwrap();
        std::fs::write(legacy_bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(&legacy_trunk).unwrap();

        let applied = reconcile_one(&home, "o/r").unwrap();

        assert_eq!(applied.status, ReconcileStatus::Failed);
        let error = applied.error.as_deref().unwrap();
        assert!(error.contains("legacy layout"), "{applied:?}");
        assert!(error.contains("grove doctor --fix"), "{applied:?}");
        assert!(
            !bare_dir(&home, "o/r").exists(),
            "no second root beside the legacy one"
        );
        assert!(
            legacy_bare.join("HEAD").is_file(),
            "the legacy bare is read"
        );
        assert!(legacy_trunk.is_dir(), "the legacy trunk is read");
    }

    /// The same arm would clone into any occupied root — a directory an operator laid
    /// out by hand, a root whose bare was deleted under it. What is not grove's own is
    /// somebody's, and the refusal names it rather than interleaving a clone with it.
    #[test]
    fn reconcile_one_refuses_to_clone_into_an_occupied_root() {
        let tmp = TempDir::new().unwrap();
        let home = declared_unrealized(&tmp);
        let code = code_dir(&home, "o/r");
        std::fs::create_dir_all(&code).unwrap();
        std::fs::write(code.join("notes.txt"), "mine").unwrap();

        let applied = reconcile_one(&home, "o/r").unwrap();

        assert_eq!(applied.status, ReconcileStatus::Failed);
        assert!(
            applied.error.as_deref().unwrap().contains("notes.txt"),
            "the refusal names what it found: {applied:?}"
        );
        assert!(!bare_dir(&home, "o/r").exists(), "nothing was cloned");
        assert!(code.join("notes.txt").exists());
    }

    /// The one entry a code dir tolerates. `.DS_Store` appears in any directory a Mac
    /// has looked at, nobody chose to put it there, and reading it as occupancy would
    /// leave a root nothing but a hand-delete could realize.
    #[test]
    fn reconcile_one_clones_into_a_code_dir_holding_only_a_ds_store() {
        let tmp = TempDir::new().unwrap();
        let home = declared_unrealized(&tmp);
        let code = code_dir(&home, "o/r");
        std::fs::create_dir_all(&code).unwrap();
        std::fs::write(code.join(".DS_Store"), "finder").unwrap();

        let applied = reconcile_one(&home, "o/r").unwrap();

        assert_eq!(applied.status, ReconcileStatus::Cloned, "{applied:?}");
        assert!(bare_dir(&home, "o/r").join("HEAD").exists());
        assert!(trunk_dir(&home, "o/r").join("README.md").exists());
    }

    #[test]
    fn reconcile_one_reconciles_worktrees_and_materializes_shares() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();

        // A worktree born out-of-band, plus a declared share — reconcile_one must
        // adopt the worktree and materialize the share into both worktrees.
        crate::worktrees::create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();
        manifest::add_share(&manifest_path(&home), "o/r", "symlink", &[".env"]).unwrap();

        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Present
        );
        assert!(
            trunk_dir(&home, "o/r").join(".env").exists(),
            "share source materialized in the trunk"
        );
        assert!(
            home.join("code/o/r/feat/.env").is_symlink(),
            "share linked into the adopted worktree"
        );
    }

    #[test]
    fn reconcile_one_recovers_a_partial_clone_missing_its_trunk() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();

        // Simulate a clone that crashed between the bare and the trunk: bare present,
        // trunk checkout gone, its `cloning` marker left behind unlocked.
        std::fs::remove_dir_all(trunk_dir(&home, "o/r")).unwrap();
        std::fs::write(root_dir(&home, "o/r").join(CLONING_MARKER), "").unwrap();

        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Cloned,
            "re-clones instead of stuck present"
        );
        assert!(trunk_dir(&home, "o/r").join("README.md").exists());
        assert!(
            !root_dir(&home, "o/r").join(CLONING_MARKER).exists(),
            "a whole root sheds the stale marker, or adopt would skip it forever"
        );
    }

    /// With no marker left over, the repair still runs under one: a second realizer
    /// that also found the trunk missing meets the claim instead of repairing beside it.
    #[test]
    fn realize_claims_a_trunk_repair_without_a_leftover_marker() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        std::fs::remove_dir_all(trunk_dir(&home, "o/r")).unwrap();

        let (applied, claim) = realize(&home, &declared(&home, "o/r").unwrap(), None);

        assert_eq!(applied.status, ReconcileStatus::Cloned, "{applied:?}");
        assert!(claim.is_some());
        let marker = std::fs::File::open(root_dir(&home, "o/r").join(CLONING_MARKER)).unwrap();
        assert!(matches!(
            marker.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
    }

    /// Under its claim the repair looks at the trunk again: one a racing realizer
    /// finished since the first look — here a link the claim's own marker lands — is
    /// present, and the marker it laid goes.
    #[test]
    fn realize_rechecks_the_trunk_under_its_claim() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        let marker = root_dir(&home, "o/r").join(CLONING_MARKER);
        std::fs::remove_dir_all(trunk_dir(&home, "o/r")).unwrap();
        std::os::unix::fs::symlink(&marker, trunk_dir(&home, "o/r")).unwrap();

        let (applied, claim) = realize(&home, &declared(&home, "o/r").unwrap(), None);

        assert_eq!(applied.status, ReconcileStatus::Present, "{applied:?}");
        assert!(claim.is_none() && !marker.exists());
    }

    /// A lock counts only while the marker still names its inode: one taken on a
    /// marker its holder unlinked is on a file the next realizer never sees.
    #[test]
    fn a_marker_names_only_the_inode_it_still_points_at() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join(CLONING_MARKER);
        let file = std::fs::File::create(&marker).unwrap();
        assert!(names(&marker, &file));
        std::fs::remove_file(&marker).unwrap();
        assert!(!names(&marker, &file), "unlinked");
        std::fs::write(&marker, "").unwrap();
        assert!(!names(&marker, &file), "replaced by a fresh marker");
    }

    /// One realizer, ever: a clone another process is running holds the marker's
    /// flock, and a second realizer — before the bare lands, or after gix has laid down
    /// a half-written one — touches nothing. Once the holder is gone the marker is
    /// stale and the recovery proceeds.
    #[test]
    fn reconcile_one_leaves_a_clone_in_flight_untouched() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        manifest::add_root(&manifest_path(&home), "o/r", src.to_str().unwrap()).unwrap();
        std::fs::create_dir_all(root_dir(&home, "o/r")).unwrap();
        let held = std::fs::File::create(root_dir(&home, "o/r").join(CLONING_MARKER)).unwrap();
        held.lock().unwrap();
        let bare = bare_dir(&home, "o/r");
        let in_flight = |home: &Path| {
            let out = reconcile_one(home, "o/r").unwrap();
            assert_eq!(out.status, ReconcileStatus::Failed);
            assert!(out.error.unwrap().contains("in flight"));
        };

        in_flight(&home);
        assert!(!bare.exists(), "no second clone started");
        crate::testfix::git(&home, &["init", "--bare", "-q", bare.to_str().unwrap()]);
        crate::testfix::git(&bare, &["remote", "add", "origin", src.to_str().unwrap()]);
        std::fs::write(bare.join("IN_FLIGHT"), "the holder's pack").unwrap();
        in_flight(&home);
        // A realizer that looked before the clone claimed its marker meets it at the bare.
        let (raced, _) = realize(&home, &declared(&home, "o/r").unwrap(), None);
        assert_eq!(raced.error.as_deref(), Some(IN_FLIGHT), "{raced:?}");
        assert!(
            bare.join("IN_FLIGHT").exists(),
            "the half-written bare is not wiped"
        );

        drop(held);
        assert!(
            await_clone(&home, "o/r"),
            "a free marker is a holder gone, its leftover the retry's to claim"
        );
        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Cloned
        );
        assert!(await_clone(&home, "o/r"), "a gone marker is a clone ended");
    }

    /// The clone's claim outlives the clone: `realize` hands it back still locked, for
    /// `reconcile_one` to hold through the worktree and share passes — or a realizer
    /// woken by the lock's release would run those passes in the same git beside it.
    #[test]
    fn realize_hands_back_the_clone_claim_still_held() {
        let tmp = TempDir::new().unwrap();
        let home = declared_unrealized(&tmp);
        let marker = root_dir(&home, "o/r").join(CLONING_MARKER);

        let root = declared(&home, "o/r").unwrap();

        let (applied, claim) = realize(&home, &root, None);

        assert_eq!(applied.status, ReconcileStatus::Cloned, "{applied:?}");
        assert!(claim.is_some());
        assert!(matches!(
            std::fs::File::open(&marker).unwrap().try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        // A realizer that passed `reconcile_one`'s check just before the claim was
        // taken still meets it at the clone.
        let raced = clone_and_trunk(&home, &root, &bare_dir(&home, "o/r"), None);
        assert!(raced.unwrap_err().to_string().contains("in flight"));
    }

    /// A clone that died before its bare landed leaves only the marker. The claim the
    /// reconcile took on it is the clone's own — a second claim would meet the first.
    #[test]
    fn reconcile_one_clones_under_a_crashed_clones_marker() {
        let tmp = TempDir::new().unwrap();
        let home = declared_unrealized(&tmp);
        let marker = root_dir(&home, "o/r").join(CLONING_MARKER);
        std::fs::create_dir_all(root_dir(&home, "o/r")).unwrap();
        std::fs::write(&marker, "").unwrap();

        let applied = reconcile_one(&home, "o/r").unwrap();

        assert_eq!(applied.status, ReconcileStatus::Cloned, "{applied:?}");
        assert!(!marker.exists());
    }

    /// A held marker on a whole root is another realizer's post-clone passes: this one
    /// runs none of its own beside them. Once free, the marker is a leftover, and the
    /// reconcile sheds it.
    #[test]
    fn reconcile_one_runs_no_passes_beside_a_held_marker() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        manifest::add_share(&manifest_path(&home), "o/r", "symlink", &[".env"]).unwrap();
        let marker = root_dir(&home, "o/r").join(CLONING_MARKER);
        let held = std::fs::File::create(&marker).unwrap();
        held.lock().unwrap();

        let out = reconcile_one(&home, "o/r").unwrap();
        assert_eq!(out.error.as_deref(), Some(IN_FLIGHT), "{out:?}");
        assert!(
            !trunk_dir(&home, "o/r").join(".env").exists(),
            "no share pass"
        );

        drop(held);
        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Present
        );
        assert!(trunk_dir(&home, "o/r").join(".env").exists());
        assert!(!marker.exists(), "a free marker on a whole root is shed");

        // A marker grove cannot open is no clone in flight: the refusal names the file.
        std::fs::write(&marker, "").unwrap();
        let read_only = std::os::unix::fs::PermissionsExt::from_mode(0o444);
        std::fs::set_permissions(&marker, read_only).unwrap();
        let out = reconcile_one(&home, "o/r").unwrap();
        assert!(out.error.unwrap().contains(&marker.display().to_string()));
    }

    #[test]
    fn reconcile_one_recovers_a_missing_trunk_past_the_roots_warm_pool() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();

        // A warm slot registers outside the code dir entirely, under `roots/o/r/pool`,
        // and its basename (`slot-0`) names no checkout. A basename-only exclusion read
        // grove's own disposable checkout as a user checkout and wedged recovery for
        // good — every pooled root permanently unrecoverable through grove's commands.
        crate::pool::fill(&home, "o/r").unwrap();
        assert!(home.join("roots/o/r/pool/slot-0/README.md").exists());
        std::fs::remove_dir_all(trunk_dir(&home, "o/r")).unwrap();

        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Cloned,
            "a pool slot is grove's own, not a live worktree — recovery proceeds"
        );
        assert!(trunk_dir(&home, "o/r").join("README.md").exists());
    }

    /// The recovery re-adds the trunk from the bare that is already there, and touches
    /// **nothing else under the root**. It used to `remove_dir_all` the whole root
    /// directory — a human's notes, a vendored tree, an unrelated clone with commits
    /// that exist nowhere else, deleted with no report, by a reconcile nobody asked
    /// for (declaring an *unrelated* repo is enough to trigger it).
    #[test]
    fn reconcile_one_recovers_a_missing_trunk_without_deleting_anything_else() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();

        let code = home.join("code/o/r");
        std::fs::write(code.join("NOTES.md"), "my notes").unwrap();
        std::fs::create_dir_all(code.join("sideproject")).unwrap();
        std::fs::write(code.join("sideproject/only-copy.txt"), "irreplaceable").unwrap();
        std::fs::remove_dir_all(trunk_dir(&home, "o/r")).unwrap();

        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Cloned,
            "the root is realized again"
        );
        assert!(
            trunk_dir(&home, "o/r").join("README.md").exists(),
            "the trunk checkout is back"
        );
        assert!(code.join("NOTES.md").exists(), "a human's file survives");
        assert!(code.join("sideproject/only-copy.txt").exists());
    }

    /// And when the bare is too broken to produce a worktree, a re-clone — which
    /// means deleting the root directory — happens only over grove's own entries.
    #[test]
    fn reconcile_one_refuses_to_re_clone_over_a_humans_files() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();

        let code = home.join("code/o/r");
        std::fs::remove_dir_all(trunk_dir(&home, "o/r")).unwrap();
        // A bare git still answers `worktree list` from, but carrying no refs at all —
        // so no branch resolves and the trunk cannot be re-added from it.
        let _ = std::fs::remove_file(bare_dir(&home, "o/r").join("packed-refs"));
        for entry in std::fs::read_dir(bare_dir(&home, "o/r").join("refs/heads"))
            .unwrap()
            .flatten()
        {
            let _ = std::fs::remove_file(entry.path());
        }
        std::fs::write(code.join("scratch-notes.md"), "unsaved thinking").unwrap();

        let applied = reconcile_one(&home, "o/r").unwrap();
        assert_eq!(applied.status, ReconcileStatus::Failed);
        assert!(
            applied
                .error
                .as_deref()
                .unwrap()
                .contains("scratch-notes.md"),
            "the refusal names what it would have destroyed: {applied:?}"
        );
        assert!(code.join("scratch-notes.md").exists());
        assert!(
            root_dir(&home, "o/r").join(CLONING_MARKER).exists(),
            "the refused repair's marker stays, visibly grove's"
        );

        // An empty code dir: the re-clone proceeds.
        std::fs::remove_file(code.join("scratch-notes.md")).unwrap();
        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Cloned
        );
        assert!(trunk_dir(&home, "o/r").join("README.md").exists());
    }

    /// A clone in flight (or one that died mid-transfer) writes the bare with an
    /// `origin` before a byte lands. Adopting that is how an operator's undeclare gets
    /// silently undone and the root re-cloned; the marker is what keeps discovery off
    /// it.
    #[test]
    fn adopt_skips_a_clone_still_in_flight() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        plant_bare(&home, "o/r", &src);
        std::fs::write(root_dir(&home, "o/r").join(CLONING_MARKER), "").unwrap();

        assert!(
            adopt(&home).unwrap().is_empty(),
            "a clone in flight is not a discovery"
        );
        std::fs::remove_file(root_dir(&home, "o/r").join(CLONING_MARKER)).unwrap();
        assert_eq!(
            adopt(&home).unwrap().len(),
            1,
            "and is adopted once it lands"
        );
    }

    #[test]
    fn reconcile_one_refuses_to_wipe_live_worktrees_when_trunk_is_missing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();

        crate::worktrees::create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();
        std::fs::write(home.join("code/o/r/feat/UNCOMMITTED"), "work").unwrap();
        std::fs::remove_dir_all(trunk_dir(&home, "o/r")).unwrap();

        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Failed,
            "guard holds — no wipe with live worktrees"
        );
        assert!(home.join("code/o/r/feat/UNCOMMITTED").exists());
        assert!(bare_dir(&home, "o/r").exists());
        assert!(
            !root_dir(&home, "o/r").join(CLONING_MARKER).exists(),
            "a refusal over a human's checkouts claims nothing"
        );
    }

    /// Fail safe over a bare git cannot read: with the trunk on disk its worktree pass
    /// failing is a warning, not a failed root; with the trunk gone it may still own live
    /// checkouts, so the recovery refuses rather than wipe and re-clone it.
    #[test]
    fn reconcile_one_never_wipes_a_bare_git_cannot_read() {
        let tmp = TempDir::new().unwrap();
        let home = crate::testfix::home_with_root(&tmp);
        let config = bare_dir(&home, "o/r").join("config");
        std::fs::write(&config, "[broken").unwrap();
        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Present
        );

        std::fs::remove_dir_all(trunk_dir(&home, "o/r")).unwrap();
        let applied = reconcile_one(&home, "o/r").unwrap();
        assert_eq!(applied.status, ReconcileStatus::Failed);
        assert!(applied.error.unwrap().contains("live worktrees"));
        assert_eq!(std::fs::read_to_string(config).unwrap(), "[broken");
    }

    /// Declarations colliding with the trunk: one naming its own directory and branch
    /// is the trunk itself and is dropped, moving nothing; one on its branch elsewhere
    /// cannot be checked out, which leaves it declared and the root still present.
    #[test]
    fn reconcile_one_settles_worktrees_declared_over_the_trunk() {
        let tmp = TempDir::new().unwrap();
        let home = crate::testfix::home_with_root(&tmp);
        for name in ["main", "dup"] {
            manifest::add_worktree(&manifest_path(&home), "o/r", name, "main", None).unwrap();
        }
        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Present
        );
        let declared = manifest::list_worktrees(&manifest_path(&home), "o/r").unwrap();
        assert_eq!(
            declared.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(),
            ["dup"]
        );
        assert!(!code_dir(&home, "o/r").join("dup").exists());
        assert_eq!(
            git::default_branch(&bare_dir(&home, "o/r")).unwrap(),
            "main"
        );
    }

    #[test]
    fn reconcile_one_errors_on_an_undeclared_slug() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        assert!(reconcile_one(&home, "o/r").is_err(), "not declared");
        assert!(reconcile_one(&home, "../etc").is_err(), "traversal slug");
    }

    #[test]
    fn adopt_declares_an_out_of_band_bare_without_cloning() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        // A declared root first (its `add` runs adopt over an empty `code/`), then
        // plant an out-of-band bare beside it — only the planted one is undeclared.
        add(&home, "o/already", src.to_str().unwrap()).unwrap();
        plant_bare(&home, "o/r", &src);

        let adopted = adopt(&home).unwrap();
        assert_eq!(adopted.len(), 1, "only the undeclared root is adopted");
        assert_eq!(adopted[0].slug, "o/r");
        assert_eq!(adopted[0].status, AdoptStatus::Adopted);

        let declared: std::collections::HashSet<_> =
            list(&home).unwrap().into_iter().map(|r| r.slug).collect();
        assert!(declared.contains("o/r") && declared.contains("o/already"));

        // Re-adopt is a no-op (already declared).
        assert!(adopt(&home).unwrap().is_empty());
    }

    /// The full `root.sync` contract: fetch + ff move the trunk to the remote tip,
    /// slots stranded at the old tip are recycled, and a re-sync is a no-op.
    #[test]
    fn sync_fast_forwards_the_trunk_and_prunes_stale_slots() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        // A test-local source (never the shared testfix template — sync commits to it).
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();

        // Warm a slot at the current tip, then move the remote ahead.
        crate::pool::fill(&home, "o/r").unwrap();
        std::fs::write(src.join("NEW.md"), "ahead").unwrap();
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
        crate::testfix::git(&src, &[&id[..], &["add", "."]].concat());
        crate::testfix::git(&src, &[&id[..], &["commit", "-q", "-m", "ahead"]].concat());

        let report = sync(&home, "o/r").unwrap();
        assert!(report.fetched);
        assert_eq!(report.trunk, git::FastForward::Updated);
        assert_eq!(report.stale_slots_pruned, 1, "old-tip slot recycled");
        assert_eq!(
            report.tip,
            git::rev_parse(&src, "HEAD").unwrap(),
            "trunk at the remote tip"
        );
        assert!(trunk_dir(&home, "o/r").join("NEW.md").exists());
        assert_eq!(
            crate::worktrees::pool_count(&home, "o/r").unwrap(),
            0,
            "pool emptied for the engine's refill"
        );

        // A refill now warms slots at the new tip; a re-sync prunes nothing.
        crate::pool::fill(&home, "o/r").unwrap();
        let again = sync(&home, "o/r").unwrap();
        assert_eq!(again.trunk, git::FastForward::AlreadyCurrent);
        assert_eq!(again.stale_slots_pruned, 0, "current slots kept");
        assert_eq!(crate::worktrees::pool_count(&home, "o/r").unwrap(), 1);

        // A slot git cannot read (its `.git` pointing nowhere) is skipped, not fatal.
        let slot = crate::pool::slots(&home, "o/r").unwrap().remove(0);
        std::fs::write(slot.join(".git"), "gitdir: /nonexistent").unwrap();
        assert_eq!(sync(&home, "o/r").unwrap().stale_slots_pruned, 0);
        assert!(slot.exists(), "and left where it is");
    }

    /// A diverged trunk is reported and untouched — and because the trunk didn't
    /// move, its slots still match the trunk tip and are kept.
    #[test]
    fn sync_reports_a_diverged_trunk_and_keeps_its_slots() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        crate::pool::fill(&home, "o/r").unwrap();

        let trunk = trunk_dir(&home, "o/r");
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
        std::fs::write(trunk.join("LOCAL.md"), "local").unwrap();
        crate::testfix::git(&trunk, &[&id[..], &["add", "."]].concat());
        crate::testfix::git(
            &trunk,
            &[&id[..], &["commit", "-q", "-m", "local"]].concat(),
        );
        std::fs::write(src.join("REMOTE.md"), "remote").unwrap();
        crate::testfix::git(&src, &[&id[..], &["add", "."]].concat());
        crate::testfix::git(&src, &[&id[..], &["commit", "-q", "-m", "remote"]].concat());

        let report = sync(&home, "o/r").unwrap();
        assert_eq!(report.trunk, git::FastForward::Diverged);
        assert!(
            !trunk.join("REMOTE.md").exists(),
            "diverged trunk not reset"
        );
        // The slot predates the local commit, so it IS stale relative to the
        // trunk tip — but the trunk itself never moved, and pruning only targets
        // slots that differ from the *current* trunk tip.
        assert_eq!(
            report.stale_slots_pruned, 1,
            "slot behind the trunk tip recycled"
        );
    }

    /// Sync follows the *declared* trunk, not the branch the root started on: the
    /// fetch refspec and the fast-forward both name `canary`, and `main` — still on
    /// disk as an ordinary worktree — is left exactly where it is.
    #[test]
    fn sync_fetches_and_fast_forwards_the_declared_trunk() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo_with_canary(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        declare_trunk(&home, "o/r", "canary");
        reconcile_one(&home, "o/r").unwrap();

        // Move `canary` ahead in the source and leave `main` behind it.
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
        crate::testfix::git(&src, &["checkout", "-q", "canary"]);
        std::fs::write(src.join("AHEAD.md"), "canary only").unwrap();
        crate::testfix::git(&src, &[&id[..], &["add", "."]].concat());
        crate::testfix::git(&src, &[&id[..], &["commit", "-q", "-m", "ahead"]].concat());
        crate::testfix::git(&src, &["checkout", "-q", "main"]);

        let report = sync(&home, "o/r").unwrap();
        assert_eq!(report.trunk, git::FastForward::Updated);
        assert_eq!(
            report.tip,
            git::rev_parse(&src, "canary").unwrap(),
            "the trunk sits at the remote's canary tip"
        );
        let code = code_dir(&home, "o/r");
        assert!(code.join("canary/AHEAD.md").exists());
        assert!(
            !code.join("main/AHEAD.md").exists(),
            "sync moves the trunk and nothing else"
        );
    }

    #[test]
    fn sync_errors_when_the_root_is_not_ready() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        assert!(
            sync(&home, "o/r").is_err(),
            "no bare or trunk checkout → clean error"
        );
        assert!(sync(&home, "../etc").is_err(), "traversal slug rejected");

        // A bare without its trunk checkout names the checkout as what is missing.
        let home = crate::testfix::home_with_root(&tmp);
        let trunk = trunk_dir(&home, "o/r");
        std::fs::remove_dir_all(&trunk).unwrap();
        let err = sync(&home, "o/r").unwrap_err();
        assert!(
            matches!(&err, Error::NotReady(m) if m.contains(&*trunk.to_string_lossy())),
            "{err}"
        );
    }

    #[test]
    fn remove_undeclares_and_deletes() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);

        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        remove(&home, "o/r", Removal::Guarded).unwrap();

        assert_eq!(list(&home).unwrap(), vec![]);
        assert!(!home.join("code/o/r").exists(), "no checkouts left behind");
        assert!(!home.join("roots/o/r").exists(), "and none of grove's own");

        // A root declared but never realized has nothing on disk to delete.
        manifest::add_root(&manifest_path(&home), "o/r", src.to_str().unwrap()).unwrap();
        remove(&home, "o/r", Removal::Guarded).unwrap();
        assert_eq!(list(&home).unwrap(), vec![], "undeclared all the same");
    }

    /// D4: after `remove` (delete-then-undeclare), the on-disk bare is gone, so a
    /// following `adopt` has nothing to re-find — the deleted root is not resurrected.
    #[test]
    fn remove_then_adopt_declares_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);

        add(&home, "o/r", src.to_str().unwrap()).unwrap();
        remove(&home, "o/r", Removal::Guarded).unwrap();

        assert!(
            adopt(&home).unwrap().is_empty(),
            "adopt does not re-declare the deleted root"
        );
        assert!(list(&home).unwrap().is_empty());
    }

    /// The typed `status` must not have moved the wire: `Applied` serializes exactly
    /// as it did when the field was a `String`, omit-when-None included. The fixture
    /// pins the spellings; this pins the shape they sit in.
    #[test]
    fn applied_serializes_to_the_same_wire_shape_as_before_the_typed_status() {
        assert_eq!(
            serde_json::to_value(Applied {
                slug: "o/r".into(),
                status: ReconcileStatus::Cloned,
                default_branch: Some("main".into()),
                error: None,
            })
            .unwrap(),
            serde_json::json!({"slug": "o/r", "status": "cloned", "default_branch": "main"})
        );
        assert_eq!(
            serde_json::to_value(Applied {
                slug: "o/r".into(),
                status: AdoptStatus::Skipped,
                default_branch: None,
                error: Some("non-UTF-8 directory name; cannot form a slug".into()),
            })
            .unwrap(),
            serde_json::json!({
                "slug": "o/r",
                "status": "skipped",
                "error": "non-UTF-8 directory name; cannot form a slug"
            })
        );
    }

    /// The observability half of A1.2: a `failed` recreate outcome is turned into an
    /// operator-facing stderr line (slug + worktree + git error); healthy outcomes
    /// yield nothing. Tests the surfaced *content* (the emission is a trivial loop).
    #[test]
    fn reconcile_warnings_surface_only_failed_outcomes() {
        use crate::worktrees::WorktreeOutcome;
        let report = vec![
            WorktreeOutcome {
                name: "ok".into(),
                status: crate::wire::WorktreeOutcomeStatus::Recreated,
                error: None,
            },
            WorktreeOutcome {
                name: "wedged".into(),
                status: crate::wire::WorktreeOutcomeStatus::Failed,
                error: Some("'x' is already checked out".into()),
            },
        ];
        let lines = reconcile_warnings("o/r", &report);
        assert_eq!(lines.len(), 1, "only the failed outcome surfaces");
        assert!(
            lines[0].contains("slug=o/r")
                && lines[0].contains("name=wedged")
                && lines[0].contains("already checked out"),
            "carries slug, worktree, and git error: {}",
            lines[0]
        );
        assert!(
            reconcile_warnings("o/r", &report[..1]).is_empty(),
            "a healthy report warns about nothing"
        );
    }

    /// D4: a partial `remove_dir_all` failure must leave the slug **declared** — so a
    /// half-deleted root surfaces to doctor/reconcile as broken rather than being
    /// resurrected by adopt (which the old undeclare-first ordering would allow).
    #[test]
    fn remove_keeps_the_root_declared_on_a_partial_delete() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();

        // Strip write on the root dir so its children can't be unlinked — a
        // remove_dir_all that fails partway, mid-delete.
        let dir = home.join("code/o/r");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = remove(&home, "o/r", Removal::Guarded);
        // Restore perms unconditionally so TempDir can clean up.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(err.is_err(), "the partial delete surfaces an error");
        assert!(
            list(&home).unwrap().iter().any(|r| r.slug == "o/r"),
            "slug stays declared (undeclare never ran) — broken, not resurrected"
        );
    }
}
