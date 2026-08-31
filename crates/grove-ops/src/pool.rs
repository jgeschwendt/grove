//! The warm-worktree pool: pre-checked-out slots under `<root>/.pool/` that
//! `promote` claims into user worktrees near-instantly, skipping a cold checkout.
//!
//! Two ops, both serialized on their root's per-root lane like every other
//! mutation on that root (different roots run in parallel):
//!
//! - [`fill`] adds **one** detached slot at the default-branch tip — and *nothing
//!   else*. No share materialization: a slot at `.pool/slot-N` is one level deeper
//!   than a canonical worktree, so `env::materialize`'s sibling-of-`.trunk` depth
//!   invariant doesn't hold there (links would dangle at `.pool/.trunk/…`), and the
//!   promote move would invalidate them regardless. Slots stay out of the declared
//!   set via the path-based reserved exclusion in `worktrees::actual`.
//! - [`promote`] attaches the branch IN the slot (DWIM, like `worktree_add`), then
//!   claims it via `git worktree move` (not a bare rename — that strips git's gitdir
//!   pointers), declares it, and materializes shares (now at canonical depth).
//!
//! **Crash-safety.** Attaching the branch *before* the move is what makes promote
//! convergent under interruption. Any worktree that ever reaches `<root>/<name>`
//! already carries a branch, so a moved-but-undeclared one is adopted by
//! `worktrees::reconcile` (which only sees branch-carrying worktrees) — there is no
//! window where a *detached* orphan is stranded at the user path (which reconcile
//! could neither adopt nor recreate). An attach failure (e.g. the branch is checked
//! out in another worktree) leaves the slot cleanly detached and reusable. Promote
//! never clobbers an existing `<root>/<name>`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::roots::{bare_dir, manifest_path, root_dir};
use crate::{Error, git, manifest, worktrees};

/// The outcome of a [`promote`]. `Cold` is **not** an error — the caller (the
/// engine) maps it to a cold-create fallback; the reason distinguishes an empty
/// pool from a name collision for logging/telemetry.
#[derive(Debug, PartialEq, Eq)]
pub enum Promotion {
    /// A warm slot was claimed and is now the user worktree `<name>`.
    Promoted,
    /// No promote happened; fall back to a cold `worktrees::create`.
    Cold(ColdReason),
}

/// Why a promote declined — distinct signals so the engine maps `Empty` to
/// refill-then-retry/cold vs `Conflict` to never-clobber.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ColdReason {
    /// The pool had no free slot to claim.
    Empty,
    /// `<root>/<name>` already exists — a user worktree we must never overwrite.
    Conflict,
}

/// Add one warm slot at the default-branch tip, detached. Idempotent toward a
/// target: each call adds exactly one slot, so N calls converge to N slots. Returns
/// the resulting slot count. Errors if the bare repo is missing — the engine only
/// fills a `:ready` root, so a missing bare is a real fault, not a steady state.
// stele:landmark worktree-depth
pub fn fill(home: &Path, slug: &str) -> Result<usize, Error> {
    manifest::validate_slug(slug).map_err(Error::invalid_input)?;
    let bare = bare_dir(home, slug);
    if !bare.exists() {
        return Err(Error::NotReady(format!(
            "cannot fill pool for {slug}: bare repo missing (root not ready)"
        )));
    }
    // Clear git-level ghosts first. A slot dir removed out-of-band leaves a
    // `prunable` registration: `free_slot` sees the empty path and picks it, but the
    // detached-add then fails ("missing but already registered"), wedging every fill
    // forever. Pruning drops the stale registration so the slot re-realizes cleanly
    // (and keeps `pool_count` honest). Best-effort — a prune hiccup shouldn't block.
    let _ = git::worktree_prune(&bare);
    let tip = git::default_branch(&bare).map_err(Error::git)?;
    let slot = free_slot(home, slug).map_err(Error::io)?;
    if let Some(parent) = slot.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))
            .map_err(Error::io)?;
    }
    git::worktree_add_detached(&bare, &slot, &tip).map_err(Error::git)?;
    worktrees::pool_count(home, slug)
}

