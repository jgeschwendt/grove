//! On-disk version layout: versioned dirs + an atomic `current`/`previous` symlink
//! flip. Pure mechanism — no network, fully unit-tested.
//!
//! One thing here is new in v2: the **pending marker**. A flip is durable the instant
//! `rename(2)` returns, but a flip is only *trustworthy* once the health gate has
//! answered for it, and v1 held that verdict in the updater's memory alone — so a
//! crash in between stranded `current` on a version nothing had ever proven. The
//! marker writes that owed verdict to disk before `current` moves, and the updater
//! clears it once the gate settles; see [`Updater::up`](super::Updater::up) for the
//! recovery that reads it.

use std::collections::BTreeSet;
use std::fs;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};

use crate::CliError;

/// Default number of version dirs to retain after a successful update.
pub(super) const KEEP_VERSIONS: usize = 3;

/// Which way a pending flip moved `current`.
///
/// The marker carries this because **both** movers of `current` mark one, and the two
/// owe opposite recoveries. A marker naming `current` says only "a gate never answered
/// for this"; without the direction the recovery cannot tell an unproven forward flip
/// (undo it) from an interrupted rollback (whose `current` is already the *proven*
/// version — undoing it flips straight back onto the release the gate rejected).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flip {
    /// `current` moved onto a version nothing has vouched for.
    Forward,
    /// `current` moved back onto the version that was proven before the flip being
    /// undone. Nothing here may move `current` again.
    Rollback,
}

impl Flip {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Rollback => "rollback",
        }
    }
}

/// A flip whose health gate has not answered: what moved, and which way.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pending {
    pub version: String,
    pub kind: Flip,
}

/// The on-disk version layout rooted at `GROVE_INSTALL`. Every mutation is an
/// atomic symlink rename; version directories are immutable once written.
pub struct Layout {
    home: PathBuf,
}

impl Layout {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    fn versions_dir(&self) -> PathBuf {
        self.home.join("versions")
    }

    /// Absolute path of a version dir (whether or not it exists yet).
    #[must_use]
    pub fn version_path(&self, v: &str) -> PathBuf {
        self.versions_dir().join(v)
    }

    fn current_link(&self) -> PathBuf {
        self.home.join("current")
    }

    fn previous_link(&self) -> PathBuf {
        self.home.join("previous")
    }

    /// The file naming a flip whose health gate has not answered yet.
    fn pending_path(&self) -> PathBuf {
        self.home.join("pending")
    }

    /// The version `current` points at, if any.
    #[must_use]
    pub fn current_version(&self) -> Option<String> {
        link_target_name(&self.current_link())
    }

    /// The version `previous` points at, if any.
    #[must_use]
    pub fn previous_version(&self) -> Option<String> {
        link_target_name(&self.previous_link())
    }

    /// The version a flip moved `current` to and whose gate has not settled — an
    /// updater that is still running, or one that died mid-update. Cleared by
    /// [`clear_pending`](Self::clear_pending) the moment the gate answers.
    #[must_use]
    pub fn pending_version(&self) -> Option<String> {
        self.pending_flip().map(|p| p.version)
    }

