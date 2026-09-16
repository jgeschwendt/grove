//! The retired root layouts, and the one place grove spells them.
//!
//! Every half of every retired layout lives *inside the root's code dir* — which is
//! where the trunk checkout still lives, and where the bare and the pool no longer do.
//! Two generations are retired:
//!
//! - **v1**, before the trunk was named by its branch: the bare at `.git`, the trunk
//!   checkout at `.trunk`.
//! - **v2**, before a root got a directory of its own: the bare at `.bare`, the warm
//!   pool at `.pool`.
//!
//! Two readers need the same answer for opposite reasons, which is why the names and
//! the predicate live here rather than inside either of them: [`crate::doctor`] names
//! the layout so an operator can retire it, and [`crate::roots`]'s realizer refuses to
//! clone over it. A second spelling would let the two disagree, and the way they
//! disagree is silent — reconcile reads "no bare here" and lays a whole second root
//! down beside the first, which is exactly the shape this module exists to prevent.

use std::fmt;
use std::path::Path;

/// v1: the bare's directory inside a root's code dir, and the trunk checkout's beside
/// it. Everything on the current layout reads [`crate::roots::bare_dir`] and
/// [`crate::roots::trunk`] instead.
pub const LEGACY_BARE: &str = ".git";
pub const LEGACY_TRUNK: &str = ".trunk";

/// v2: the bare and the warm pool, still inside the code dir. The current layout keeps
/// both under the root's own directory — [`crate::roots::bare_dir`] and
/// [`crate::pool::pool_dir`].
pub const LEGACY_V2_BARE: &str = ".bare";
pub const LEGACY_V2_POOL: &str = ".pool";

/// One half of one retired layout, as it sits in a code dir.
///
/// Halves rather than whole generations because a migration interrupted between its
/// renames leaves exactly one of a pair, and a root can carry halves of *both*
/// generations — a v1 root warmed by a v2 grove has `.trunk` beside `.pool`. Each half
/// is enough on its own to refuse a clone, and each is migrated independently.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Half {
    V1Bare,
    V1Trunk,
    V2Bare,
    V2Pool,
}

impl Half {
    /// Every half, oldest generation first — the order a [`Legacy`] reports in.
    pub const ALL: [Self; 4] = [Self::V1Bare, Self::V1Trunk, Self::V2Bare, Self::V2Pool];

    /// The code-dir entry this half occupies. The migration moves it, the detector
    /// reads it, and neither spells it itself.
    #[must_use]
    pub const fn entry(self) -> &'static str {
        match self {
            Self::V1Bare => LEGACY_BARE,
            Self::V1Trunk => LEGACY_TRUNK,
            Self::V2Bare => LEGACY_V2_BARE,
            Self::V2Pool => LEGACY_V2_POOL,
        }
    }

    /// Which retired layout this half belongs to. An operator asked to migrate wants
    /// the generation first — it is what tells them how old the root is and which of
    /// their boxes are about to say the same thing.
    #[must_use]
    pub const fn generation(self) -> &'static str {
        match self {
            Self::V1Bare | Self::V1Trunk => "v1",
            Self::V2Bare | Self::V2Pool => "v2",
        }
    }

    /// Is this half the root's bare? The two generations differ only in where they put
    /// it, so every reader that moves, refuses or names "the bare in the code dir" asks
    /// this rather than matching the variants itself.
    #[must_use]
    pub const fn is_bare(self) -> bool {
        matches!(self, Self::V1Bare | Self::V2Bare)
    }

    /// What it is, for the sentence an operator reads.
    const fn what(self) -> &'static str {
        match self {
            Self::V1Bare | Self::V2Bare => "a bare repo",
            Self::V1Trunk => "the trunk checkout",
            Self::V2Pool => "the warm pool",
        }
    }

    /// Is this half on disk under `code`? A bare is proved rather than assumed — see
    /// [`is_legacy_bare`]; the other halves are plain directories.
    fn found(self, code: &Path) -> bool {
        let path = code.join(self.entry());
        if self.is_bare() {
            is_legacy_bare(&path)
        } else {
            path.is_dir()
        }
    }
}

/// The halves a root carries — non-empty by construction, since a root carrying none
/// is on the current layout and [`legacy`] answers `None` for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Legacy(Vec<Half>);

impl Legacy {
    /// The halves found, oldest generation first.
    #[must_use]
    pub fn halves(&self) -> &[Half] {
        &self.0
    }

    /// The generations found, oldest first — one entry for an ordinary retired root,
    /// two for a root that carries halves of both.
    #[must_use]
    pub fn generations(&self) -> Vec<&'static str> {
        let mut generations: Vec<&'static str> = self.0.iter().map(|h| h.generation()).collect();
        generations.dedup();
        generations
    }
}

/// Phrased as what was *found*, not as a verdict: the sentence is embedded in a
/// refusal that already supplies the remedy, and an operator reading it needs to know
/// which generation they are on and which entries to go look at.
impl fmt::Display for Legacy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let groups: Vec<String> = self
            .generations()
            .into_iter()
            .map(|generation| {
                let halves: Vec<String> = self
                    .0
                    .iter()
                    .filter(|half| half.generation() == generation)
                    .map(|half| format!("{} is {}", half.entry(), half.what()))
                    .collect();
                format!("{generation} ({})", halves.join(", and "))
            })
            .collect();
        f.write_str(&groups.join(" and "))
    }
}