/// Give one warm slot back — the other half of converging on `[pool] size`, for when
/// the declared target *drops* (or is removed entirely).
///
/// Without it `fill`'s add-only convergence is one-way: a root lowered from `size =
/// 2` to `0` kept both checkouts on disk forever, and `doctor` reported `2/0`
/// indefinitely with no command that could fix it. Returns the resulting slot count.
///
/// Non-forced, like `tree remove`: a slot is grove's own disposable checkout, but if
/// somebody has been working in one, git's refusal is the right answer and the
/// operator sees it in the fill task's outcome.
pub fn reclaim(home: &Path, slug: &str) -> Result<usize, Error> {
    manifest::validate_slug(slug).map_err(Error::invalid_input)?;
    let bare = bare_dir(home, slug);
    if !bare.exists() {
        return Ok(0);
    }
    let _ = git::worktree_prune(&bare);
    // The highest-indexed slot, so the surviving ones keep the low names `free_slot`
    // hands out and a later refill reuses the same path rather than climbing.
    if let Some(slot) = slots(home, slug).map_err(Error::git)?.last() {
        git::worktree_remove(&bare, slot).map_err(Error::git)?;
    }
    worktrees::pool_count(home, slug)
}

/// The declared warm-pool target for `slug` (`[pool] size`, default `0`). Read-only:
/// the engine reads this to derive its convergence target, keeping the manifest the
/// single source (never cached daemon-side). Lenient like the other `manifest::*` reads.
pub fn size(home: &Path, slug: &str) -> Result<u32, Error> {
    manifest::pool_size(&manifest_path(home), slug).map_err(Error::io)
}

/// A root's warm-pool state: slots on disk (`observed`) vs. the declared `target`.
/// Surfaced by `grove doctor` so an under-filled pool — a failed background refill
/// only retries on the next event — is operator-visible (the no-polling recovery
/// channel the convergence design relies on).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PoolStatus {
    pub slug: String,
    pub observed: usize,
    pub target: u32,
}

/// Pool state for one root (`Some`) or every declared root (`None`). Roots with
/// neither a target nor any slot are omitted — only pools worth reporting.
pub fn status(home: &Path, slug: Option<&str>) -> Result<Vec<PoolStatus>, Error> {
    let slugs = match slug {
        Some(s) => {
            manifest::validate_slug(s).map_err(Error::invalid_input)?;
            vec![s.to_string()]
        }
        None => crate::roots::list(home)?
            .into_iter()
            .map(|r| r.slug)
            .collect(),
    };
    let mut out = Vec::new();
    for slug in slugs {
        let target = size(home, &slug).unwrap_or(0);
        let observed = worktrees::pool_count(home, &slug).unwrap_or(0);
        if target > 0 || observed > 0 {
            out.push(PoolStatus {
                slug,
                observed,
                target,
            });
        }
    }
    Ok(out)
}