    /// The whole marker: the version and the direction the flip moved.
    ///
    /// The file is `to=<v>` + `kind=forward|rollback`, one field per line. A file
    /// carrying a bare version and no fields is read as a **forward** flip — the only
    /// shape that existed before the direction was recorded, and the reading whose
    /// recovery is bounded (undo it) rather than one that leaves an unproven `current`
    /// standing.
    #[must_use]
    pub fn pending_flip(&self) -> Option<Pending> {
        let text = fs::read_to_string(self.pending_path()).ok()?;
        let mut version = None;
        let mut kind = Flip::Forward;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match line.split_once('=') {
                Some(("to", v)) => version = Some(v.trim().to_string()),
                Some(("kind", k)) if k.trim() == Flip::Rollback.as_str() => kind = Flip::Rollback,
                Some(_) => {}
                None => version = version.or_else(|| Some(line.to_string())),
            }
        }
        version
            .filter(|v| !v.is_empty())
            .map(|version| Pending { version, kind })
    }

    /// Record that `current` is about to move to `v`, which way, and that nothing has
    /// vouched for it yet. Written and fsync'd **before** the flip, which is what makes
    /// the two orderings distinguishable after a crash: a marker naming the version
    /// `current` points at means the flip landed and the gate never answered; one
    /// naming anything else means the flip itself never happened.
    fn mark_pending(&self, v: &str, kind: Flip) -> Result<(), CliError> {
        let path = self.pending_path();
        fs::write(&path, format!("to={v}\nkind={}\n", kind.as_str()))
            .map_err(|e| CliError::Update(format!("write {}: {e}", path.display())))?;
        fsync_file(&path)?;
        fsync_dir(&self.home);
        Ok(())
    }

    /// Drop the pending marker — the gate answered, so `current` is vouched for.
    /// Best-effort on the fsync, like every other directory barrier here.
    pub fn clear_pending(&self) {
        let _ = fs::remove_file(self.pending_path());
        fsync_dir(&self.home);
    }

    /// Installed versions, ascending by version order.
    #[must_use]
    pub fn installed_versions(&self) -> Vec<String> {
        let mut vs: Vec<String> = match fs::read_dir(self.versions_dir()) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect(),
            Err(_) => Vec::new(),
        };
        vs.sort_by_key(|v| version_key(v));
        vs
    }

    /// Point `current` at version `v`, atomically, moving the prior `current`
    /// target to `previous`. The flip is a `rename(2)` over the symlink, so it
    /// is never observed half-applied. Errors if `v` is not installed.
    ///
    /// Marks the flip pending first: every mover of `current` in this file does, so
    /// "an unproven `current`" is a state the filesystem can report rather than one
    /// only a live updater knows about.
    // stele:landmark pending-marker
    pub fn flip_to(&self, v: &str) -> Result<(), CliError> {
        if !self.version_path(v).is_dir() {
            return Err(CliError::Update(format!("version {v} is not installed")));
        }
        self.mark_pending(v, Flip::Forward)?;
        match self.current_version() {
            Some(old) if old != v => self.point(&self.previous_link(), &old)?,
            _ => {}
        }
        self.point(&self.current_link(), v)
    }

    /// Flip `current` back to `previous`, swapping the two so a forward flip is
    /// still possible. Returns the version rolled back to.
    ///
    /// Marks the flip pending as `rollback`: this mover of `current` owes a gate like
    /// the forward one, but the version it lands on is the *proven* one, so its
    /// recovery must never flip again.
    pub fn rollback(&self) -> Result<String, CliError> {
        let prev = self
            .previous_version()
            .ok_or_else(|| CliError::Update("no previous version to roll back to".into()))?;
        let cur = self.current_version();
        self.mark_pending(&prev, Flip::Rollback)?;
        self.point(&self.current_link(), &prev)?;
        if let Some(cur) = cur {
            self.point(&self.previous_link(), &cur)?;
        }
        Ok(prev)
    }

    /// Remove old version dirs, retaining the `keep` newest plus whatever
    /// `current`/`previous` reference (never deleted). Returns removed versions.
    pub fn prune(&self, keep: usize) -> Result<Vec<String>, CliError> {
        let protected: BTreeSet<String> = [self.current_version(), self.previous_version()]
            .into_iter()
            .flatten()
            .collect();

        let all = self.installed_versions(); // ascending
        let keep_newest: BTreeSet<&String> = all.iter().rev().take(keep).collect();

        let mut removed = Vec::new();
        for v in &all {
            if keep_newest.contains(v) || protected.contains(v) {
                continue;
            }
            fs::remove_dir_all(self.version_path(v))
                .map_err(|e| CliError::Update(format!("prune {v}: {e}")))?;
            removed.push(v.clone());
        }
        Ok(removed)
    }

    /// Extract a `.tar.gz` bundle into `versions/<v>/` (replacing any prior dir).
    pub fn install_bundle(&self, v: &str, tar_gz: &[u8]) -> Result<(), CliError> {
        let dest = self.version_path(v);
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .map_err(|e| CliError::Update(format!("clear {}: {e}", dest.display())))?;
        }
        fs::create_dir_all(&dest)
            .map_err(|e| CliError::Update(format!("create {}: {e}", dest.display())))?;

        let decoder = flate2::read::GzDecoder::new(tar_gz);
        tar::Archive::new(decoder)
            .unpack(&dest)
            .map_err(|e| CliError::Update(format!("extract {v}: {e}")))?;

        // A bundle without the binary it is supposed to carry is not an install.
        // Nothing downstream would notice: the health gate that would catch it is
        // skipped whenever no daemon is running — the state of every first install
        // and of the hand-off `install.sh` makes — so `current` flipped onto an empty
        // tree, `grove up` exited 0, and the operator's PATH symlink
        // (`current/bin/grove`) dangled, leaving no `grove` to roll back with. The
        // module's promise is that a broken release never becomes `current`; this is
        // what keeps it true on the path with no live daemon to prove it.
        let binary = dest.join("bin/grove");
        if !binary.is_file() {
            let _ = fs::remove_dir_all(&dest);
            return Err(CliError::Update(format!(
                "bundle for {v} carries no bin/grove; refusing to install it"
            )));
        }

        // Persist the extracted tree before it can become `current`. Without this
        // a crash between the flip and writeback could leave `current` pointing at
        // a version dir whose files were lost or truncated — a rename(2) outliving
        // the data it names. fsync every file + dir, then the versions dir so the
        // new dir entry itself is durable.
        fsync_tree(&dest)?;
        fsync_dir(&self.versions_dir());
        Ok(())
    }

    /// Exclusive advisory lock serializing `grove up`/`--rollback` on this
    /// `GROVE_INSTALL` — install→flip is otherwise racy between two updaters. Held
    /// for the lifetime of the returned handle (released on drop). `O_NOFOLLOW`
    /// refuses a symlinked lock path.
    ///
    /// `O_CLOEXEC` for the same reason [`ServerControl::lock_start`] carries it: the
    /// bounce inside the lock spawns the daemon, and a `flock` on an inherited open
    /// file description would outlive this process — every later `grove up` would
    /// block on the lock forever instead of taking it.
    ///
    /// [`ServerControl::lock_start`]: crate::ServerControl
    pub(super) fn lock(&self) -> Result<OwnedFd, CliError> {
        fs::create_dir_all(&self.home)
            .map_err(|e| CliError::Update(format!("create {}: {e}", self.home.display())))?;
        let fd = open(
            self.home.join("update.lock"),
            OFlags::CLOEXEC | OFlags::CREATE | OFlags::WRONLY | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|e| CliError::Update(format!("open update lock: {e}")))?;
        flock(&fd, FlockOperation::LockExclusive)
            .map_err(|e| CliError::Update(format!("lock update: {e}")))?;
        Ok(fd)
    }

    /// Atomically (re)point a symlink at `versions/<v>` using a relative target,
    /// so the layout survives a moved `GROVE_INSTALL`: write a temp link in the same
    /// dir, then `rename(2)` it over the destination.
    fn point(&self, link: &Path, v: &str) -> Result<(), CliError> {
        let target: PathBuf = ["versions", v].iter().collect();
        let tmp = self.home.join(format!(".{}.tmp", file_name(link)));
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp)
            .map_err(|e| CliError::Update(format!("symlink {}: {e}", tmp.display())))?;
        fs::rename(&tmp, link)
            .map_err(|e| CliError::Update(format!("flip {}: {e}", link.display())))?;
        // Persist the rename: fsync the link's parent (GROVE_INSTALL) so the flipped
        // `current`/`previous` symlink survives a crash right after the rename.
        fsync_dir(&self.home);
        Ok(())
    }
}

