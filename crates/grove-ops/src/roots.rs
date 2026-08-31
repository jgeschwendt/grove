//! Root lifecycle: declare a repo in the manifest, then realize it on disk.
//! Manifest is desired state, git on disk is actual state, and reconcile drives
//! actual → desired (clone what's declared-but-missing). Two reconciler halves:
//! `adopt` (global discovery — declare undeclared bares, no clone) and
//! `reconcile_one` (per-root realize — clone + worktree reconcile + materialize
//! shares). The Watcher calls `adopt` and the per-root engines call `reconcile_one`;
//! `add` declares a root then realizes just that one via `reconcile_one`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::wire::{AdoptStatus, ReconcileStatus};
use crate::{Error, git, manifest};

#[must_use]
pub fn manifest_path(home: &Path) -> PathBuf {
    home.join("manifest.toml")
}

/// The on-disk layout, published rather than crate-private: the daemon's engine
/// derives a root's status by asking whether the bare and `.trunk` are both there,
/// and v1 duplicated exactly these three joins on the far side of its process
/// boundary (`paths.ex`) — a second copy of the layout that a rename would have
/// split silently. One process now, so one definition.
#[must_use]
pub fn root_dir(home: &Path, slug: &str) -> PathBuf {
    home.join("code").join(slug)
}

/// The root's bare clone — `<root>/.git`.
#[must_use]
pub fn bare_dir(home: &Path, slug: &str) -> PathBuf {
    root_dir(home, slug).join(".git")
}