/// Claim a warm slot for the user worktree `<name>` on `branch` (off `base`/HEAD if
/// new), mirroring the cold `worktrees::create` contract. See the module doc for the
/// attach→move→declare→materialize order and its crash-convergence.
// stele:landmark promote-attach-before-move
pub fn promote(
    home: &Path,
    slug: &str,
    name: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<Promotion, Error> {
    manifest::validate_slug(slug).map_err(Error::invalid_input)?;
    manifest::validate_name(name).map_err(Error::invalid_input)?;

    let dest = root_dir(home, slug).join(name);
    if dest.exists() {
        return Ok(Promotion::Cold(ColdReason::Conflict)); // never clobber a user worktree
    }
    let Some(slot) = next_slot(home, slug).map_err(Error::git)? else {
        return Ok(Promotion::Cold(ColdReason::Empty));
    };

    // Attach the branch IN the slot, BEFORE the move. An interrupted promote (attach
    // fails, or a crash) must never leave a *detached* tree at `<root>/<name>` —
    // reconcile drops detached worktrees, so such an orphan is neither adopted nor
    // recreatable and would wedge the name. Attaching first guarantees any tree that
    // reaches the user path carries a branch (⇒ adoptable); a failed attach leaves the
    // slot cleanly detached and reusable, and the caller cold-falls-back.
    git::attach_branch(&slot, branch, base).map_err(Error::git)?;
    git::worktree_move(&bare_dir(home, slug), &slot, &dest).map_err(Error::git)?;
    manifest::add_worktree(&manifest_path(home), slug, name, branch, base).map_err(Error::io)?;
    // The worktree now sits at canonical sibling-of-`.trunk` depth, so share targets
    // resolve correctly. Best-effort, like the cold path — a share hiccup must not
    // fail an otherwise-complete promote; reconcile/doctor retries it.
    let _ = crate::env::materialize(home, Some(slug), crate::env::Fix::Safe);
    Ok(Promotion::Promoted)
}

/// The registered slot worktrees under `<root>/.pool/`, sorted by path. `pub(crate)`
/// for `roots::sync`, which prunes slots stranded at a pre-sync tip.
pub(crate) fn slots(home: &Path, slug: &str) -> Result<Vec<PathBuf>> {
    let bare = bare_dir(home, slug);
    if !bare.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<PathBuf> = git::worktree_list(&bare)?
        .into_iter()
        .map(|wt| wt.path)
        .filter(|p| worktrees::under_pool(home, slug, p))
        .collect();
    out.sort();
    Ok(out)
}

/// The slot a promote claims — deterministic (lowest by path), or `None` when empty.
fn next_slot(home: &Path, slug: &str) -> Result<Option<PathBuf>> {
    Ok(slots(home, slug)?.into_iter().next())
}

/// The lowest-index free `<root>/.pool/slot-<n>` path — collision-free and stable,
/// so a crashed-then-retried fill reuses the same name rather than racing ahead.
fn free_slot(home: &Path, slug: &str) -> Result<PathBuf> {
    let pool = root_dir(home, slug).join(".pool");
    for n in 0.. {
        let cand = pool.join(format!("slot-{n}"));
        if !cand.exists() {
            return Ok(cand);
        }
    }
    unreachable!("0.. is unbounded")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfix::git;
    use tempfile::TempDir;

    /// A home with one cloned root `o/r` (bare + `.trunk` on `main`).
    fn home_with_root(tmp: &TempDir) -> PathBuf {
        crate::testfix::home_with_root(tmp)
    }

    fn root(home: &Path) -> PathBuf {
        home.join("code/o/r")
    }

    #[test]
    fn fill_creates_a_detached_slot_at_head_and_is_idempotent_toward_n() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);

        assert_eq!(fill(&home, "o/r").unwrap(), 1, "first slot");
        assert_eq!(
            fill(&home, "o/r").unwrap(),
            2,
            "second slot — converges to N"
        );

        // Slots exist, are checked out at the tip, and are DETACHED (no branch).
        assert!(root(&home).join(".pool/slot-0/README.md").exists());
        assert!(root(&home).join(".pool/slot-1/README.md").exists());
        let bare = home.join("code/o/r/.git");
        let detached = crate::git::worktree_list(&bare)
            .unwrap()
            .into_iter()
            .filter(|w| w.path.starts_with(root(&home).join(".pool")))
            .all(|w| w.branch.is_none());
        assert!(detached, "every slot is detached");
    }

    /// Convergence runs both ways. `fill` alone is add-only, so lowering `pool.size`
    /// (or removing the section) left the slots on disk forever and `doctor`
    /// reporting `2/0` with no command that could fix it.
    #[test]
    fn reclaim_gives_a_slot_back_so_a_lowered_target_converges() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        fill(&home, "o/r").unwrap();
        assert_eq!(fill(&home, "o/r").unwrap(), 2);

        assert_eq!(reclaim(&home, "o/r").unwrap(), 1, "the highest slot goes");
        assert!(root(&home).join(".pool/slot-0").exists());
        assert!(!root(&home).join(".pool/slot-1").exists());
        assert_eq!(reclaim(&home, "o/r").unwrap(), 0);
        assert_eq!(reclaim(&home, "o/r").unwrap(), 0, "empty is a no-op");
    }

    #[test]
    fn fill_materializes_no_shares_in_slots() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        manifest::add_share(&manifest_path(&home), "o/r", "symlink", &[".env"]).unwrap();
        crate::env::materialize(&home, Some("o/r"), crate::env::Fix::Safe).unwrap();

        fill(&home, "o/r").unwrap();
        // The depth invariant: NO share link in the slot (it would dangle).
        assert!(
            !root(&home).join(".pool/slot-0/.env").exists(),
            "no share link warmed into a slot"
        );
    }

    /// A slot dir removed out-of-band leaves a `prunable` registration. Without the
    /// prune-first in `fill`, `free_slot` re-picks that path and the detached-add
    /// fails ("missing but already registered"), wedging every subsequent fill. The
    /// fix must prune the ghost and refill cleanly.
    #[test]
    fn fill_refills_after_a_slot_dir_is_deleted_out_of_band() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        fill(&home, "o/r").unwrap();
        let slot = root(&home).join(".pool/slot-0");
        assert!(slot.exists());

        std::fs::remove_dir_all(&slot).unwrap(); // stray `rm -rf`, still registered
        assert_eq!(
            worktrees::pool_count(&home, "o/r").unwrap(),
            0,
            "the ghost slot isn't counted"
        );

        let count = fill(&home, "o/r").unwrap();
        assert!(
            slot.join("README.md").exists(),
            "slot re-realized after prune"
        );
        assert_eq!(count, 1, "pool refilled");
    }

    #[test]
    fn fill_errors_when_the_bare_is_missing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        assert!(fill(&home, "o/r").is_err(), "not ready → clean error");
    }

    #[test]
    fn promote_claims_a_slot_moves_attaches_declares_and_links() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        manifest::add_share(&manifest_path(&home), "o/r", "symlink", &[".env"]).unwrap();
        crate::env::materialize(&home, Some("o/r"), crate::env::Fix::Safe).unwrap();
        fill(&home, "o/r").unwrap();

        assert_eq!(
            promote(&home, "o/r", "feat", "feature/x", Some("main")).unwrap(),
            Promotion::Promoted
        );

        // Moved to <root>/feat, declared, slot consumed.
        assert!(root(&home).join("feat/README.md").exists());
        assert!(!root(&home).join(".pool/slot-0").exists(), "slot consumed");
        assert_eq!(pool_count_(&home), 0);
        let declared = manifest::list_worktrees(&manifest_path(&home), "o/r").unwrap();
        assert!(
            declared
                .iter()
                .any(|w| w.name == "feat" && w.branch == "feature/x")
        );

        // The branch is attached and the share is materialized at correct depth.
        let bare = home.join("code/o/r/.git");
        let on = crate::git::worktree_list(&bare)
            .unwrap()
            .into_iter()
            .find(|w| {
                w.path.canonicalize().unwrap() == root(&home).join("feat").canonicalize().unwrap()
            })
            .unwrap();
        assert_eq!(on.branch.as_deref(), Some("feature/x"));
        let link = root(&home).join("feat/.env");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../.trunk/.env")
        );
        std::fs::write(root(&home).join(".trunk/.env"), "SECRET").unwrap();
        assert_eq!(
            std::fs::read_to_string(&link).unwrap(),
            "SECRET",
            "link resolves"
        );
    }

    fn pool_count_(home: &Path) -> usize {
        worktrees::pool_count(home, "o/r").unwrap()
    }

    /// A worktree named `.pool` used to be declarable, and it destroyed the warm
    /// pool: `under_pool` counted the pool DIRECTORY as a slot, `next_slot` sorted
    /// it ahead of `.pool/slot-0`, and promote's `git worktree move` carried the
    /// whole pool — every slot inside it — into the user's new worktree. Both gates
    /// are pinned: the name is refused at the API *and* at the hand-edited-manifest
    /// path (what the watcher exists to serve), and the pool dir is never a slot.
    #[test]
    fn a_reserved_pool_name_can_never_claim_the_warm_pool() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let mpath = manifest_path(&home);

        // The API refuses it outright.
        assert!(
            crate::worktrees::create(&home, "o/r", ".pool", "feature/x", Some("main")).is_err(),
            "`.pool` is not a declarable worktree name"
        );

        // A hand-edited declaration is skipped by the reader, so reconcile never
        // realizes a worktree at `<root>/.pool`.
        let doc = std::fs::read_to_string(&mpath).unwrap();
        std::fs::write(
            &mpath,
            format!("{doc}\n[roots.\"o/r\".worktrees.\".pool\"]\nbranch = \"feature/x\"\n"),
        )
        .unwrap();
        assert!(
            manifest::list_worktrees(&mpath, "o/r")
                .unwrap()
                .iter()
                .all(|w| w.name != ".pool"),
            "a reserved name never reaches the reconciler"
        );
        worktrees::reconcile(&home, "o/r").unwrap();

        fill(&home, "o/r").unwrap();
        fill(&home, "o/r").unwrap();
        assert_eq!(pool_count_(&home), 2, "two real slots, no phantom");
        assert!(
            !worktrees::under_pool(&home, "o/r", &root(&home).join(".pool")),
            "the pool directory is not itself a slot"
        );

        assert_eq!(
            promote(&home, "o/r", "mine", "feature/x", Some("main")).unwrap(),
            Promotion::Promoted
        );
        assert!(
            root(&home).join(".pool/slot-1").is_dir(),
            "the warm pool survives the promote"
        );
        assert_eq!(pool_count_(&home), 1, "exactly one slot was claimed");
        assert!(root(&home).join("mine/README.md").exists());
        assert!(
            !root(&home).join("mine/slot-1").exists(),
            "no slot was carried inside the user worktree"
        );
    }

    #[test]
    fn size_reads_the_declared_target_default_zero() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        assert_eq!(size(&home, "o/r").unwrap(), 0, "opt-in default");
        manifest::set_pool_size(&manifest_path(&home), "o/r", 3).unwrap();
        assert_eq!(size(&home, "o/r").unwrap(), 3);
    }

    #[test]
    fn promote_of_an_existing_branch_matches_the_cold_path() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        // A branch that lives only on the remote — the cold path DWIMs it into a
        // tracking branch; promote must do the same (no bare `-b`).
        let bare = home.join("code/o/r/.git");
        git(
            &bare,
            &["update-ref", "refs/remotes/origin/remote-only", "HEAD"],
        );
        fill(&home, "o/r").unwrap();

        promote(&home, "o/r", "ro", "remote-only", None).unwrap();
        let upstream = String::from_utf8(
            crate::git::git_command()
                .arg("-C")
                .arg(root(&home).join("ro"))
                .args(["rev-parse", "--abbrev-ref", "remote-only@{upstream}"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(upstream.trim(), "origin/remote-only", "warm == cold DWIM");
    }

    #[test]
    fn promote_on_an_empty_pool_signals_cold_empty() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        assert_eq!(
            promote(&home, "o/r", "feat", "feature/x", Some("main")).unwrap(),
            Promotion::Cold(ColdReason::Empty)
        );
        assert!(!root(&home).join("feat").exists(), "no worktree created");
    }

    #[test]
    fn promote_attach_failure_strands_no_orphan_and_keeps_the_slot() {
        // Attaching the branch BEFORE the move means an attach failure can't leave a
        // detached orphan at the user path. Force the failure by promoting a branch
        // already checked out in another worktree (git refuses a second checkout).
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        crate::worktrees::create(&home, "o/r", "held", "taken", Some("main")).unwrap();
        fill(&home, "o/r").unwrap();

        let err = promote(&home, "o/r", "feat", "taken", Some("main"));
        assert!(err.is_err(), "attach of an in-use branch fails");

        // No orphan at the user path, and the warm slot survives intact + detached.
        assert!(!root(&home).join("feat").exists(), "no stranded worktree");
        assert_eq!(pool_count_(&home), 1, "slot not consumed on attach failure");
        let bare = home.join("code/o/r/.git");
        assert!(
            crate::git::worktree_list(&bare)
                .unwrap()
                .into_iter()
                .filter(|w| w.path.starts_with(root(&home).join(".pool")))
                .all(|w| w.branch.is_none()),
            "the slot is still detached and reusable"
        );

        // The name is NOT wedged: a fresh cold create of <feat> still works.
        crate::worktrees::create(&home, "o/r", "feat", "feature/y", Some("main")).unwrap();
        assert!(root(&home).join("feat/README.md").exists());
    }

    #[test]
    fn status_reports_observed_vs_target_per_pool() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        manifest::set_pool_size(&manifest_path(&home), "o/r", 2).unwrap();
        fill(&home, "o/r").unwrap();

        let s = status(&home, Some("o/r")).unwrap();
        assert_eq!(
            s,
            vec![PoolStatus {
                slug: "o/r".into(),
                observed: 1,
                target: 2
            }]
        );
        // A root with neither target nor slots is omitted.
        assert!(status(&home, None).unwrap().iter().all(|p| p.slug == "o/r"));
    }

    #[test]
    fn promote_never_clobbers_an_existing_worktree() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        crate::worktrees::create(&home, "o/r", "feat", "feature/x", Some("main")).unwrap();
        std::fs::write(root(&home).join("feat/UNCOMMITTED"), "work").unwrap();
        fill(&home, "o/r").unwrap();

        assert_eq!(
            promote(&home, "o/r", "feat", "other", Some("main")).unwrap(),
            Promotion::Cold(ColdReason::Conflict)
        );
        // The existing worktree and its uncommitted work are untouched; the slot stays.
        assert_eq!(
            std::fs::read_to_string(root(&home).join("feat/UNCOMMITTED")).unwrap(),
            "work"
        );
        assert_eq!(pool_count_(&home), 1, "slot not consumed on conflict");
    }
}
