//! Worktrees: a branch checked out under a root. **Declared** in the manifest (so
//! it can carry settings git can't hold) and **realized** in git. Reconcile is
//! two-way *additive* — create declared-but-missing, adopt in-git-but-undeclared
//! — and never deletes. See `docs/worktrees.html`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::roots::{bare_dir, code_dir, manifest_path};
use crate::wire::WorktreeOutcomeStatus;
use crate::{Error, git, manifest};

/// A dotted name can never be a checkout: git refuses a ref component that begins
/// with a dot, so no branch — and therefore no directory [`name_for`] derives from one
/// — is ever spelled this way. A code dir holds checkouts and only checkouts, so a
/// dotted entry there is somebody else's artefact and grove neither declares nor
/// adopts it. A predicate rather than a list, because the rule is about what git
/// permits, not about an enumerable set of names.
pub(crate) fn is_reserved(name: &str) -> bool {
    name.starts_with('.')
}

/// The directory name a branch checks out under: `feature/x` → `feature-x`.
///
/// The one branch-to-directory rule in grove. The trunk is named by it exactly as
/// every user worktree is, so the trunk directory is not a special name — it is
/// whichever checkout the manifest's `trunk` (or the bare's HEAD) points at.
#[must_use]
pub fn name_for(branch: &str) -> String {
    branch.replace('/', "-")
}

/// A worktree with both views: what the manifest declares and whether git has it.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct WorktreeStatus {
    pub name: String,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Realized in git (`git worktree list`).
    pub present: bool,
    /// Recorded in the manifest.
    pub declared: bool,
    /// The checkout's own git drift — what the dashboard draws in place of the
    /// branch name. `None` when the worktree isn't on disk (nothing to read) or
    /// when the read failed, which a wedged checkout must not turn into a failed
    /// list: same reasoning as [`crate::roots::trunk_status`].
    ///
    /// `status.branch` is the branch actually checked out, where [`Self::branch`]
    /// is the one the manifest declares — the two disagreeing is the drift the
    /// dashboard flags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<git::Status>,
}

