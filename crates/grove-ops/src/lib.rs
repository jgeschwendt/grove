//! grove-ops — synchronous git/repository + manifest operations for grove.
//!
//! The local mutation layer, and a library only: the CLI, the daemon's per-root
//! lanes, and the offline realizer all call it in-process. The authority is the
//! files — `manifest.toml` (desired state) and git on disk (actual state); this
//! crate is how anyone with local filesystem access changes them.

pub mod clock;
pub mod doctor;
pub mod env;
mod error;
pub mod git;
pub mod layout;
pub mod manifest;
pub mod pool;
pub mod roots;
pub mod telemetry;
// Shared git/home fixtures. `test-util` publishes them to OTHER crates' tests (the
// daemon's HTTP suite needs a real cloned root to remove); the feature is off by
// default and belongs under `[dev-dependencies]` only, so they cannot link into the
// shipped binary.
#[cfg(any(test, feature = "test-util"))]
pub mod testfix;
pub mod wire;
pub mod worktrees;

pub use error::{Error, Result};

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The data dir every grove process reads and writes: `GROVE_HOME`, else
/// `$HOME/.grove`, else `./.grove`.
///
/// **One resolution, called by all three entry points** — the CLI's dispatch, the
/// launcher's [`ServerControl`](../grove/struct.ServerControl.html), and the daemon's
/// `Config::from_env`. It lives in the crate that owns the on-disk layout beside
/// [`roots::root_dir`], because a second spelling of this rule is a split brain
/// between what `grove serve` realizes, what `grove on` pid-files, and what the
/// offline realizer writes — and nothing would fail while the copies agreed.
#[must_use]
pub fn home() -> PathBuf {
    std::env::var_os("GROVE_HOME").map_or_else(
        || {
            let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
            PathBuf::from(home).join(".grove")
        },
        PathBuf::from,
    )
}

/// The install root every grove process launches from and `grove up` flips:
/// `GROVE_INSTALL`, else `$XDG_DATA_HOME/grove`, else `$HOME/.local/share/grove`,
/// else `./.grove-install`.
///
/// Holds `versions/`, `current`, `previous`, `channel`, `pending` and
/// `update.lock` — disposable, regenerable, owned by the updater, and knowing
/// nothing about repos. That is the opposite lifecycle from the workspace [`home`]
/// names, which is why these are two roots under two knobs rather than one root
/// that `uninstall.sh` cannot safely delete.
///
/// **One resolution, called by all three entry points** — for the same reason
/// [`home`] is one: a second spelling is a split brain between what `grove up`
/// flips, what the launcher execs, and what the installer symlinks, and nothing
/// would fail while the copies agreed.
///
/// A var set to the empty string counts as *set*, mirroring [`home`]'s reading of
/// `GROVE_HOME`: both take the presence of the name, not the shape of its value,
/// as the operator having spoken.
#[must_use]
pub fn install_home() -> PathBuf {
    install_home_from(std::env::var_os)
}