/// Is this root on a retired layout, and which halves of it?
///
/// Addressed by `home` and `slug` like every other layout lookup
/// ([`crate::roots::root_dir`], [`crate::roots::code_dir`]) rather than by a directory
/// the caller resolved: the halves live in one tree and what replaces them lives in
/// another, so a detector handed a single path could only ever see half the question.
#[must_use]
pub fn legacy(home: &Path, slug: &str) -> Option<Legacy> {
    let code = crate::roots::code_dir(home, slug);
    let halves: Vec<Half> = Half::ALL
        .into_iter()
        .filter(|half| half.found(&code))
        .collect();
    (!halves.is_empty()).then_some(Legacy(halves))
}

/// Is `path` one of the retired bares? A bare repository is a directory with a `HEAD`
/// in it — which is what separates it from a linked worktree's gitlink of the same
/// name, a *file*, and from a code dir that holds no repository at all.
#[must_use]
pub fn is_legacy_bare(path: &Path) -> bool {
    path.is_dir() && path.join("HEAD").is_file()
}

#[cfg(test)]
mod tests {
    use super::{Half, LEGACY_BARE, LEGACY_V2_POOL, is_legacy_bare, legacy};
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A home whose one root's code dir carries exactly the named halves, laid down by
    /// hand: the shapes here are the ones grove's own writers no longer produce.
    fn home(tmp: &TempDir, halves: &[Half]) -> PathBuf {
        let home = tmp.path().join("home");
        let code = crate::roots::code_dir(&home, crate::testfix::SLUG);
        std::fs::create_dir_all(&code).unwrap();
        for half in halves {
            let entry = code.join(half.entry());
            std::fs::create_dir_all(&entry).unwrap();
            if half.is_bare() {
                std::fs::write(entry.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            }
        }
        home
    }

    fn found(tmp: &TempDir, halves: &[Half]) -> Option<Vec<Half>> {
        let home = home(tmp, halves);
        legacy(&home, crate::testfix::SLUG).map(|l| l.halves().to_vec())
    }

    /// Every shape on disk reads back as itself — each half alone, each generation
    /// whole, both generations mixed — and a root on the current layout reads as none.
    #[test]
    fn legacy_names_which_halves_are_present() {
        for halves in [
            vec![],
            vec![Half::V1Bare],
            vec![Half::V1Trunk],
            vec![Half::V2Bare],
            vec![Half::V2Pool],
            vec![Half::V1Bare, Half::V1Trunk],
            vec![Half::V2Bare, Half::V2Pool],
            vec![Half::V1Trunk, Half::V2Bare, Half::V2Pool],
        ] {
            let tmp = TempDir::new().unwrap();
            let want = (!halves.is_empty()).then(|| halves.clone());
            assert_eq!(found(&tmp, &halves), want, "{halves:?}");
        }
    }

    /// A root carrying halves of both generations is on both, and says so.
    #[test]
    fn a_mixed_root_reports_both_generations() {
        let tmp = TempDir::new().unwrap();
        let home = home(&tmp, &[Half::V1Trunk, Half::V2Bare]);
        let found = legacy(&home, crate::testfix::SLUG).unwrap();
        assert_eq!(found.generations(), vec!["v1", "v2"]);
        assert_eq!(
            legacy(&home, "other/root"),
            None,
            "the answer is per-root, not per-home"
        );
    }

    /// A `.git` *file* is a linked worktree's gitlink — the shape every checkout in a
    /// code dir carries, and reading it as a bare would refuse every healthy root
    /// there is.
    #[test]
    fn a_gitlink_file_is_not_a_legacy_bare() {
        let tmp = TempDir::new().unwrap();
        let home = home(&tmp, &[]);
        let code = crate::roots::code_dir(&home, crate::testfix::SLUG);
        std::fs::write(
            code.join(LEGACY_BARE),
            "gitdir: ../../../roots/o/r/bare/worktrees/main\n",
        )
        .unwrap();

        assert!(!is_legacy_bare(&code.join(LEGACY_BARE)));
        assert_eq!(legacy(&home, crate::testfix::SLUG), None);
    }

    /// A directory without a `HEAD` is not a repository, bare or otherwise — and a
    /// half that is not a bare needs no such proof.
    #[test]
    fn a_headless_directory_is_not_a_legacy_bare() {
        let tmp = TempDir::new().unwrap();
        let home = home(&tmp, &[]);
        let code = crate::roots::code_dir(&home, crate::testfix::SLUG);
        std::fs::create_dir_all(code.join(LEGACY_BARE)).unwrap();
        std::fs::create_dir_all(code.join(LEGACY_V2_POOL)).unwrap();

        assert!(!is_legacy_bare(&code.join(LEGACY_BARE)));
        assert_eq!(
            legacy(&home, crate::testfix::SLUG)
                .unwrap()
                .halves()
                .to_vec(),
            vec![Half::V2Pool],
            "the headless `.git` is not a half; the pool is"
        );
    }

    /// The rendering an operator reads in the refusal names the generation and every
    /// entry to go look at.
    #[test]
    fn the_rendering_names_every_half_found() {
        // A fresh scratch per rendering: the halves are laid down cumulatively, so one
        // shared home would make every later case a superset of the earlier ones.
        let render = |halves: &[Half]| {
            let tmp = TempDir::new().unwrap();
            legacy(&home(&tmp, halves), crate::testfix::SLUG)
                .unwrap()
                .to_string()
        };

        assert_eq!(
            render(&[Half::V1Bare, Half::V1Trunk]),
            "v1 (.git is a bare repo, and .trunk is the trunk checkout)"
        );
        assert_eq!(
            render(&[Half::V2Bare, Half::V2Pool]),
            "v2 (.bare is a bare repo, and .pool is the warm pool)"
        );
        assert_eq!(
            render(&[Half::V1Trunk, Half::V2Bare]),
            "v1 (.trunk is the trunk checkout) and v2 (.bare is a bare repo)"
        );
    }
}