/// Recursively fsync every regular file and directory under `root`, then `root`
/// itself — a durability barrier for a freshly-extracted version dir. Symlinks
/// are skipped (nothing of ours to persist through them).
fn fsync_tree(root: &Path) -> Result<(), CliError> {
    for entry in
        fs::read_dir(root).map_err(|e| CliError::Update(format!("read {}: {e}", root.display())))?
    {
        let entry = entry.map_err(|e| CliError::Update(format!("read {}: {e}", root.display())))?;
        let ft = entry
            .file_type()
            .map_err(|e| CliError::Update(format!("stat {}: {e}", entry.path().display())))?;
        if ft.is_dir() {
            fsync_tree(&entry.path())?;
        } else if ft.is_file() {
            fsync_file(&entry.path())?;
        }
    }
    fsync_dir(root);
    Ok(())
}

/// fsync a regular file's bytes to disk.
fn fsync_file(path: &Path) -> Result<(), CliError> {
    fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| CliError::Update(format!("fsync {}: {e}", path.display())))
}

/// fsync a directory's entries — best-effort: some filesystems reject fsync on a
/// directory fd (`EINVAL`), which isn't fatal to the flip's correctness.
fn fsync_dir(path: &Path) {
    if let Ok(f) = fs::File::open(path) {
        let _ = f.sync_all();
    }
}