/// One line of a [`reconcile`] report: what happened to a single worktree in the
/// additive convergence. `recreated` — a declared-but-missing checkout re-realized;
/// `adopted` — an undeclared in-git worktree written into the manifest; `failed` — a
/// recreate that git refused (surfaced, never swallowed, so a wedged worktree is
/// operator-visible rather than silently retried forever).
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct WorktreeOutcome {
    pub name: String,
    pub status: WorktreeOutcomeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn worktree_dir(home: &Path, slug: &str, name: &str) -> PathBuf {
    code_dir(home, slug).join(name)
}

/// Declare a worktree without realizing it — every user-facing declaration's one
/// door. The manifest refuses a name another branch already holds; this refuses the
/// trunk's own directory, case-folded as APFS folds it, which the manifest cannot see
/// (the trunk may be the bare's `HEAD`) and which `roots::converge_trunk` would
/// otherwise refuse on every pass.
pub fn declare(
    home: &Path,
    slug: &str,
    name: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<(), Error> {
    if let Ok(t) = crate::roots::trunk(home, slug)
        && t.name.eq_ignore_ascii_case(name)
    {
        return Err(Error::Conflict(format!(
            "`{name}` names the trunk checkout `{}` of {slug}; a worktree cannot take its \
             directory",
            t.name
        )));
    }
    manifest::add_worktree(&manifest_path(home), slug, name, branch, base).map_err(Error::io)
}

/// Declare a worktree, then realize it — **claiming a warm pool slot when one is
/// there**, and cold-checking-out when none is. Declaration persists, so a failed
/// realize is retried by `reconcile` — same contract as roots.
///
/// The pool is claimed here, in the one function every realizer reaches, rather than
/// at a caller: `grove tree add` with a daemon up declares and lets the engine's
/// reconcile do the work, offline it calls this directly, and a hand-edited manifest
/// arrives through `reconcile` — three entry points, one claim. A pool wired to only
/// one of them is a pool that pays for itself and is never redeemed.
pub fn create(
    home: &Path,
    slug: &str,
    name: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<(), Error> {
    // A branch that doesn't exist yet starts at the trunk unless the caller named a
    // base. Git's own default start point is the bare's `HEAD`, which reconcile
    // converges onto the trunk — so the two agree in the settled state and differ in
    // exactly the window that matters: between a `trunk` edit and the reconcile that
    // applies it, where `HEAD` still names the branch the root is leaving. A root
    // whose trunk cannot be resolved (no bare yet) keeps git's default.
    let trunk = crate::roots::trunk(home, slug).ok().map(|t| t.branch);
    let base = base.or(trunk.as_deref());
    // Declared before the promote, so a refused name never has a slot attached to its
    // branch and moved into place first.
    declare(home, slug, name, branch, base)?;
    if crate::pool::promote(home, slug, name, branch, base)? == crate::pool::Promotion::Promoted {
        return Ok(());
    }
    git::worktree_add(
        &bare_dir(home, slug),
        &worktree_dir(home, slug, name),
        branch,
        base,
    )
    .map_err(Error::git)?;
    // Provision the new worktree's environment (declared shares). Best-effort: a
    // share hiccup must not fail the checkout — `reconcile`/`doctor` retries it.
    let _ = crate::env::materialize(home, Some(slug), crate::env::Fix::Safe);
    Ok(())
}

/// Remove a worktree from git **then** undeclare it — git first, so a later
/// reconcile doesn't re-adopt what we just undeclared.
///
/// Only a checkout git registers at exactly this name's path is unlaid and removed.
/// A name APFS folds onto another directory — a case twin, ASCII or not, of
/// a worktree or of the trunk — resolves there, and git, matching paths by its
/// `core.ignorecase`, would remove that one: such a name is not present, and only
/// undeclares.
///
/// Undeclared is [`Error::NotDeclared`], matching the daemon route's 404 rather than
/// v1's cheerful `removed <name>` at exit 0 over a worktree that never existed.
pub fn remove(home: &Path, slug: &str, name: &str) -> Result<(), Error> {
    // Validate the slug before it becomes a filesystem path — the same chokepoint
    // roots::remove applies, since both build a path we hand to git/the FS.
    manifest::validate_slug(slug).map_err(Error::invalid_input)?;
    manifest::validate_name(name).map_err(Error::invalid_input)?;
    if !manifest::list_worktrees(&manifest_path(home), slug)
        .map_err(Error::io)?
        .iter()
        .any(|w| w.name == name)
    {
        return Err(Error::NotDeclared(format!(
            "worktree not declared: {slug}/{name}"
        )));
    }
    let bare = bare_dir(home, slug);
    let dir = worktree_dir(home, slug, name);
    if dir.exists()
        && actual(home, slug)
            .map_err(Error::git)?
            .iter()
            .any(|(n, _)| n == name)
    {
        // Never `--force`: take back grove's own shares so they can't block the
        // remove, and lay them again if git still refuses over a user's file.
        let _ = crate::env::unlay(home, slug, name);
        git::worktree_remove(&bare, &dir)
            .inspect_err(|_| {
                let _ = crate::env::materialize(home, Some(slug), crate::env::Fix::Safe);
            })
            .map_err(Error::git)?;
    } else if bare.exists() {
        // The directory was removed out-of-band (a stray `rm -rf`), but git still
        // holds a `prunable` registration under the bare's `worktrees/<name>`. Skipping
        // the git side would leave that registration alive, so the *next* reconcile
        // re-adopts the worktree we're deleting — the delete wouldn't stick. Prune
        // it so the removal is git-visible before we undeclare.
        git::worktree_prune(&bare).map_err(Error::git)?;
    }
    manifest::remove_worktree(&manifest_path(home), slug, name).map_err(Error::io)
}

/// Declared ⋈ actual — every worktree with its present/declared status. Surfaces
/// undeclared-in-git ones too, so the UI sees reality before the next adopt.
pub fn list(home: &Path, slug: &str) -> Result<Vec<WorktreeStatus>, Error> {
    let declared = manifest::list_worktrees(&manifest_path(home), slug).map_err(Error::io)?;
    let actual = actual(home, slug).map_err(Error::git)?;

    let code = code_dir(home, slug);

    let mut out: Vec<WorktreeStatus> = declared
        .iter()
        .map(|d| {
            let present = actual.iter().any(|(n, _)| n == &d.name);
            WorktreeStatus {
                name: d.name.clone(),
                branch: d.branch.clone(),
                base: d.base.clone(),
                present,
                declared: true,
                status: present.then(|| checkout_status(&code, &d.name)).flatten(),
            }
        })
        .collect();

    for (name, branch) in &actual {
        // An undeclared worktree is surfaced only when it carries a branch (an
        // adoption candidate). A detached undeclared checkout has no branch to
        // record and isn't adoptable, so it stays out of the list — a declared
        // detached one is already covered above via `present`.
        if let Some(branch) = branch
            && !declared.iter().any(|d| &d.name == name)
        {
            out.push(WorktreeStatus {
                name: name.clone(),
                branch: branch.clone(),
                base: None,
                present: true,
                declared: false,
                status: checkout_status(&code, name),
            });
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// One present worktree's git drift, or `None` if git wouldn't answer for it.
///
/// This is the one read here that costs a subprocess per worktree rather than per
/// root — `git status` is the only thing that carries ahead/behind *and* the dirty
/// counts, and it has to run inside each checkout. A read that fails is swallowed
/// to `None` (the row simply draws no pills) so one wedged worktree can't fail the
/// whole list and blank the dashboard.
fn checkout_status(code: &Path, name: &str) -> Option<git::Status> {
    git::status(&code.join(name)).ok()
}

/// The number of warm pool slots under [`crate::pool::pool_dir`] git knows about. The
/// engine reads the observed count from here to converge toward the declared
/// `pool.size`; doctor/list surface it. Counts registered worktrees whose path passes
/// under the pool (so a stray non-worktree dir doesn't inflate it).
pub fn pool_count(home: &Path, slug: &str) -> Result<usize, Error> {
    crate::pool::slots(home, slug)
        .map(|s| s.len())
        .map_err(Error::git)
}

/// Does a git-reported worktree path sit *strictly* under the root's pool?
/// Canonicalizes both sides — git returns `/private/var/…` on macOS while the
/// composed pool path is the as-declared `home/…`, so a raw `starts_with` would miss
/// every slot.
///
/// Strict is load-bearing: `starts_with` is true for the pool DIRECTORY itself, so a
/// worktree registered at the pool would count as a warm slot — and being the lowest
/// path, `pool::next_slot` would hand `promote` the pool's own parent to move. Nothing
/// grove writes can land there (a declared worktree is a single segment under the code
/// dir), which is exactly why the backstop is worth keeping: the count and the promote
/// pick both key off it.
pub(crate) fn under_pool(home: &Path, slug: &str, wt: &Path) -> bool {
    let pool = crate::pool::pool_dir(home, slug);
    let pool = pool.canonicalize().unwrap_or(pool);
    wt.canonicalize()
        .is_ok_and(|p| p != pool && p.starts_with(&pool))
}

/// Two-way additive convergence (never deletes a *live* worktree): create
/// declared-but-missing worktrees, adopt in-git-but-undeclared ones into the
/// manifest. Returns a per-worktree report of what converged.
///
/// **Prunes git-level ghosts first.** A worktree dir removed out-of-band (a stray
/// `rm -rf`) leaves a `prunable` registration that (a) makes git report it as still
/// present — masking the declared-but-missing case so the recreate never fires — and
/// (b) refuses a re-`add` at that path ("missing but already registered"). Pruning
/// before we read actual state clears both: the checkout re-realizes cleanly, and a
/// deleted-then-still-registered worktree is never mistaken for live. This deletes
/// nothing real — the working tree is already gone.
// ※ reconcile-additive
pub fn reconcile(home: &Path, slug: &str) -> Result<Vec<WorktreeOutcome>> {
    let mpath = manifest_path(home);
    let bare = bare_dir(home, slug);
    let declared = manifest::list_worktrees(&mpath, slug)?;

    // Best-effort: a prune hiccup must not abort the whole reconcile.
    if bare.exists() {
        let _ = git::worktree_prune(&bare);
    }
    let actual = actual(home, slug)?;
    let mut report = Vec::new();

    for d in &declared {
        if !actual.iter().any(|(n, _)| n == &d.name) {
            // A warm slot first, exactly as `create` does: this is the path a
            // `grove tree add` against a running daemon takes (declare, then the
            // engine's reconcile realizes), so the pool has to be claimed here or a
            // declared `pool.size` is pure disk cost. `Cold` — or a promote fault,
            // whose real cause the cold attempt reports — falls through.
            if matches!(
                crate::pool::promote(home, slug, &d.name, &d.branch, d.base.as_deref()),
                Ok(crate::pool::Promotion::Promoted)
            ) {
                report.push(WorktreeOutcome {
                    name: d.name.clone(),
                    status: WorktreeOutcomeStatus::Recreated,
                    error: None,
                });
                continue;
            }
            match git::worktree_add(
                &bare,
                &worktree_dir(home, slug, &d.name),
                &d.branch,
                d.base.as_deref(),
            ) {
                Ok(()) => report.push(WorktreeOutcome {
                    name: d.name.clone(),
                    status: WorktreeOutcomeStatus::Recreated,
                    error: None,
                }),
                // Surface the failure instead of the old `let _ =`: a recreate that
                // git refuses is reported (name + reason), not silently dropped.
                Err(e) => report.push(WorktreeOutcome {
                    name: d.name.clone(),
                    status: WorktreeOutcomeStatus::Failed,
                    error: Some(format!("{e:#}")),
                }),
            }
        }
    }

    for (name, branch) in &actual {
        // Adoption needs a branch to record, so a detached checkout is never adopted.
        // A *declared* detached worktree is left entirely alone (present above → no
        // recreate; here → not adopted); an *undeclared* detached one is skipped until
        // it carries a branch.
        let undeclared = !declared.iter().any(|d| d.name.eq_ignore_ascii_case(name));
        if let Some(branch) = branch
            && undeclared
            && manifest::validate_name(name).is_ok()
        {
            manifest::add_worktree(&mpath, slug, name, branch, None)?;
            report.push(WorktreeOutcome {
                name: name.clone(),
                status: WorktreeOutcomeStatus::Adopted,
                error: None,
            });
        }
    }

    Ok(report)
}

/// git's worktrees as `(name, branch?)` — name from the dir, restricted to checkouts
/// that sit *directly* under this root's code dir (`<code>/<name>`), excluding the
/// trunk, dotted entries, and nested paths. **A detached checkout is
/// retained** (`branch: None`): presence is independent of the branch, so a managed
/// worktree the user has detached (`git bisect`, `git switch --detach`) still reads as
/// present — callers that *adopt* (which needs a branch to record) filter `None` out
/// themselves, but the *presence* and no-recreate decisions must see it.
fn actual(home: &Path, slug: &str) -> Result<Vec<(String, Option<String>)>> {
    let bare = bare_dir(home, slug);
    if !bare.exists() {
        return Ok(Vec::new());
    }
    // git canonicalizes worktree paths (`/private/var/…` on macOS), so compare
    // against the canonical code dir too — else the prefix strip misses. Fall back to
    // the as-declared one when it isn't on disk.
    let code = code_dir(home, slug);
    let code = code.canonicalize().unwrap_or(code);
    // The trunk is a git worktree like any other, and since it is named by its branch
    // there is no longer anything in its *name* to tell it apart. Which one it is is a
    // manifest question, asked once per pass. Unresolvable (a root with no bare yet)
    // excludes nothing — there are no worktrees to classify in that state anyway.
    let trunk = crate::roots::trunk(home, slug).ok();
    let trunk = trunk.as_ref().map(|t| t.name.as_str());
    let mut out = Vec::new();
    for wt in git::worktree_list(&bare)? {
        let Some(name) = adoptable_name(&code, trunk, &wt.path) else {
            continue;
        };
        out.push((name, wt.branch));
    }
    Ok(out)
}

/// The adoptable name of a git worktree: its basename, IFF it sits *directly* under
/// `code` (`<code>/<name>`) and is neither dotted nor the trunk. Returns `None` for:
/// - an **out-of-tree** worktree (`git worktree add ~/elsewhere/x`, and every warm-pool
///   slot, which lives under `roots/<slug>/pool`) — its path doesn't strip under the
///   code dir, so it's never adopted by basename as if it were a checkout here;
/// - a **nested** path (`<code>/a/b`) — more than one segment below the code dir, so it
///   never enters the declared set *regardless of branch or detachment*;
/// - a **dotted** direct child, which no branch name can produce ([`is_reserved`]);
/// - the **trunk**, which is a checkout the root owns rather than a worktree the
///   manifest declares: adopting it would put grove's own integration checkout in
///   `worktrees.<name>`, where a `tree remove` could delete it.
fn adoptable_name(code: &Path, trunk: Option<&str>, wt: &Path) -> Option<String> {
    // git reports canonical paths; canonicalize again to match the canonical `code`,
    // falling back to the reported path if the dir was removed out-of-band.
    let canonical = wt.canonicalize();
    let wt = canonical.as_deref().unwrap_or(wt);
    let mut segs = wt.strip_prefix(code).ok()?.components();
    let name = segs.next()?.as_os_str().to_str()?;
    // A direct child only, never nested below the code dir; a `..` or `.` is dotted.
    // The trunk by directory, not spelling: on APFS git reports a twin's spelling.
    let trunk = trunk.is_some_and(|t| t == name || same_dir(&code.join(t), &code.join(name)));
    (segs.next().is_none() && !is_reserved(name) && !trunk).then(|| name.to_string())
}

/// Do two paths name one directory? A case or Unicode twin does on a folding
/// filesystem (APFS by default) and does not on a case-sensitive one, which no
/// comparison of the names alone can tell apart. Never through a symlink: a link to
/// the trunk is not the trunk's directory.
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    matches!((a.symlink_metadata(), b.symlink_metadata()),
        (Ok(a), Ok(b)) if (a.dev(), a.ino()) == (b.dev(), b.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git(cwd: &Path, args: &[&str]) {
        assert!(
            crate::git::git_command()
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap()
                .success(),
            "git {args:?}"
        );
    }

    /// A home with one cloned root `o/r` (bare + a `main` trunk checkout).
    fn home_with_root(tmp: &TempDir) -> PathBuf {
        crate::testfix::home_with_root(tmp)
    }

    /// The warm pool is claimed by the realizer, not by one caller — so a slot is
    /// redeemed whether the worktree arrives through `grove tree add` offline
    /// (`create`) or through a declaration a daemon reconciles (`reconcile`). A pool
    /// filled and never claimed is a full checkout of disk cost per slot, forever,
    /// plus a `pool n/n` readiness in the UI that means nothing.
    #[test]
    fn a_declared_worktree_claims_a_warm_slot_on_both_realizing_paths() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);

        crate::pool::fill(&home, "o/r").unwrap();
        assert_eq!(pool_count(&home, "o/r").unwrap(), 1);
        create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();
        assert!(home.join("code/o/r/feat/README.md").is_file());
        assert_eq!(
            pool_count(&home, "o/r").unwrap(),
            0,
            "`create` claimed the slot rather than cold-checking-out beside it"
        );

        // The daemon's path: declare only, then let reconcile realize it.
        crate::pool::fill(&home, "o/r").unwrap();
        crate::manifest::add_worktree(
            &manifest_path(&home),
            "o/r",
            "second",
            "feature/y",
            Some("main"),
        )
        .unwrap();
        let report = reconcile(&home, "o/r").unwrap();
        assert!(
            report
                .iter()
                .any(|o| o.name == "second" && o.status == WorktreeOutcomeStatus::Recreated),
            "{report:?}"
        );
        assert!(home.join("code/o/r/second/README.md").is_file());
        assert_eq!(
            pool_count(&home, "o/r").unwrap(),
            0,
            "reconcile claimed the slot too"
        );
    }

    /// An undeclared name is a refusal, not `removed <name>` at exit 0 over a
    /// worktree that never existed — the same answer the daemon route gives.
    #[test]
    fn remove_refuses_an_undeclared_worktree() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let err = remove(&home, "o/r", "nope").unwrap_err();
        assert_eq!(err.code(), "not_declared", "{err}");
    }

    /// A declaration of a root never realized has no checkout and no bare to prune:
    /// removing it only undeclares.
    #[test]
    fn remove_of_a_never_realized_worktree_only_undeclares() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        manifest::add_root(&manifest_path(&home), "o/r", "u").unwrap();
        manifest::add_worktree(&manifest_path(&home), "o/r", "feat", "feature/x", None).unwrap();
        remove(&home, "o/r", "feat").unwrap();
        assert_eq!(
            manifest::list_worktrees(&manifest_path(&home), "o/r").unwrap(),
            vec![]
        );
    }

    /// A root sharing `.envrc` (linked) and `local.json` (copied), neither ignored by
    /// the repo — so git sees both as untracked in every worktree — plus `feat`.
    fn home_with_shares(tmp: &TempDir) -> PathBuf {
        let home = home_with_root(tmp);
        let m = manifest_path(&home);
        std::fs::write(home.join("code/o/r/main/.envrc"), "x").unwrap();
        std::fs::write(home.join("code/o/r/main/local.json"), "{}").unwrap();
        crate::manifest::add_share(&m, "o/r", "symlink", &[".envrc"]).unwrap();
        crate::manifest::add_share(&m, "o/r", "copy", &["local.json"]).unwrap();
        create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();
        home
    }

    /// Git counts grove's own share links and seeded copies as untracked files, so the
    /// non-forced remove refused every worktree of a root sharing an unignored path.
    #[test]
    fn remove_takes_back_the_shares_grove_laid() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_shares(&tmp);
        let feat = home.join("code/o/r/feat");
        assert!(feat.join(".envrc").is_symlink() && feat.join("local.json").is_file());
        remove(&home, "o/r", "feat").unwrap();
        assert!(!feat.exists());
    }

    /// An edited copy is the user's file: git still refuses over it, it survives, and
    /// the link taken back ahead of the attempt is laid again.
    #[test]
    fn remove_still_refuses_over_an_edited_copy() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_shares(&tmp);
        let feat = home.join("code/o/r/feat");
        std::fs::write(feat.join("local.json"), "edited").unwrap();
        assert_eq!(remove(&home, "o/r", "feat").unwrap_err().code(), "git");
        assert_eq!(
            std::fs::read_to_string(feat.join("local.json")).unwrap(),
            "edited"
        );
        assert!(feat.join(".envrc").is_symlink());
    }

    /// A share of a tracked path equals the trunk's in every clean worktree, yet it is
    /// git's file, not grove's: taking it back reads as a deletion and blocks the remove.
    #[test]
    fn remove_leaves_a_tracked_share_to_git() {
        for mode in ["symlink", "copy"] {
            let tmp = TempDir::new().unwrap();
            let home = home_with_root(&tmp);
            crate::manifest::add_share(&manifest_path(&home), "o/r", mode, &["README.md"]).unwrap();
            create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();
            remove(&home, "o/r", "feat").unwrap_or_else(|e| panic!("{mode}: {e}"));
        }
    }

    /// A worktree hand-declared under the trunk's directory, exactly or as its case
    /// twin, is no checkout of its own: neither `unlay` nor `remove` takes the trunk's
    /// share sources — its real files, identical to themselves — or the trunk.
    #[test]
    fn remove_never_takes_the_trunk_or_its_sources() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_shares(&tmp);
        let main = home.join("code/o/r/main");
        for twin in ["Main", "main"] {
            manifest::add_worktree(&manifest_path(&home), "o/r", twin, "main", None).unwrap();
            let _ = crate::env::unlay(&home, "o/r", twin);
            remove(&home, "o/r", twin).unwrap();
            assert!(main.join(".envrc").is_file() && main.join("README.md").is_file());
        }
    }

    /// A name differing from a live worktree's only in case, ASCII or not,
    /// is that worktree's directory on APFS. Hand-declared past the manifest's
    /// refusal, its remove finds no checkout of its own and only undeclares: the live
    /// one's ignored, uncommitted file survives on any filesystem.
    #[test]
    fn remove_of_a_twin_leaves_the_live_worktree_alone() {
        for (live, twin) in [("\u{00C4}", "\u{00E4}"), ("Foo", "foo")] {
            let tmp = TempDir::new().unwrap();
            let home = home_with_root(&tmp);
            create(&home, "o/r", live, "live", Some("main")).unwrap();
            std::fs::write(bare_dir(&home, "o/r").join("info/exclude"), "secret.env").unwrap();
            let secret = code_dir(&home, "o/r").join(live).join("secret.env");
            std::fs::write(&secret, "s").unwrap();
            let mpath = manifest_path(&home);
            let mut doc: toml_edit::DocumentMut =
                std::fs::read_to_string(&mpath).unwrap().parse().unwrap();
            doc["roots"]["o/r"]["worktrees"][twin]["branch"] = toml_edit::value("twin");
            std::fs::write(&mpath, doc.to_string()).unwrap();
            remove(&home, "o/r", twin).unwrap();
            assert!(secret.is_file(), "{live:?}");
            assert_eq!(
                manifest::list_worktrees(&mpath, "o/r").unwrap()[0].name,
                live
            );
        }
    }

    /// Same guard as `roots::Applied`: the typed `status` leaves the wire shape
    /// untouched, `error` omitted when there is none.
    #[test]
    fn worktree_outcome_serializes_to_the_same_wire_shape_as_before() {
        assert_eq!(
            serde_json::to_value(WorktreeOutcome {
                name: "feat".into(),
                status: WorktreeOutcomeStatus::Recreated,
                error: None,
            })
            .unwrap(),
            serde_json::json!({"name": "feat", "status": "recreated"})
        );
    }

    /// Without a base, a new branch forks from the **trunk** — the branch this root
    /// integrates on. Git's own default start point is the bare's `HEAD`, which is
    /// the same commit once a `trunk` edit has been reconciled and a different one in
    /// exactly the window this test stands in: declared, not yet converged.
    #[test]
    fn create_without_a_base_forks_the_new_branch_from_the_trunk() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let code = code_dir(&home, "o/r");

        // A `canary` one commit ahead of `main`, declared as the trunk by a hand-edit
        // so the bare's HEAD still names `main`.
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
        let trunk = code.join("main");
        git(&trunk, &["checkout", "-q", "-b", "canary"]);
        std::fs::write(trunk.join("AHEAD.md"), "canary only").unwrap();
        git(&trunk, &[&id[..], &["add", "."]].concat());
        git(
            &trunk,
            &[&id[..], &["commit", "-q", "-m", "ahead"]].concat(),
        );
        git(&trunk, &["checkout", "-q", "main"]);
        let mpath = manifest_path(&home);
        let mut doc: toml_edit::DocumentMut =
            std::fs::read_to_string(&mpath).unwrap().parse().unwrap();
        doc["roots"]["o/r"]["trunk"] = toml_edit::value("canary");
        std::fs::write(&mpath, doc.to_string()).unwrap();

        create(&home, "o/r", "feat", "feature/x", None).unwrap();

        assert!(
            code.join("feat/AHEAD.md").exists(),
            "the new branch forked from the trunk, not from HEAD"
        );
        let declared = manifest::list_worktrees(&mpath, "o/r").unwrap();
        assert_eq!(
            declared
                .iter()
                .find(|w| w.name == "feat")
                .and_then(|w| w.base.as_deref()),
            Some("canary"),
            "and the base it actually used is the one declared: {declared:?}"
        );
    }

    /// A name is one directory: the trunk's is never a worktree's, and a declared
    /// one is not handed to a second branch that folds onto it — refused before the
    /// pool attaches that branch to a slot and moves it into place.
    #[test]
    fn create_refuses_the_trunks_directory_and_a_name_another_branch_holds() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        for trunk in ["main", "Main"] {
            let err = create(&home, "o/r", trunk, "x", None).unwrap_err();
            assert_eq!(err.code(), "conflict", "{err}");
        }

        let mpath = manifest_path(&home);
        manifest::add_worktree(&mpath, "o/r", "feature-x", "feature/x", None).unwrap();
        crate::pool::fill(&home, "o/r").unwrap();
        let err = create(&home, "o/r", "feature-x", "feature-x", None).unwrap_err();
        assert!(
            err.to_string()
                .contains("already declared on branch `feature/x`")
        );
        assert_eq!(
            pool_count(&home, "o/r").unwrap(),
            1,
            "the slot was left alone"
        );
        let declared = manifest::list_worktrees(&mpath, "o/r").unwrap();
        assert_eq!(
            (declared.len(), declared[0].branch.as_str()),
            (1, "feature/x")
        );
    }

    #[test]
    fn create_list_remove() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);

        create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();
        let feat = list(&home, "o/r")
            .unwrap()
            .into_iter()
            .find(|w| w.name == "feat")
            .unwrap();
        assert_eq!(feat.branch, "feature/x");
        assert!(feat.present && feat.declared);
        assert!(home.join("code/o/r/feat/README.md").exists());

        remove(&home, "o/r", "feat").unwrap();
        assert!(list(&home, "o/r").unwrap().iter().all(|w| w.name != "feat"));
        assert!(!home.join("code/o/r/feat").exists());
    }

    #[test]
    fn reconcile_adopts_an_out_of_band_worktree() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let bare = bare_dir(&home, "o/r");
        let stray = home.join("code/o/r/stray");
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-b",
                "stray",
                stray.to_str().unwrap(),
                "main",
            ],
        );

        // Visible immediately as present-but-undeclared...
        let s = list(&home, "o/r")
            .unwrap()
            .into_iter()
            .find(|w| w.name == "stray")
            .unwrap();
        assert!(s.present && !s.declared);

        // ...then reconcile writes it into the manifest.
        reconcile(&home, "o/r").unwrap();
        let declared =
            manifest::list_worktrees(&crate::roots::manifest_path(&home), "o/r").unwrap();
        assert!(
            declared
                .iter()
                .any(|w| w.name == "stray" && w.branch == "stray")
        );
    }

    #[test]
    fn a_pool_slot_is_excluded_from_list_and_reconcile_even_with_a_branch() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let bare = bare_dir(&home, "o/r");
        // A slot in the root's pool carrying a BRANCH (not detached) — a basename-only
        // exclusion would have listed/adopted it; the path-based rule must not.
        let slot = crate::pool::pool_dir(&home, "o/r").join("slot-1");
        std::fs::create_dir_all(slot.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-b",
                "slot-branch",
                slot.to_str().unwrap(),
                "main",
            ],
        );

        assert!(
            list(&home, "o/r")
                .unwrap()
                .iter()
                .all(|w| w.name != "slot-1"),
            "pool slot never listed"
        );
        reconcile(&home, "o/r").unwrap();
        let declared =
            manifest::list_worktrees(&crate::roots::manifest_path(&home), "o/r").unwrap();
        assert!(
            declared.iter().all(|w| w.name != "slot-1"),
            "pool slot never adopted"
        );
        assert_eq!(
            pool_count(&home, "o/r").unwrap(),
            1,
            "but counted as a slot"
        );
    }

    #[test]
    fn reconcile_recreates_a_declared_but_missing_worktree() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();

        // Delete the checkout from git but leave it declared.
        let bare = bare_dir(&home, "o/r");
        git(
            &bare,
            &[
                "worktree",
                "remove",
                home.join("code/o/r/feat").to_str().unwrap(),
            ],
        );
        assert!(!home.join("code/o/r/feat").exists());

        reconcile(&home, "o/r").unwrap();
        assert!(home.join("code/o/r/feat/README.md").exists(), "recreated");
    }

    /// The out-of-band twin of the test above: the dir is `rm -rf`'d *without*
    /// `git worktree remove`, so git keeps a `prunable` registration. reconcile must
    /// prune it and re-realize the declared checkout (the old `let _ =` add failed
    /// forever on a registered-but-missing path), reporting `recreated`.
    #[test]
    fn reconcile_recreates_an_out_of_band_deleted_worktree() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();

        let dir = home.join("code/o/r/feat");
        std::fs::remove_dir_all(&dir).unwrap(); // stray `rm -rf`, still registered in git
        assert!(!dir.exists());

        let report = reconcile(&home, "o/r").unwrap();
        assert!(dir.join("README.md").exists(), "recreated after prune");
        assert!(
            report
                .iter()
                .any(|o| o.name == "feat" && o.status == WorktreeOutcomeStatus::Recreated),
            "recreate surfaced in the report: {report:?}"
        );
    }

    /// `remove` on a worktree whose dir was `rm -rf`'d out-of-band must still clear
    /// git's `prunable` registration — else the entry survives and the next reconcile
    /// re-adopts the worktree we just deleted (the delete wouldn't stick).
    #[test]
    fn remove_prunes_an_out_of_band_deleted_worktree() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();

        let bare = bare_dir(&home, "o/r");
        let dir = home.join("code/o/r/feat");
        std::fs::remove_dir_all(&dir).unwrap(); // still registered in git

        remove(&home, "o/r", "feat").unwrap();

        // Undeclared, git-deregistered, and NOT resurrected by a following reconcile.
        assert!(list(&home, "o/r").unwrap().iter().all(|w| w.name != "feat"));
        assert!(
            git::worktree_list(&bare).unwrap().iter().all(|w| w
                .path
                .file_name()
                .and_then(|n| n.to_str())
                != Some("feat")),
            "git no longer lists the pruned worktree"
        );
        reconcile(&home, "o/r").unwrap();
        assert!(
            list(&home, "o/r").unwrap().iter().all(|w| w.name != "feat"),
            "reconcile does not resurrect the deleted worktree"
        );
    }

    /// A recreate git *cannot* satisfy (prune can't fix it — the branch is checked
    /// out elsewhere) must be reported as `failed` with the git error, not swallowed.
    /// This is the outcome `reconcile_one` surfaces to the operator.
    #[test]
    fn reconcile_reports_a_recreate_git_refuses() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        // `w` holds branch `shared`; declare `w2` on the SAME branch, manifest-only.
        create(&home, "o/r", "w", "shared", Some("main")).unwrap();
        manifest::add_worktree(
            &crate::roots::manifest_path(&home),
            "o/r",
            "w2",
            "shared",
            None,
        )
        .unwrap();

        // reconcile can't check out `shared` a second time → w2 recreate fails.
        let report = reconcile(&home, "o/r").unwrap();
        let failed = report
            .iter()
            .find(|o| o.name == "w2")
            .expect("w2 in the report");
        assert_eq!(failed.status, WorktreeOutcomeStatus::Failed);
        // git reworded this (older: "already checked out"; newer: "already used
        // by worktree") — accept both.
        let error = failed.error.as_deref().unwrap_or("");
        assert!(
            error.contains("already checked out") || error.contains("already used by worktree"),
            "the git error is surfaced: {failed:?}"
        );
    }

    /// A managed worktree the user detaches (`git bisect`, `git switch --detach`)
    /// must stay `present` (presence is independent of the branch) and be left
    /// entirely alone by reconcile — no recreate attempt, no `failed` outcome. The
    /// pre-fix `actual` dropped detached checkouts, so reconcile saw it as missing and
    /// re-attempted (and failed) a `git worktree add` on every pass.
    #[test]
    fn a_detached_declared_worktree_is_present_and_not_recreated() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();

        // Detach HEAD inside the managed worktree.
        let wt = home.join("code/o/r/feat");
        git(&wt, &["switch", "--detach"]);

        // Presence survives the detach: still present + declared at `list`.
        let s = list(&home, "o/r")
            .unwrap()
            .into_iter()
            .find(|w| w.name == "feat")
            .expect("declared worktree still listed");
        assert!(s.present && s.declared, "detached worktree present: {s:?}");

        // Reconcile leaves it alone — no recreate, no failed outcome for `feat`.
        let report = reconcile(&home, "o/r").unwrap();
        assert!(
            report.iter().all(|o| o.name != "feat"),
            "detached declared worktree neither recreated nor failed: {report:?}"
        );
        assert!(wt.join("README.md").exists(), "worktree left intact");
    }

    /// An out-of-tree worktree (`git worktree add ~/elsewhere/x`) shares only a
    /// basename with the root's namespace — it must never be listed or adopted as if
    /// it lived under the root.
    #[test]
    fn an_out_of_tree_worktree_is_not_adopted() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let bare = bare_dir(&home, "o/r");
        // A worktree OUTSIDE the root dir entirely.
        let outside = tmp.path().join("elsewhere/x");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-b",
                "x",
                outside.to_str().unwrap(),
                "main",
            ],
        );

        assert!(
            list(&home, "o/r").unwrap().iter().all(|w| w.name != "x"),
            "out-of-tree worktree not listed under the root"
        );
        reconcile(&home, "o/r").unwrap();
        let declared =
            manifest::list_worktrees(&crate::roots::manifest_path(&home), "o/r").unwrap();
        assert!(
            declared.iter().all(|w| w.name != "x"),
            "out-of-tree worktree not adopted into the manifest"
        );
    }

    /// The dot rule, both directions. Git refuses a ref component beginning with a
    /// dot, so "reserved" and "not a possible branch name" are the same set — which
    /// is what lets grove drop the hand-maintained list the layout used to carry.
    #[test]
    fn reserved_is_exactly_the_dotted_names() {
        for dotted in [".git", ".DS_Store", ".hidden", ".anything-later"] {
            assert!(is_reserved(dotted), "{dotted:?} is no checkout");
        }
        for checkout in ["main", "canary", "feature-x", "release-1.2"] {
            assert!(!is_reserved(checkout), "{checkout:?} is a checkout");
        }
    }

    /// One rule names every checkout, the trunk included — a branch with `/` folded
    /// to `-`, so a `feature/x` checkout never nests a directory the layout forbids.
    #[test]
    fn name_for_folds_slashes_into_dashes() {
        assert_eq!(name_for("main"), "main");
        assert_eq!(name_for("feature/x"), "feature-x");
        assert_eq!(name_for("a/b/c"), "a-b-c");
    }
}