/// The root's default-branch checkout and share source — `<root>/.trunk`.
#[must_use]
pub fn trunk_dir(home: &Path, slug: &str) -> PathBuf {
    root_dir(home, slug).join(".trunk")
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

/// The git status of a root's `.trunk` checkout — what the dashboard draws next to
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
    /// uncommitted tracked changes or unpushed commits.
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
/// **Declared first, deleted second.** The slug is looked up in the manifest before
/// any path is handed to `remove_dir_all`: `<home>/code/<slug>` is a real directory
/// whether or not grove put it there, and an offline `grove clone remove` used to
/// obliterate a valid-shaped slug that named somebody else's checkout — the same
/// typo the daemon-up path answers with a 404. Undeclared is [`Error::NotDeclared`]
/// (exit 3), on both paths.
// stele:landmark delete-before-undeclare
pub fn remove(home: &Path, slug: &str, removal: Removal) -> Result<(), Error> {
    // Validate before building a path we `remove_dir_all` — a traversal slug
    // must never reach the filesystem here.
    manifest::validate_slug(slug).map_err(Error::invalid_input)?;
    if !list(home)?.iter().any(|r| r.slug == slug) {
        return Err(Error::NotDeclared(format!("root not declared: {slug}")));
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
    let dir = root_dir(home, slug);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("remove {}", dir.display()))
            .map_err(Error::io)?;
    }
    manifest::remove_root(&manifest_path(home), slug).map_err(Error::io)
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
/// Best-effort by construction: a worktree git cannot read contributes nothing
/// rather than blocking the remove — the guard exists to catch the ordinary "I
/// forgot what was in there", not to be a lock.
fn unsaved_work(home: &Path, slug: &str) -> Vec<String> {
    let bare = bare_dir(home, slug);
    let Ok(worktrees) = git::worktree_list(&bare) else {
        return Vec::new();
    };
    worktrees
        .into_iter()
        .filter(|wt| !crate::worktrees::under_pool(home, slug, &wt.path) && wt.path.is_dir())
        .filter(|wt| {
            let dirty =
                git::status(&wt.path).is_ok_and(|s| s.staged + s.unstaged + s.conflicted > 0);
            dirty || git::unpushed_count(&wt.path).is_ok_and(|n| n > 0)
        })
        .map(|wt| {
            wt.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_owned()
        })
        .collect()
}

/// Per-root realize for one **declared** root: clone the declared-but-missing bare
/// (gix, the bounded-timeout path), reconcile that root's worktrees (two-way
/// additive), materialize its shares. Idempotent: a `present` root re-reconciles
/// to a no-op (no re-clone). The bare-but-`.trunk`-missing re-clone guard (re-clone
/// only if no other live worktrees, else `failed`) is preserved.
///
/// `Err` is reserved for not-declared / lookup faults; a clone or guard failure is
/// an `Ok(Applied{status: Failed})` so one wedged root never aborts the rest.
pub fn reconcile_one(home: &Path, slug: &str) -> Result<Applied, Error> {
    manifest::validate_slug(slug).map_err(Error::invalid_input)?;
    let root = list(home)?
        .into_iter()
        .find(|r| r.slug == slug)
        .ok_or_else(|| Error::NotDeclared(format!("root not declared: {slug}")))?;

    let applied = realize(home, &root);
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

fn realize(home: &Path, root: &manifest::Root) -> Applied {
    let bare = bare_dir(home, &root.slug);
    let trunk = trunk_dir(home, &root.slug);

    if bare.exists() && trunk.exists() {
        return present(&root.slug);
    }

    // `.trunk` is gone but the bare is here — `.trunk` vanished out-of-band (a stray
    // `rm`, a `git worktree prune`); grove never produces this state itself.
    //
    // Three answers, cheapest and least destructive first:
    //
    // 1. **Refuse** when the bare carries other live worktrees (a user's checkouts,
    //    possibly with uncommitted work). Reconcile *adds, never deletes*.
    // 2. **Re-add `.trunk`** from the bare that is already there. This is the whole
    //    of the real repair — a worktree is the only thing missing — and it touches
    //    nothing else under the root.
    // 3. Only when the bare cannot produce a worktree at all (it is itself broken)
    //    is a re-clone the answer, and a re-clone means deleting the root directory.
    //    That is done ONLY if the directory holds nothing but grove's own entries:
    //    `remove_dir_all` on a path a human also writes into is how carried law 5
    //    fails on the one code path that names it.
    if bare.exists() {
        if has_live_worktrees(home, &root.slug, &bare) {
            return Applied {
                slug: root.slug.clone(),
                status: ReconcileStatus::Failed,
                default_branch: None,
                error: Some(
                    "root has live worktrees but its `.trunk` is missing; refusing \
                     to re-clone (recreate `.trunk`, or `grove clone remove` the root)"
                        .into(),
                ),
            };
        }
        match recover_trunk(home, &root.slug, &bare) {
            Ok(branch) => {
                return Applied {
                    slug: root.slug.clone(),
                    status: ReconcileStatus::Cloned,
                    default_branch: Some(branch),
                    error: None,
                };
            }
            Err(e) => {
                let foreign = foreign_entries(&root_dir(home, &root.slug));
                if !foreign.is_empty() {
                    return Applied {
                        slug: root.slug.clone(),
                        status: ReconcileStatus::Failed,
                        default_branch: None,
                        error: Some(format!(
                            "could not recreate `.trunk` ({e:#}), and re-cloning would \
                             delete {} under the root; move it aside, or \
                             `grove clone remove` the root",
                            foreign.join(", ")
                        )),
                    };
                }
                let _ = std::fs::remove_dir_all(root_dir(home, &root.slug));
            }
        }
    }

    match clone_and_trunk(home, root, &bare) {
        Ok(branch) => Applied {
            slug: root.slug.clone(),
            status: ReconcileStatus::Cloned,
            default_branch: Some(branch),
            error: None,
        },
        Err(e) => Applied {
            slug: root.slug.clone(),
            status: ReconcileStatus::Failed,
            default_branch: None,
            error: Some(format!("{e:#}")),
        },
    }
}

fn clone_and_trunk(home: &Path, root: &manifest::Root, bare: &Path) -> Result<String> {
    let dir = root_dir(home, &root.slug);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    // A marker for the whole of the network clone, so `adopt` does not declare the
    // half-materialized bare a clone in flight has already written (gix creates
    // `<root>/.git` with its `origin` before a byte of the transfer lands). Removed
    // on both exits: a *crashed* clone leaves it behind on purpose — the leftover is
    // then visibly grove's, and undeclaring the root keeps it undeclared instead of
    // adopt resurrecting it on the next manifest event.
    let marker = dir.join(CLONING_MARKER);
    let _ = std::fs::write(&marker, "");
    let cloned = git::clone_bare(&root.url, bare).and_then(|branch| {
        git::worktree_add(bare, &trunk_dir(home, &root.slug), &branch, None).map(|()| branch)
    });
    let _ = std::fs::remove_file(&marker);
    cloned
}

/// Grove's own entries under a root directory. Everything else there was put there
/// by a human or another tool, and nothing grove does deletes it.
const OWNED: &[&str] = &[".git", ".trunk", ".pool", CLONING_MARKER];

/// Written for the length of a clone; see [`clone_and_trunk`].
const CLONING_MARKER: &str = ".grove-cloning";

/// Re-add the missing `.trunk` from the bare that is already on disk — the whole of
/// the repair when a checkout, not a repository, is what went missing.
///
/// Pruning first is what makes it work at all: a `.trunk` deleted out-of-band leaves
/// a `prunable` registration under `.git/worktrees/`, and git refuses to re-add a
/// path it still believes is registered.
fn recover_trunk(home: &Path, slug: &str, bare: &Path) -> Result<String> {
    let _ = git::worktree_prune(bare);
    let branch = git::default_branch(bare)?;
    git::worktree_add(bare, &trunk_dir(home, slug), &branch, None)?;
    Ok(branch)
}

/// Names under `dir` that are not grove's own, sorted — what a re-clone's
/// `remove_dir_all` would destroy. An unreadable directory reports none: it is about
/// to be re-created anyway, and a read error is not evidence of a human's files.
fn foreign_entries(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| !OWNED.contains(&name.as_str()))
        .collect();
    names.sort();
    names
}

/// Does `bare` have any linked worktree that is somebody's — i.e. not one of
/// grove's own disposable checkouts? Guards the destructive partial-clone
/// recovery: a user's checkout must never be wiped to recover a missing `.trunk`.
///
/// Two exclusions, and they need two different tests. `.git`/`.trunk` are
/// reserved *basenames* one level under the root, so a basename match is exact
/// for them. A warm-pool slot is registered at `<root>/.pool/slot-N`, whose
/// basename is `slot-N` — matching `.pool` by basename never fires, so slots
/// would read as user checkouts and wedge every pooled root's recovery forever.
/// They are excluded by path, through the same `worktrees::under_pool` prefix
/// test `pool::slots` counts with. Wiping them is correct: a slot is a detached
/// checkout at the trunk tip carrying no user data, the bare is re-cloned from
/// scratch so its registrations go with it, and `pool::fill` restores the
/// declared target at the new tip.
///
/// **Fail safe** — if `git worktree list` errors (a corrupt/unreadable bare, which
/// can still own live checkouts), assume worktrees may exist and refuse the wipe. A
/// genuinely partial clone has a valid bare, so this still re-clones cleanly.
// stele:landmark trunk-recovery-guard
fn has_live_worktrees(home: &Path, slug: &str, bare: &Path) -> bool {
    match git::worktree_list(bare) {
        Ok(wts) => wts.iter().any(|wt| {
            let reserved_name = wt
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::worktrees::RESERVED.contains(&n));
            !reserved_name && !crate::worktrees::under_pool(home, slug, &wt.path)
        }),
        Err(_) => true,
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
/// single-refspec), fast-forward `.trunk` onto the tracking ref, then prune any
/// warm-pool slot stranded at a pre-sync tip (a stale slot defeats the pool's
/// purpose; the engine's fill loop restores the target at the new tip). One lane
/// call = atomic per root, like `reconcile_one`. Never forces: a diverged or
/// dirty trunk is a *reported* outcome ([`SyncReport::trunk`]), not a reset.
/// Pruning compares each slot to the trunk's tip *after* the op — it recycles
/// exactly the slots a fresh `pool.fill` would no longer produce, whichever way
/// the trunk outcome went.
// stele:landmark sync-never-forces
pub fn sync(home: &Path, slug: &str) -> Result<SyncReport, Error> {
    manifest::validate_slug(slug).map_err(Error::invalid_input)?;
    let bare = bare_dir(home, slug);
    let trunk = trunk_dir(home, slug);
    if !bare.exists() || !trunk.exists() {
        return Err(Error::NotReady(format!(
            "cannot sync {slug}: root not ready (bare or .trunk missing)"
        )));
    }

    let branch = git::default_branch(&bare).map_err(Error::git)?;
    git::fetch(&bare, "origin", &branch).map_err(Error::network)?;
    let outcome = git::fast_forward(&trunk, &format!("origin/{branch}")).map_err(Error::git)?;
    let tip = git::rev_parse(&trunk, "HEAD").map_err(Error::git)?;

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
    } else if s.contains('@') && s.contains(':') {
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
/// `code/<org>/<name>/` (depth-2, dotfile dirs like `.trash` skipped at both
/// levels), reads each `.git`'s origin URL, and declares any slug not already in
/// the manifest. Idempotent and additive — never auto-deletes a declared root,
/// never overwrites a declared URL. Repos without an `origin` remote are skipped
/// (no URL to record). Returns one `Applied{status: Adopted}` per newly-declared
/// slug.
pub fn adopt(home: &Path) -> Result<Vec<Applied<AdoptStatus>>, Error> {
    let code = home.join("code");
    if !code.is_dir() {
        return Ok(vec![]);
    }

    let declared: std::collections::HashSet<String> =
        list(home)?.into_iter().map(|r| r.slug).collect();
    let manifest = manifest_path(home);
    let mut adopted = Vec::new();

    for org_entry in std::fs::read_dir(&code)
        .with_context(|| format!("read {}", code.display()))
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

            let bare = repo_entry.path().join(".git");
            if !bare.is_dir() {
                continue;
            }
            // A clone in flight (or one that died mid-transfer) has a bare with an
            // `origin` and nothing else. Adopting it re-declares a root the operator
            // may have just undeclared, and the engine then re-clones what they
            // asked to be rid of — so the marker `clone_and_trunk` writes is a
            // "not mine to discover" sign, not a lock.
            if repo_entry.path().join(CLONING_MARKER).exists() {
                continue;
            }
            // Skip silently if origin isn't configured — the bare exists but we
            // can't recover a URL to record, so adoption isn't possible.
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
        assert!(home.join("code/o/r/.git/HEAD").exists());
        assert!(home.join("code/o/r/.trunk/README.md").exists());

        // Declaring a second root realizes only it — the first is untouched.
        let other = tmp.path().join("src2");
        fixture_repo(&other);
        let applied = add(&home, "o/r2", other.to_str().unwrap()).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].slug, "o/r2");
        assert_eq!(applied[0].status, ReconcileStatus::Cloned);
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
        std::fs::write(home.join("code/o/r/.trunk/.env"), "SHARED=1").unwrap();

        remove(&home, "o/r", Removal::Guarded).unwrap();
        assert!(!home.join("code/o/r").exists());
    }

    /// Plant a fully-formed grove root on disk *without* touching the manifest —
    /// the shape `adopt` expects to find (bare at `<root>/.git`, `.trunk` worktree).
    fn plant_bare(home: &Path, slug: &str, src: &Path) {
        let bare = bare_dir(home, slug);
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
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
        // bare + .trunk already on disk, so `present`.
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

    #[test]
    fn adopt_skips_dotfile_dirs_like_trash() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);

        // A bare repo under `.trash/old/r/` — must not be adopted.
        let trash_bare = home.join("code/.trash/old/r/.git");
        std::fs::create_dir_all(trash_bare.parent().unwrap()).unwrap();
        git::clone_bare(src.to_str().unwrap(), &trash_bare).unwrap();

        adopt(&home).unwrap();
        assert!(
            list(&home).unwrap().is_empty(),
            ".trash repos must not be adopted"
        );
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
        assert!(home.join("code/o/r/.git/HEAD").exists());
        assert!(home.join("code/o/r/.trunk/README.md").exists());

        // Idempotent: a present root re-reconciles to `present`, no re-clone.
        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Present
        );
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
            home.join("code/o/r/.trunk/.env").exists(),
            "share source materialized in .trunk"
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

        // Simulate an interrupted clone: bare present, .trunk gone.
        std::fs::remove_dir_all(home.join("code/o/r/.trunk")).unwrap();
        assert!(home.join("code/o/r/.git").exists());

        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Cloned,
            "re-clones instead of stuck present"
        );
        assert!(home.join("code/o/r/.trunk/README.md").exists());
    }

    #[test]
    fn reconcile_one_recovers_a_missing_trunk_past_the_roots_warm_pool() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let src = tmp.path().join("src");
        fixture_repo(&src);
        add(&home, "o/r", src.to_str().unwrap()).unwrap();

        // A warm slot registers at `<root>/.pool/slot-0`, whose BASENAME is `slot-0`.
        // Matching the reserved `.pool` by basename never fires on it, so grove's own
        // disposable checkout used to read as a user checkout and wedge recovery for
        // good — every pooled root permanently unrecoverable through grove's commands.
        crate::pool::fill(&home, "o/r").unwrap();
        assert!(home.join("code/o/r/.pool/slot-0/README.md").exists());
        std::fs::remove_dir_all(home.join("code/o/r/.trunk")).unwrap();

        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Cloned,
            "a pool slot is grove's own, not a live worktree — recovery proceeds"
        );
        assert!(home.join("code/o/r/.trunk/README.md").exists());
    }

    /// The recovery re-adds `.trunk` from the bare that is already there, and touches
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

        let root = home.join("code/o/r");
        std::fs::write(root.join("NOTES.md"), "my notes").unwrap();
        std::fs::create_dir_all(root.join("sideproject")).unwrap();
        std::fs::write(root.join("sideproject/only-copy.txt"), "irreplaceable").unwrap();
        std::fs::remove_dir_all(root.join(".trunk")).unwrap();

        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Cloned,
            "the root is realized again"
        );
        assert!(root.join(".trunk/README.md").exists(), "`.trunk` is back");
        assert!(root.join("NOTES.md").exists(), "a human's file survives");
        assert!(root.join("sideproject/only-copy.txt").exists());
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

        let root = home.join("code/o/r");
        std::fs::remove_dir_all(root.join(".trunk")).unwrap();
        // A bare git still answers `worktree list` from, but carrying no refs at all —
        // so no branch resolves and `.trunk` cannot be re-added from it.
        let _ = std::fs::remove_file(root.join(".git/packed-refs"));
        for entry in std::fs::read_dir(root.join(".git/refs/heads"))
            .unwrap()
            .flatten()
        {
            let _ = std::fs::remove_file(entry.path());
        }
        std::fs::write(root.join("scratch-notes.md"), "unsaved thinking").unwrap();

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
        assert!(root.join("scratch-notes.md").exists());

        // Grove's own entries alone: the re-clone proceeds.
        std::fs::remove_file(root.join("scratch-notes.md")).unwrap();
        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Cloned
        );
        assert!(root.join(".trunk/README.md").exists());
    }

    /// A clone in flight (or one that died mid-transfer) writes `<root>/.git` with an
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
        std::fs::write(home.join("code/o/r").join(CLONING_MARKER), "").unwrap();

        assert!(
            adopt(&home).unwrap().is_empty(),
            "a clone in flight is not a discovery"
        );
        std::fs::remove_file(home.join("code/o/r").join(CLONING_MARKER)).unwrap();
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
        std::fs::remove_dir_all(home.join("code/o/r/.trunk")).unwrap();

        assert_eq!(
            reconcile_one(&home, "o/r").unwrap().status,
            ReconcileStatus::Failed,
            "guard holds — no wipe with live worktrees"
        );
        assert!(home.join("code/o/r/feat/UNCOMMITTED").exists());
        assert!(home.join("code/o/r/.git").exists());
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
        assert!(home.join("code/o/r/.trunk/NEW.md").exists());
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

    #[test]
    fn sync_errors_when_the_root_is_not_ready() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        assert!(sync(&home, "o/r").is_err(), "no bare/.trunk → clean error");
        assert!(sync(&home, "../etc").is_err(), "traversal slug rejected");
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
        assert!(!home.join("code/o/r").exists());
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