/// Resolve the version name a symlink points at (its target's final component).
fn link_target_name(link: &Path) -> Option<String> {
    let target = fs::read_link(link).ok()?;
    target.file_name()?.to_str().map(String::from)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("link")
        .to_string()
}

/// Version-aware sort key. Splits the core from a `-<prerelease>` suffix so a
/// release outranks its own prereleases: key on (core numbers, is-release, then
/// prerelease numbers, then the raw string). `is_release` (no `-` suffix) sorts
/// ABOVE any prerelease of the same core, so `0.4.0` > `0.4.0-canary.9`; equal
/// cores tie-break on the prerelease number (`…canary.9` > `…canary.2`) and
/// finally the raw string. Keeps `0.10.0` > `0.9.0` via numeric segments.
pub(super) fn version_key(v: &str) -> (Vec<u64>, bool, Vec<u64>, String) {
    let (core, pre) = v.split_once('-').unwrap_or((v, ""));
    (
        numeric_segments(core),
        pre.is_empty(),
        numeric_segments(pre),
        v.to_string(),
    )
}

/// The `u64` runs in `s`, split on any non-digit (so `0.10.0` → `[0, 10, 0]`).
fn numeric_segments(s: &str) -> Vec<u64> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u64>().ok())
        .collect()
}

/// A version names a `versions/<v>` directory and the symlink target, so it must
/// be a single safe path component: no `/`, never `.`/`..`. Guards against a
/// crafted `--version` or `latest` channel value escaping `versions/`.
pub(super) fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v != "."
        && v != ".."
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// Create an installed (empty) version dir with one marker file.
    fn install(layout: &Layout, v: &str) {
        let dir = layout.version_path(v);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin/grove"), v).unwrap();
    }

    /// A `.tar.gz` containing `bin/grove` whose contents name the version.
    fn fake_bundle(v: &str) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let body = format!("grove {v}");
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "bin/grove", body.as_bytes())
            .unwrap();
        let tar_bytes = tar.into_inner().unwrap();

        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn flip_sets_current_and_moves_old_to_previous() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        install(&layout, "0.3.1");
        install(&layout, "0.4.0");

        layout.flip_to("0.3.1").unwrap();
        assert_eq!(layout.current_version().as_deref(), Some("0.3.1"));
        assert!(layout.previous_version().is_none());

        layout.flip_to("0.4.0").unwrap();
        assert_eq!(layout.current_version().as_deref(), Some("0.4.0"));
        assert_eq!(layout.previous_version().as_deref(), Some("0.3.1"));

        // current/bin/grove resolves through the symlink to the new version.
        assert_eq!(
            fs::read_to_string(home.path().join("current/bin/grove")).unwrap(),
            "0.4.0"
        );
    }

    #[test]
    fn flip_to_uninstalled_version_errors() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        let err = layout.flip_to("9.9.9").unwrap_err();
        assert_eq!(err.exit_code(), 7);
        assert!(
            layout.pending_version().is_none(),
            "a refused flip marks nothing — the installed check comes first"
        );
    }

    #[test]
    fn rollback_swaps_current_and_previous() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        install(&layout, "0.3.1");
        install(&layout, "0.4.0");
        layout.flip_to("0.3.1").unwrap();
        layout.flip_to("0.4.0").unwrap();

        let restored = layout.rollback().unwrap();
        assert_eq!(restored, "0.3.1");
        assert_eq!(layout.current_version().as_deref(), Some("0.3.1"));
        // previous now points at what was current — roll forward is possible.
        assert_eq!(layout.previous_version().as_deref(), Some("0.4.0"));
    }

    #[test]
    fn rollback_without_previous_errors() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        assert_eq!(layout.rollback().unwrap_err().exit_code(), 7);
    }

    /// The marker is written by the flip and outlives it: nothing in `Layout` clears
    /// it, because only the gate's verdict may. A crash anywhere between the flip and
    /// that verdict therefore leaves exactly this state on disk.
    ///
    /// And it records **which way** — the half a marker naming only a version cannot
    /// supply. Both movers write one, and a recovery that reads a rollback's marker as
    /// a forward flip undoes the rollback: straight back onto the version the gate
    /// just rejected.
    #[test]
    fn every_mover_of_current_marks_the_flip_pending_with_its_direction() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        assert!(layout.pending_flip().is_none(), "nothing owed yet");
        install(&layout, "0.3.1");
        install(&layout, "0.4.0");

        layout.flip_to("0.3.1").unwrap();
        assert_eq!(
            layout.pending_flip(),
            Some(Pending {
                version: "0.3.1".into(),
                kind: Flip::Forward
            })
        );
        layout.clear_pending();
        assert!(layout.pending_version().is_none());

        layout.flip_to("0.4.0").unwrap();
        assert_eq!(layout.pending_version().as_deref(), Some("0.4.0"));

        // A rollback moves `current` too, so its gate is owed the same way — but it
        // lands on the PROVEN version, and the marker has to say so.
        layout.clear_pending();
        assert_eq!(layout.rollback().unwrap(), "0.3.1");
        assert_eq!(
            layout.pending_flip(),
            Some(Pending {
                version: "0.3.1".into(),
                kind: Flip::Rollback
            })
        );
    }

    /// The marker's on-disk shape, and the one tolerance it grants: a file carrying a
    /// bare version — a hand-edit, or a crash simulated by writing one — reads as a
    /// forward flip, the only shape that existed before the direction was recorded.
    #[test]
    fn a_marker_names_its_direction_and_an_unlabelled_one_reads_forward() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        install(&layout, "0.4.0");
        layout.flip_to("0.4.0").unwrap();

        let raw = fs::read_to_string(home.path().join("pending")).unwrap();
        assert_eq!(raw, "to=0.4.0\nkind=forward\n");

        fs::write(home.path().join("pending"), "0.4.0\n").unwrap();
        assert_eq!(
            layout.pending_flip(),
            Some(Pending {
                version: "0.4.0".into(),
                kind: Flip::Forward
            })
        );

        // An empty (or whitespace-only) marker is no marker at all.
        fs::write(home.path().join("pending"), " \n").unwrap();
        assert!(layout.pending_flip().is_none());
    }

    /// The marker is written BEFORE `current` moves, which is the whole reason it
    /// carries a version rather than being a bare flag: a marker naming something
    /// `current` does not point at is a flip that never landed, so nothing is
    /// unproven and the recovery must leave `current` alone.
    #[test]
    fn a_marker_can_outlive_a_flip_that_never_landed() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        install(&layout, "0.3.1");
        layout.flip_to("0.3.1").unwrap();
        layout.clear_pending();

        // The crash window the ordering opens: marked, then the process died before
        // the rename. `current` still names the old, proven version.
        layout.mark_pending("0.4.0", Flip::Forward).unwrap();
        assert_eq!(layout.pending_version().as_deref(), Some("0.4.0"));
        assert_eq!(layout.current_version().as_deref(), Some("0.3.1"));
    }

    #[test]
    fn version_key_ranks_release_above_its_prerelease() {
        // A release must outrank its own prereleases so `prune` retains it: the
        // bug keyed `0.4.0-canary.9` → [0,4,0,9] ABOVE `0.4.0` → [0,4,0].
        let mut vs = vec!["0.4.0-canary.9", "0.4.0", "0.3.0"];
        vs.sort_by_key(|v| version_key(v));
        // Ascending: 0.3.0 < 0.4.0-canary.9 < 0.4.0 (the release is newest).
        assert_eq!(vs, ["0.3.0", "0.4.0-canary.9", "0.4.0"]);
        // Equal cores tie-break on the prerelease number, then the raw string.
        let mut canaries = vec!["0.4.0-canary.2", "0.4.0-canary.10", "0.4.0-canary.1"];
        canaries.sort_by_key(|v| version_key(v));
        assert_eq!(
            canaries,
            ["0.4.0-canary.1", "0.4.0-canary.2", "0.4.0-canary.10"]
        );
    }

    #[test]
    fn fsync_tree_walks_a_nested_dir() {
        // The durability barrier must fsync every file + dir without error over a
        // realistic (nested) extracted tree.
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        layout
            .install_bundle("0.4.0", &fake_bundle("0.4.0"))
            .unwrap();
        let dir = layout.version_path("0.4.0");
        fs::create_dir_all(dir.join("lib/nested")).unwrap();
        fs::write(dir.join("lib/nested/x"), b"data").unwrap();
        assert!(fsync_tree(&dir).is_ok());
    }

    #[test]
    fn installed_versions_sort_by_version_not_lexically() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        for v in ["0.9.0", "0.10.0", "0.2.0"] {
            install(&layout, v);
        }
        assert_eq!(layout.installed_versions(), ["0.2.0", "0.9.0", "0.10.0"]);
    }

    #[test]
    fn prune_keeps_newest_n_plus_current_and_previous() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        for v in ["0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.5.0"] {
            install(&layout, v);
        }
        // current=0.2.0 (old), previous=0.1.0 (oldest) — both must survive prune.
        layout.flip_to("0.2.0").unwrap();
        layout.flip_to("0.1.0").unwrap();
        layout.flip_to("0.2.0").unwrap();

        let removed = layout.prune(2).unwrap();
        let left = layout.installed_versions();
        // keep newest 2 (0.4.0, 0.5.0) + current (0.2.0) + previous (0.1.0); drop 0.3.0.
        assert_eq!(removed, ["0.3.0"]);
        assert_eq!(left, ["0.1.0", "0.2.0", "0.4.0", "0.5.0"]);
    }

    #[test]
    fn install_bundle_extracts_tarball() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());
        let bundle = fake_bundle("0.4.0");
        layout.install_bundle("0.4.0", &bundle).unwrap();
        assert_eq!(
            fs::read_to_string(layout.version_path("0.4.0").join("bin/grove")).unwrap(),
            "grove 0.4.0"
        );
    }

    /// A bundle that unpacks but carries no binary is refused before it can become
    /// `current`. The health gate cannot catch this one: it is skipped whenever no
    /// daemon is running, which is every first install — so `grove up` reported
    /// success and left the operator's `grove` a dangling symlink into an empty
    /// version dir, with nothing on PATH to roll back with.
    #[test]
    fn install_bundle_refuses_a_bundle_with_no_binary() {
        let home = TempDir::new().unwrap();
        let layout = Layout::new(home.path());

        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(6);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "README", &b"hello\n"[..])
            .unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar.into_inner().unwrap()).unwrap();
        let bundle = enc.finish().unwrap();

        let err = layout.install_bundle("7.0.0", &bundle).unwrap_err();
        assert_eq!(err.exit_code(), 7);
        assert!(err.to_string().contains("no bin/grove"), "{err}");
        assert!(
            !layout.version_path("7.0.0").exists(),
            "the half-installed version dir is cleared, not left to be flipped onto"
        );
    }

    #[test]
    fn valid_version_rejects_traversal() {
        assert!(valid_version("0.4.0") && valid_version("0.1.0-smoke") && valid_version("1.2.3+b"));
        for bad in ["", ".", "..", "../x", "a/b", "x\0y", "a b"] {
            assert!(!valid_version(bad), "should reject {bad:?}");
        }
    }
}