/// [`install_home`]'s rungs over an injected lookup, so each rung is testable
/// without touching the process environment — this workspace forbids `unsafe` and
/// edition 2024's `std::env::set_var` is an `unsafe fn`, so no test here can set a
/// var, serialized behind a mutex or not.
// Keys are literals at every call site, so `&'static str` is the honest bound —
// and the one `std::env::var_os` can satisfy: as a generic fn item it implements
// `Fn` at one instantiated lifetime, never for all of them.
fn install_home_from(var: impl Fn(&'static str) -> Option<OsString>) -> PathBuf {
    if let Some(install) = var("GROVE_INSTALL") {
        return PathBuf::from(install);
    }
    if let Some(xdg) = var("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("grove");
    }
    if let Some(home) = var("HOME") {
        return PathBuf::from(home).join(".local/share/grove");
    }
    PathBuf::from("./.grove-install")
}

/// Each rung of [`install_home`], and the precedence between them.
///
/// Separate from the ops-scenario module below because it needs none of its git
/// fixtures: the resolution is pure, and its only dependency — the environment —
/// arrives as an argument.
#[cfg(test)]
mod install_home_tests {
    use super::install_home_from;
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// A lookup over a fixed table — what `std::env::var_os` would answer, with no
    /// process-global state for a parallel test to race.
    fn env(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&'static str) -> Option<OsString> {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    /// Rung 1 wins outright: an operator naming the install root is not second-
    /// guessed by an XDG or HOME that also happens to be set.
    #[test]
    fn install_home_prefers_grove_install() {
        let resolved = install_home_from(env(&[
            ("GROVE_INSTALL", "/opt/grove"),
            ("XDG_DATA_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ]));
        assert_eq!(resolved, PathBuf::from("/opt/grove"));
    }

    /// Rung 2: XDG owns the dir, grove owns a named subdir of it — never the dir
    /// itself, which belongs to every other XDG-respecting tool on the box.
    #[test]
    fn install_home_falls_back_to_xdg_data_home() {
        let resolved = install_home_from(env(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/u")]));
        assert_eq!(resolved, PathBuf::from("/xdg/grove"));
    }

    /// Rung 3 spells out the XDG default rather than deriving it, so a box with no
    /// `XDG_DATA_HOME` still lands where an XDG-respecting one does.
    #[test]
    fn install_home_falls_back_to_home() {
        let resolved = install_home_from(env(&[("HOME", "/home/u")]));
        assert_eq!(resolved, PathBuf::from("/home/u/.local/share/grove"));
    }

    /// Rung 4: with no environment at all the root is cwd-relative, so a test or a
    /// sandbox gets a scratch install instead of a path rooted at `/`.
    #[test]
    fn install_home_falls_back_to_cwd() {
        assert_eq!(
            install_home_from(env(&[])),
            PathBuf::from("./.grove-install")
        );
    }

    /// The empty string is *set*, matching `home`'s reading of `GROVE_HOME`. Locked
    /// down because the friendlier reading — empty means unset — would silently
    /// route an operator's typo to a different root than the one they exported.
    #[test]
    fn install_home_treats_an_empty_var_as_set() {
        let resolved = install_home_from(env(&[("GROVE_INSTALL", ""), ("HOME", "/home/u")]));
        assert_eq!(resolved, PathBuf::new());
    }
}

/// Realize the whole manifest offline: adopt undeclared on-disk bares (global
/// discovery), then reconcile every declared root (clone declared-but-missing). A
/// per-root clone/guard failure becomes an `Applied{status: Failed}` rather than
/// aborting the sweep. The single-realizer composition the offline CLI (`grove
/// apply`) drives directly; a running daemon drives it per-root through its engines.
pub fn apply(home: &Path) -> Result<Vec<roots::Applied>> {
    roots::adopt(home)?;
    Ok(roots::list(home)?
        .iter()
        .map(|root| {
            roots::reconcile_one(home, &root.slug).unwrap_or_else(|e| roots::Applied {
                slug: root.slug.clone(),
                status: wire::ReconcileStatus::Failed,
                default_branch: None,
                error: Some(format!("{e:#}")),
            })
        })
        .collect())
}

/// End-to-end ops scenarios at the public library seam — the op inventory the
/// daemon's routes are built against.
///
/// These carry over the dispatch tests the v1 Port binary held: same scenarios,
/// same assertions, no framing. They live here rather than in `tests/` because they
/// lean on [`testfix`], which is `#[cfg(any(test, feature = "test-util"))] pub` —
/// off by default, and a `[dev-dependencies]` opt-in for the crates that need it.
#[cfg(test)]
mod tests {
    use super::apply;
    use crate::doctor::run as doctor;
    use crate::testfix;
    use std::path::Path;
    use tempfile::TempDir;

    const SLUG: &str = "o/r";

    /// A home with `o/r` cloned from a *writable* source repo (the returned path),
    /// so a test may commit to the remote. `testfix::home_with_root`'s source is the
    /// process-lifetime template and must never be written to.
    fn home_with_writable_src(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let src = tmp.path().join("src");
        testfix::fixture_repo(&src);
        let home = tmp.path().join("home");
        crate::roots::add(&home, SLUG, src.to_str().unwrap()).unwrap();
        (home, src)
    }

    fn commit(src: &Path, file: &str) {
        std::fs::write(src.join(file), "more").unwrap();
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
        testfix::git(src, &[&id[..], &["add", "."]].concat());
        testfix::git(src, &[&id[..], &["commit", "-q", "-m", file]].concat());
    }

    /// `add` → `list` → `remove`: the root CRUD trio, with delete-on-disk before
    /// undeclare (D4) observable as a vanished root dir *and* a vanished entry.
    #[test]
    fn root_add_list_remove_round_trip() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = home_with_writable_src(&tmp);

        let roots = crate::roots::list(&home).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].slug, SLUG);
        assert!(crate::roots::trunk_dir(&home, SLUG).is_dir());

        crate::roots::remove(&home, SLUG, crate::roots::Removal::Guarded).unwrap();
        assert!(crate::roots::list(&home).unwrap().is_empty());
        assert!(!crate::roots::root_dir(&home, SLUG).exists());
    }

    /// `roots.adopt` + `root.reconcile` composed: an on-disk bare nobody declared is
    /// adopted, then realized — the whole `grove apply` sweep in one call.
    #[test]
    fn apply_adopts_an_undeclared_bare_then_realizes_it() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = home_with_writable_src(&tmp);
        // Undeclare, leaving the bare + trunk on disk: exactly what adopt is for.
        crate::manifest::remove_root(&home.join("manifest.toml"), SLUG).unwrap();
        assert!(crate::roots::list(&home).unwrap().is_empty());

        let applied = apply(&home).unwrap();

        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].slug, SLUG);
        assert_eq!(applied[0].status, crate::wire::ReconcileStatus::Present);
        assert!(applied[0].error.is_none());
    }

    /// A root that cannot be cloned is captured as `failed` rather than aborting the
    /// sweep — the healthy root beside it still reconciles.
    #[test]
    fn apply_reports_a_failed_root_without_aborting_the_sweep() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = home_with_writable_src(&tmp);
        // A local path that is not a repository: the clone fails at gix with no
        // resolver and no socket. A URL here would make the assertion depend on the
        // network answering NXDOMAIN, and cost gix's 20s connect bound where it
        // doesn't (an ISP wildcard, a captive portal).
        crate::manifest::add_root(
            &home.join("manifest.toml"),
            "o/bad",
            tmp.path().join("not-a-repo").to_str().unwrap(),
        )
        .unwrap();

        let applied = apply(&home).unwrap();

        let bad = applied.iter().find(|a| a.slug == "o/bad").unwrap();
        assert_eq!(bad.status, crate::wire::ReconcileStatus::Failed);
        assert!(bad.error.is_some(), "the failure travels with the outcome");
        let good = applied.iter().find(|a| a.slug == SLUG).unwrap();
        assert_eq!(good.status, crate::wire::ReconcileStatus::Present);
    }

    /// The `doctor` op on an empty home: a well-formed empty report, not an error.
    #[test]
    fn doctor_on_an_empty_home_returns_an_empty_report() {
        let tmp = TempDir::new().unwrap();
        let (report, pools) = doctor(tmp.path(), None, true, false).unwrap();
        assert!(report.is_empty());
        assert!(pools.is_empty());
    }

    /// The realized half: doctor converges the declared shares and reports the pool
    /// level beside them — the operator's window on an under-filled pool.
    #[test]
    fn doctor_materializes_shares_and_reports_pool_levels() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root_and_worktree(&tmp);
        let manifest = home.join("manifest.toml");
        crate::manifest::add_share(&manifest, SLUG, "symlink", &[".env"]).unwrap();
        // A target with nothing warmed yet — the under-filled pool doctor exists to
        // surface. `pool::status` omits roots with neither a target nor a slot.
        crate::manifest::set_pool_size(&manifest, SLUG, 1).unwrap();

        let (report, pools) = doctor(&home, Some(SLUG), false, false).unwrap();

        assert!(
            report.iter().any(|o| o.path == ".env"),
            "the declared share is reported"
        );
        let link = crate::roots::root_dir(&home, SLUG).join("feat/.env");
        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "materialized as a link, not a copy"
        );
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].slug, SLUG);
        assert_eq!(pools[0].observed, 0);
        assert_eq!(pools[0].target, 1);
    }

    /// `worktree.list`'s payload for an *unrealized* root: every leg answers rather
    /// than aborting the read — no worktrees, a zero pool count, one `Error` share row
    /// naming the missing root, and `trunk_status` `None` rather than an error, so a
    /// dashboard renders mid-clone.
    #[test]
    fn worktree_list_carries_shares_and_pool_count() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();

        assert!(crate::worktrees::list(home, SLUG).unwrap().is_empty());
        assert_eq!(crate::worktrees::pool_count(home, SLUG).unwrap(), 0);

        let shares = crate::env::diagnose(home, Some(SLUG)).unwrap();
        assert_eq!(shares.len(), 1, "one row, reporting the unopenable root");
        assert_eq!(shares[0].status, crate::env::ShareStatus::Error);

        assert!(
            crate::roots::trunk_status(home, SLUG).unwrap().is_none(),
            "unrealized root has no status"
        );
    }

    /// The realized half of the pair above: a root with a trunk checkout on disk reports
    /// its git drift inline, so the dashboard reads status and worktrees in one call.
    #[test]
    fn worktree_list_carries_trunk_git_status() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);

        let status = crate::roots::trunk_status(&home, SLUG).unwrap().unwrap();

        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.staged, 0);
        assert_eq!(status.unstaged, 0);
        assert!(status.head.is_some(), "a realized trunk has a head");
    }

    /// `root.sync`: a fetched commit fast-forwards the trunk, and a second sync over
    /// the same tip is `already_current` — the report is flat and fully populated.
    #[test]
    fn root_sync_fast_forwards_then_reports_already_current() {
        let tmp = TempDir::new().unwrap();
        let (home, src) = home_with_writable_src(&tmp);
        commit(&src, "NEW.md");

        let report = crate::roots::sync(&home, SLUG).unwrap();
        assert!(report.fetched);
        assert_eq!(report.trunk, crate::git::FastForward::Updated);
        assert_eq!(report.stale_slots_pruned, 0);
        assert!(!report.tip.is_empty());

        let again = crate::roots::sync(&home, SLUG).unwrap();
        assert_eq!(again.trunk, crate::git::FastForward::AlreadyCurrent);
    }

    /// Syncing a root with nothing on disk is `not_ready` — a *transient* wire code,
    /// so the daemon's engine retries it on the next event instead of degrading. The
    /// category, not just the failure, is the contract.
    #[test]
    fn root_sync_on_an_unrealized_root_reports_not_ready() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = home_with_writable_src(&tmp);

        let err = crate::roots::sync(&home, "o/none").unwrap_err();

        assert_eq!(err.code(), "not_ready");
    }

    /// `pool.size` is the read-only declared target (default 0, opt-in); `pool.fill`
    /// adds exactly one warm slot per call.
    #[test]
    fn pool_size_defaults_to_zero_and_fill_adds_one_slot() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = home_with_writable_src(&tmp);

        assert_eq!(crate::pool::size(&home, SLUG).unwrap(), 0);
        assert_eq!(crate::pool::fill(&home, SLUG).unwrap(), 1);
        assert_eq!(crate::pool::fill(&home, SLUG).unwrap(), 2);
    }

    /// `pool.promote` consumes a warm slot; promoting against an empty pool is
    /// `Cold(Empty)` — a fallback signal, not an error.
    #[test]
    fn pool_promote_promotes_then_reports_cold_empty() {
        let tmp = TempDir::new().unwrap();
        let (home, _src) = home_with_writable_src(&tmp);
        crate::pool::fill(&home, SLUG).unwrap();

        let promoted =
            crate::pool::promote(&home, SLUG, "feat", "feature/x", Some("main")).unwrap();
        assert!(matches!(promoted, crate::pool::Promotion::Promoted));

        let cold = crate::pool::promote(&home, SLUG, "feat2", "y", Some("main")).unwrap();
        assert!(matches!(
            cold,
            crate::pool::Promotion::Cold(crate::pool::ColdReason::Empty)
        ));
    }
}
