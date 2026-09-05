//! The root layout that predates naming the trunk by its branch, and the one place
//! grove spells it.
//!
//! Two readers need the same answer for opposite reasons, which is why the names and
//! the predicate live here rather than inside either of them: [`crate::doctor`] names
//! the layout so an operator can retire it, and [`crate::roots`]'s realizer refuses to
//! clone over it. A second spelling would let the two disagree, and the way they
//! disagree is silent — reconcile reads "no bare here" and lays a whole second root
//! down beside the first, which is exactly the shape this module exists to prevent.

use std::fmt;
use std::path::Path;

/// The bare's directory under a root before the trunk was named by its branch, and
/// the trunk checkout's beside it. Everything on the current layout reads
/// [`crate::roots::bare_dir`] and [`crate::roots::trunk`] instead.
pub const LEGACY_BARE: &str = ".git";
pub const LEGACY_TRUNK: &str = ".trunk";

/// Which halves of the legacy layout a root carries. A migration interrupted between
/// its two renames leaves exactly one of them, so `Bare` and `Trunk` are real states
/// on disk rather than a completeness exercise — and each is enough on its own to
/// refuse a clone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Legacy {
    Bare,
    Both,
    Trunk,
}

/// Phrased as what was *found*, not as a verdict: the sentence is embedded in a
/// refusal that already supplies the remedy, and an operator reading it needs to know
/// which directory to go look at.
impl fmt::Display for Legacy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bare = format!("{LEGACY_BARE} is a bare repo");
        let trunk = format!("{LEGACY_TRUNK} is the trunk checkout");
        match self {
            Self::Bare => f.write_str(&bare),
            Self::Both => write!(f, "{bare}, and {trunk}"),
            Self::Trunk => f.write_str(&trunk),
        }
    }
}

/// Is this root on the legacy layout, and which halves of it?
#[must_use]
pub fn legacy(root: &Path) -> Option<Legacy> {
    match (
        is_legacy_bare(&root.join(LEGACY_BARE)),
        root.join(LEGACY_TRUNK).is_dir(),
    ) {
        (false, false) => None,
        (false, true) => Some(Legacy::Trunk),
        (true, false) => Some(Legacy::Bare),
        (true, true) => Some(Legacy::Both),
    }
}

/// Is `<root>/.git` the legacy bare? A bare repository is a directory with a `HEAD`
/// in it — which is what separates it from a linked worktree's gitlink of the same
/// name, a *file*, and from a root that holds no repository at all.
#[must_use]
pub fn is_legacy_bare(path: &Path) -> bool {
    path.is_dir() && path.join("HEAD").is_file()
}

#[cfg(test)]
mod tests {
    use super::{LEGACY_BARE, LEGACY_TRUNK, Legacy, is_legacy_bare, legacy};
    use tempfile::TempDir;

    /// A root with the named halves of the legacy layout laid down by hand.
    fn root(tmp: &TempDir, bare: bool, trunk: bool) -> std::path::PathBuf {
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        if bare {
            std::fs::create_dir_all(root.join(LEGACY_BARE)).unwrap();
            std::fs::write(
                root.join(LEGACY_BARE).join("HEAD"),
                "ref: refs/heads/main\n",
            )
            .unwrap();
        }
        if trunk {
            std::fs::create_dir_all(root.join(LEGACY_TRUNK)).unwrap();
        }
        root
    }

    /// Each of the three shapes on disk reads back as itself, and a root on the
    /// current layout reads as none.
    #[test]
    fn legacy_names_which_halves_are_present() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(legacy(&root(&tmp, false, false)), None);

        let tmp = TempDir::new().unwrap();
        assert_eq!(legacy(&root(&tmp, true, false)), Some(Legacy::Bare));

        let tmp = TempDir::new().unwrap();
        assert_eq!(legacy(&root(&tmp, false, true)), Some(Legacy::Trunk));

        let tmp = TempDir::new().unwrap();
        assert_eq!(legacy(&root(&tmp, true, true)), Some(Legacy::Both));
    }

    /// A `.git` *file* is a linked worktree's gitlink — the shape every non-legacy
    /// checkout under a root carries, and reading it as a bare would refuse every
    /// healthy root there is.
    #[test]
    fn a_gitlink_file_is_not_the_legacy_bare() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(LEGACY_BARE), "gitdir: ../.bare/worktrees/main\n").unwrap();

        assert!(!is_legacy_bare(&root.join(LEGACY_BARE)));
        assert_eq!(legacy(&root), None);
    }

    /// A directory without a `HEAD` is not a repository, bare or otherwise.
    #[test]
    fn a_headless_directory_is_not_the_legacy_bare() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join(LEGACY_BARE)).unwrap();

        assert!(!is_legacy_bare(&root.join(LEGACY_BARE)));
        assert_eq!(legacy(&root), None);
    }

    /// The rendering an operator reads in the refusal names the directory to look at.
    #[test]
    fn the_rendering_names_what_was_found() {
        assert_eq!(Legacy::Bare.to_string(), ".git is a bare repo");
        assert_eq!(Legacy::Trunk.to_string(), ".trunk is the trunk checkout");
        assert_eq!(
            Legacy::Both.to_string(),
            ".git is a bare repo, and .trunk is the trunk checkout"
        );
    }
}
