//! Shared test fixtures — compiled under `cfg(test)`, or under the `test-util`
//! feature, which is off by default and enabled by a dependent's
//! `[dev-dependencies]` only. Nothing here links into the shipped binary.
//!
//! Every fixture is built **inside the caller's own `TempDir`**, so ordinary `Drop`
//! reclaims it when the test ends. Nothing here is held in a `static`: Rust never
//! runs destructors for statics, so a `static OnceLock<TempDir>` is a `TempDir` whose
//! `remove_dir_all` never fires — one leaked fixture tree per test *process*,
//! forever. Under nextest, which gives every test its own process, that was 113
//! directories and 2.9 GB accumulated in `$TMPDIR` over a day's development, invisible
//! to CI (whose runners are thrown away) and paid for only on a developer's disk.
//!
//! Nothing is lost by building in place. Under nextest — and under the pre-commit
//! hook's `nextest run --lib` — one test is one process, so a process-lifetime cache
//! is a per-test cache either way; a `home_with_root` costs the same clone whether it
//! is copied from a template or cloned directly. Building in place also removes the
//! copy's own hazard: linked worktrees record absolute `gitdir` paths, so a copied
//! home needed a hand-written re-homing pass that a directly-built one simply does
//! not.
//!
//! Within one `TempDir` the tiers still build lazily and at most once, so a test that
//! asks for two fixtures over the same scratch pays for the shared `src` once:
//!
//! - [`fixture_repo`] — the one-commit `src` repo: ~4 git spawns.
//! - [`home_with_root`] — `src` + a cloned root: adds a gix clone + `worktree add`.
//! - [`home_with_root_and_worktree`] — the above + a user worktree.
//!
//! The `src` a home's manifest records lives in that same `TempDir`, so it outlives
//! every use of the home and dies with it.

#![allow(
    clippy::missing_panics_doc,
    reason = "a fixture that cannot build its own git state must abort the test that \
              asked for it; a `# Panics` section on each would document nothing else"
)]

use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// The slug every fixture home declares.
pub const SLUG: &str = "o/r";

/// A one-commit local source repo on `main` at `dir` — the drop-in replacement
/// for the per-test `git init` + `add` + `commit` helpers.
pub fn fixture_repo(dir: &Path) {
    // Idempotent, so a caller that shares one `src` across fixtures builds it once.
    if dir.join(".git").exists() {
        return;
    }
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hi").unwrap();
    let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
    git(dir, &[&id[..], &["add", "."]].concat());
    git(dir, &[&id[..], &["commit", "-q", "-m", "init"]].concat());
}

/// A home under `tmp` with one cloned root `o/r` (bare + `.trunk` on `main`).
///
/// The source repo lands at `<tmp>/src` and the home at `<tmp>/home`; the manifest
/// records the former by absolute path, which is why both must share the caller's
/// scratch rather than one outliving the other.
#[must_use]
pub fn home_with_root(tmp: &TempDir) -> PathBuf {
    let home = tmp.path().join("home");
    if home.exists() {
        return home;
    }
    let src = tmp.path().join("src");
    fixture_repo(&src);
    crate::roots::add(&home, SLUG, src.to_str().unwrap()).unwrap();
    home
}

/// `home_with_root` plus one user worktree `feat` on `feature/x`.
#[must_use]
pub fn home_with_root_and_worktree(tmp: &TempDir) -> PathBuf {
    let home = home_with_root(tmp);
    if !crate::roots::root_dir(&home, SLUG).join("feat").exists() {
        crate::worktrees::create(&home, SLUG, "feat", "feature/x", Some("main")).unwrap();
    }
    home
}

/// The on-disk root directory for `slug` under `home`. Re-exported for consumers'
/// tests: the layout helper itself is crate-private, and a test that hand-built
/// `home/code/<slug>` would be a second copy of the layout.
#[must_use]
pub fn root_dir(home: &Path, slug: &str) -> PathBuf {
    crate::roots::root_dir(home, slug)
}

pub fn git(cwd: &Path, args: &[&str]) {
    // Through the scrubbed runner: a `cargo test` run from inside a git hook
    // carries GIT_DIR/GIT_INDEX_FILE, which would redirect raw fixture spawns.
    let ok = crate::git::git_command()
        // No detached auto-maintenance: its janitor deletes temp files under .git
        // while another part of the fixture may still be walking it (Linux CI race).
        .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?} failed");
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    /// The fixture contract every consumer test leans on: the home is git-functional
    /// where it stands, its worktrees are registered at *its own* paths (present +
    /// declared), and it reclaims itself with the caller's `TempDir`. A regression
    /// here would misdirect every suite built on these fixtures, so it is pinned.
    #[test]
    fn a_fixture_home_is_git_functional_and_reclaimed_with_its_scratch() {
        let scratch;
        {
            let tmp = TempDir::new().unwrap();
            scratch = tmp.path().to_path_buf();
            let home = super::home_with_root_and_worktree(&tmp);

            let feat = crate::worktrees::list(&home, super::SLUG)
                .unwrap()
                .into_iter()
                .find(|w| w.name == "feat")
                .expect("feat listed");
            assert!(feat.present && feat.declared, "links point at THIS home");

            // git still works here: a new worktree lands under this home.
            crate::worktrees::create(&home, super::SLUG, "probe", "probe/x", Some("main")).unwrap();
            assert!(
                crate::roots::root_dir(&home, super::SLUG)
                    .join("probe/README.md")
                    .exists()
            );
            // The source the manifest records lives in the same scratch, so it
            // cannot outlive — or be outlived by — the home that points at it.
            assert!(scratch.join("src/.git").exists());
        }
        assert!(
            !scratch.exists(),
            "a fixture must leave nothing behind: `Drop` is the only thing that \
             reclaims it, and a `static` TempDir never gets one"
        );
    }

    /// Two fixtures over one scratch share the source repo rather than rebuilding
    /// it — the whole benefit the old process-lifetime templates were reaching for,
    /// at the only scope where it is actually reachable.
    #[test]
    fn one_scratch_builds_its_source_once() {
        let tmp = TempDir::new().unwrap();
        let first = super::home_with_root(&tmp);
        let head = crate::roots::trunk_dir(&first, super::SLUG).join(".git");
        assert!(head.exists());
        assert_eq!(super::home_with_root_and_worktree(&tmp), first);
        assert!(
            crate::roots::root_dir(&first, super::SLUG)
                .join("feat")
                .exists(),
            "the second call layered onto the first rather than rebuilding it"
        );
    }
}
