//! Worktree environment: materialize a root's declared static shares
//! (`[roots."<slug>".env]._.symlink` / `._.copy`) into every worktree — a `symlink`
//! as a relative link to the canonical source in the trunk checkout (named by its
//! branch; live, shared), a `copy` as an independent real file seeded once from that
//! source. The sole owner of share filesystem mutation — nothing else in the codebase
//! materializes a share. See `docs/worktrees.html#environment`.
//!
//! **Copy is seed-once.** A `_.copy` share is written into a worktree only when its
//! slot is empty (or holds a stale *grove-owned* symlink, migrated from a prior
//! `_.symlink` declaration). Once a real file is there, the worktree owns it: copy
//! never overwrites it, and — being indistinguishable from a user file — never GCs
//! it on undeclare. (Symlinks, identifiable as grove's own, self-heal and GC.)
//!
//! **Security posture.** Two independent gates: `manifest::validate_share_path`
//! (the string gate — applied at `list_shares`, so only canonical, traversal-free
//! paths reach the materializer) and, here, a **dirfd-pinned `O_NOFOLLOW` descent**:
//! every component of a share path *within* the worktree is opened with
//! `openat(NOFOLLOW)` from a pinned dirfd, so a symlinked parent (`config → /etc`)
//! trips `ELOOP` and can never redirect a write outside the worktree. (The base
//! worktree/trunk dirs themselves sit under the grove-controlled `GROVE_HOME`
//! and are opened with `NOFOLLOW` on their final component — their ancestors are
//! grove's own, not attacker-influenced.) classify→act share one pinned dirfd,
//! closing the lstat→create TOCTOU. The leaf write is the `update.rs::point` idiom
//! (relative target, same-dir `rename(2)`) on the pinned fd. No `unsafe`, no `libc`.
//!
//! **Never clobber.** Only a symlink that is grove's *own* is ever touched: an
//! exact match (`../…/<trunk>/<p>`) is left alone, a *grove-shaped* link (points
//! under the trunk — under its branch-derived name, the fixed name a not-yet-migrated
//! root still carries, or — in the one pass `converge_trunk` runs before it moves
//! `HEAD` — the old trunk) at the wrong path is repointed (self-heal),
//! and an undeclared one is GC'd on the same test. A real file/dir, or a **foreign**
//! symlink (a user's, pointing outside the trunk), is reported as a `conflict` and
//! never touched — unless `Fix::Force` backs it up first
//! (`renameat` to `<leaf>.grove-bak[-N]`, then link). The `Fix::Force` backup runs
//! unlocked, so two concurrent realizers could race the backup name; in practice
//! mutations for a given root are serialized on that root's lane (distinct roots run
//! their lanes in parallel, but never share a worktree), and `Force` is an explicit
//! operator action. **The lane is the caller's to supply** — this crate has no
//! per-root mutex of its own; a concurrent daemon must serialize per root or this
//! module loses an invariant it was written against.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, fstat, mkdirat, open, openat, readlinkat, renameat, statat,
    symlinkat, unlinkat,
};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::layout::LEGACY_TRUNK;
use crate::manifest::{self, Share, ShareMode};
use crate::roots::{self, TrunkState, code_dir, list as list_roots, manifest_path};
use crate::{Error, worktrees};

/// How aggressively `materialize` resolves a real-file conflict at a share
/// destination. `Safe` reports it (never clobbers); `Force` backs the real file up
/// (`<leaf>.grove-bak[-N]`) then links — atomic, never overwriting a prior backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix {
    Safe,
    Force,
}

/// One row of a doctor report: the outcome of converging a single share at a single
/// location. `worktree` is `None` for a source-level row (the trunk's own copy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareOutcome {
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    pub path: String,
    pub status: ShareStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Fine-grained per-row status. The doctor exit-code/report view coarsens these to
/// `ok | fixed | conflict | error` via [`ShareStatus::is_conflict`]/[`is_error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareStatus {
    Ok,
    Created,
    Linked,
    Copied,
    Repointed,
    Conflict,
    Gc,
    Error,
}

impl ShareStatus {
    #[must_use]
    pub fn is_error(self) -> bool {
        matches!(self, ShareStatus::Error)
    }

    #[must_use]
    pub fn is_conflict(self) -> bool {
        matches!(self, ShareStatus::Conflict)
    }
}

/// Read-only twin of [`materialize`] — the same walk, classify-only, **mutates
/// nothing** (the `--dry-run` contract; a byte-for-byte FS snapshot is identical
/// before and after). `would-link`/`would-repoint`/`would-create`/`would-gc` reuse
/// the `Linked`/`Repointed`/`Created`/`Gc` statuses (the renderer + exit code only
/// branch on `Conflict`/`Error`, so the would/did distinction is immaterial there).
pub fn diagnose(home: &Path, slug: Option<&str>) -> Result<Vec<ShareOutcome>, Error> {
    run(home, slug, Action::Diagnose).map_err(Error::io)
}

/// Converge every declared share for `slug` (or all roots when `None`): create the
/// trunk source if missing, link/repoint it into every present worktree, GC
/// orphaned grove links. Best-effort — one bad share/worktree pushes an `Error`
/// row and never aborts the batch.
// ※ lane-is-callers
pub fn materialize(home: &Path, slug: Option<&str>, fix: Fix) -> Result<Vec<ShareOutcome>, Error> {
    run(home, slug, Action::Materialize(fix)).map_err(Error::io)
}

/// Take back what grove laid in worktree `name` ahead of its non-forced `git worktree
/// remove`, which counts shares as untracked files and refuses over them. Only a
/// declared share path is touched, and there only a grove-shaped link or a real file
/// byte-identical to its trunk source — both data-free, re-laid by the next
/// materialize. A path git tracks is git's, whatever its bytes; an edited copy is the
/// user's and still blocks; so is everything else.
/// Never the trunk itself, whose share paths ARE the sources — told by identity, not
/// name, since an APFS twin of its name opens the trunk. Best-effort per share.
pub(crate) fn unlay(home: &Path, slug: &str, name: &str) -> Result<()> {
    let trunk = roots::trunk(home, slug)?;
    let root_fd = open_dir(&code_dir(home, slug))?;
    let trunk_fd = open_dir_nofollow(&root_fd, OsStr::new(&trunk.name))?;
    let wt_fd = open_dir_nofollow(&root_fd, OsStr::new(name))?;
    let [t, w] = [&trunk_fd, &wt_fd].map(|fd| fstat(fd).map(|s| (s.st_dev, s.st_ino)));
    if t? == w? {
        return Ok(());
    }
    let shares = manifest::list_shares(&manifest_path(home), slug)?;
    let paths: Vec<&str> = shares.iter().map(|s| s.path.as_str()).collect();
    let tracked = crate::git::tracked(&code_dir(home, slug).join(name), &paths)?;
    for share in shares.iter().filter(|s| !tracked.contains(&s.path)) {
        let rel = Path::new(&share.path);
        let _ = (|| -> Result<()> {
            let (parent_fd, leaf) = descend_parent(&wt_fd, rel, false)?.context("never laid")?;
            let ours = match classify_leaf(&parent_fd, &leaf)? {
                LeafState::Symlink { target } => is_under_trunk(&trunk.name, rel, &target),
                LeafState::RealFile => read_source(&trunk_fd, rel)
                    .is_ok_and(|(src, _)| read_source(&wt_fd, rel).is_ok_and(|(b, _)| b == src)),
                LeafState::Absent | LeafState::Dir | LeafState::Other => false,
            };
            if ours {
                unlinkat(&parent_fd, &leaf, AtFlags::empty())?;
            }
            Ok(())
        })();
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Action {
    Diagnose,
    Materialize(Fix),
}

impl Action {
    fn mutates(self) -> bool {
        matches!(self, Action::Materialize(_))
    }
}

fn run(home: &Path, slug: Option<&str>, action: Action) -> Result<Vec<ShareOutcome>> {
    let slugs = match slug {
        Some(s) => vec![s.to_string()],
        None => list_roots(home)?.into_iter().map(|r| r.slug).collect(),
    };
    let mut out = Vec::new();
    for slug in &slugs {
        run_root(home, slug, action, None, &mut out);
    }
    Ok(out)
}

/// The [`materialize`] pass `converge_trunk` runs across a trunk change, before it
/// moves `HEAD`: links through `prev_trunk` are grove's own to repoint, and a source
/// missing from the new trunk is seeded from `prev_trunk`'s.
pub(crate) fn carry(home: &Path, slug: &str, prev_trunk: &str) -> Vec<ShareOutcome> {
    let mut out = Vec::new();
    run_root(
        home,
        slug,
        Action::Materialize(Fix::Safe),
        Some(prev_trunk),
        &mut out,
    );
    out
}

/// The per-root constants every row of a pass carries: the slug it reports under, the
/// name of the trunk directory its links point through, the previous trunk — the
/// trunk itself except in [`carry`] — and whether a trunk change is pending outside
/// it. Bundled so the row helpers keep a readable arity.
#[derive(Clone, Copy)]
struct Pass<'a> {
    slug: &'a str,
    trunk_name: &'a str,
    prev_trunk: &'a str,
    pending: bool,
}

impl Pass<'_> {
    fn owns(self, rel: &Path, target: &Path) -> bool {
        is_under_trunk(self.trunk_name, rel, target) || is_under_trunk(self.prev_trunk, rel, target)
    }
}

fn run_root(
    home: &Path,
    slug: &str,
    action: Action,
    prev_trunk: Option<&str>,
    out: &mut Vec<ShareOutcome>,
) {
    // Defense in depth — `doctor <slug>` takes user input; a bad slug never builds a path.
    if manifest::validate_slug(slug).is_err() {
        out.push(err(slug, None, "", "invalid slug"));
        return;
    }
    let root = code_dir(home, slug);
    // Which directory is the trunk is a manifest question, so it is asked once per
    // root and carried through every row below — never re-derived per share. A `HEAD`
    // git cannot read is no pending change: the pass runs on the declared trunk.
    let state = match roots::trunk_state(home, slug)
        .or_else(|_| roots::trunk(home, slug).map(|trunk| TrunkState::Settled { trunk }))
    {
        Ok(state) => state,
        Err(e) => return out.push(err(slug, None, "", &format!("resolve trunk: {e:#}"))),
    };
    let trunk = state.trunk();
    if !trunk.dir.exists() {
        out.push(err(
            slug,
            None,
            "",
            "trunk missing — clone/realize the root first",
        ));
        return;
    }
    let pass = Pass {
        slug,
        trunk_name: &trunk.name,
        prev_trunk: prev_trunk.unwrap_or(&trunk.name),
        // `HEAD` off the declared trunk: a change only [`carry`] can make, so a source
        // missing here is its to seed, never this pass's to create empty. Detection only —
        // ownership is still `trunk_name`/`prev_trunk`'s.
        pending: prev_trunk.is_none() && matches!(state, TrunkState::Changing { .. }),
    };

    let root_fd = match open_dir(&root) {
        Ok(fd) => fd,
        Err(e) => return out.push(err(slug, None, "", &format!("open root: {e:#}"))),
    };
    let trunk_fd = match open_dir_nofollow(&root_fd, OsStr::new(&trunk.name)) {
        Ok(fd) => fd,
        Err(e) => {
            return out.push(err(slug, None, "", &format!("open {}: {e:#}", trunk.name)));
        }
    };
    // The previous trunk seeds a missing source; outside [`carry`] that is the trunk
    // itself, where an absent source is absent still, so the seed falls through to empty.
    // Only a missing previous trunk seeds nothing: one that cannot be opened holds.
    let prev_fd = match open_dir_nofollow(&root_fd, OsStr::new(pass.prev_trunk)) {
        Ok(fd) => Some(fd),
        Err(e) if e.downcast_ref() == Some(&Errno::NOENT) => None,
        Err(e) => {
            return out.push(err(
                slug,
                None,
                "",
                &format!("open {}: {e:#}", pass.prev_trunk),
            ));
        }
    };
    let declared = match manifest::list_shares(&manifest_path(home), slug) {
        Ok(d) => d,
        Err(e) => return out.push(err(slug, None, "", &format!("read shares: {e:#}"))),
    };

    // 1. Source pass — ensure each `<trunk>/<p>` exists. A carry is all-or-nothing: a
    // dry run first, so a source it cannot seed holds it before any seed is written, and
    // a failed row returns before a link is moved off the trunk `HEAD` has not left — or,
    // while a change is pending, laid onto the new trunk's still-missing source.
    let sources = |action| -> Vec<_> {
        declared
            .iter()
            .map(|share| source_outcome(pass, action, &trunk_fd, prev_fd.as_ref(), share))
            .collect()
    };
    let failed = |rows: &[ShareOutcome]| rows.iter().any(|o| o.status.is_error());
    let rows = prev_trunk
        .map(|_| sources(Action::Diagnose))
        .filter(|dry| failed(dry))
        .unwrap_or_else(|| sources(action));
    let held = (prev_trunk.is_some() || pass.pending) && failed(&rows);
    out.extend(rows);
    if held {
        return;
    }

    // 2/3. Link + GC pass, per present worktree (never the trunk/reserved dirs).
    for wt in present_worktrees(home, slug) {
        let wt_fd = match open_dir_nofollow(&root_fd, OsStr::new(&wt)) {
            Ok(fd) => fd,
            Err(e) => {
                out.push(err(slug, Some(&wt), "", &format!("open worktree: {e:#}")));
                continue;
            }
        };
        for share in &declared {
            out.push(match share.mode {
                ShareMode::Symlink => link_outcome(pass, &wt, action, &wt_fd, &share.path),
                ShareMode::Copy => copy_outcome(pass, &wt, action, &trunk_fd, &wt_fd, &share.path),
            });
        }
        gc_worktree(pass, &wt, &root.join(&wt), &wt_fd, &declared, action, out);
    }
}

/// Source row: classify `<trunk>/<p>`, create it when missing (materialize) — seeded
/// from the previous trunk's `<p>` across a trunk change, so a link repointed onto
/// the new trunk reads the same bytes (a symlink source as the same link); empty only
/// when that source is absent too, and an `Error` row, nothing created, when it cannot
/// be read or the change is pending. The seed is read on a dry run too, so a carry's
/// dry run fails exactly where its real pass would.
fn source_outcome<Fd: AsFd>(
    pass: Pass,
    action: Action,
    trunk_fd: Fd,
    prev_fd: Option<&OwnedFd>,
    share: &Share,
) -> ShareOutcome {
    let rel = Path::new(&share.path);
    let seed = || -> Result<_> {
        let seed = prev_fd
            .map(|fd| read_present(fd, rel))
            .transpose()?
            .flatten();
        // A relative target that never climbs out of the trunk names the old trunk's own
        // file: carried verbatim it would dangle in the new one.
        if let Some(Seed::Link(target)) = &seed
            && target.is_relative()
            && rel
                .with_file_name(target)
                .components()
                .try_fold(0, |depth: u32, c| match c {
                    Component::ParentDir => depth.checked_sub(1),
                    Component::Normal(_) => Some(depth + 1),
                    _ => Some(depth),
                })
                .is_some()
        {
            bail!(
                "symlink source points inside the old trunk; re-create it in {}",
                pass.trunk_name
            );
        }
        Ok(seed)
    };
    let result = (|| -> Result<ShareStatus> {
        match descend_parent(&trunk_fd, rel, action.mutates())? {
            // parent missing, diagnose → would-create
            None => seed().map(|_| ShareStatus::Created),
            Some((parent_fd, leaf)) => {
                let state = classify_leaf(&parent_fd, &leaf)?;
                // The share link grove laid where a carry's source goes, while the new
                // trunk was a worktree, is no source of the user's: seeded over, as if
                // absent — unless it reads a directory, which no seed carries.
                let laid = matches!(&state, LeafState::Symlink { target }
                    if pass.prev_trunk != pass.trunk_name
                        && *target == trunk_relative_target(pass.prev_trunk, rel)
                        && !statat(&parent_fd, &leaf, AtFlags::empty())
                            .is_ok_and(|s| FileType::from_raw_mode(s.st_mode).is_dir()));
                match if laid { LeafState::Absent } else { state } {
                    LeafState::Absent => {
                        if pass.pending {
                            bail!("trunk change pending; reconcile carries it");
                        }
                        let seed = seed()?;
                        if action.mutates() {
                            // a file seed renames over the link; a link or empty one cannot
                            if laid && !matches!(seed, Some(Seed::File(..))) {
                                unlinkat(&parent_fd, &leaf, AtFlags::empty())
                                    .context("unlink the share link in the source's place")?;
                            }
                            match seed {
                                Some(Seed::File(bytes, mode)) => {
                                    write_file(&parent_fd, &leaf, &bytes, mode)?;
                                }
                                Some(Seed::Link(target)) => link_leaf(&parent_fd, &leaf, &target)?,
                                None => create_empty(&parent_fd, &leaf)?,
                            }
                        }
                        Ok(ShareStatus::Created)
                    }
                    // A real file or pre-made dir IS the source; leave it. And D5: a
                    // *symlink* where the source belongs is the user's own choice of
                    // source — respected too, never unlinked/replaced. A `_.symlink` share
                    // reads *through* it (worktree → `<trunk>/<p>` → the user's target); a
                    // `_.copy` share's `NOFOLLOW` source open refuses it downstream (a copy
                    // from a link is reported per-worktree, not silently followed).
                    LeafState::RealFile | LeafState::Dir | LeafState::Symlink { .. } => {
                        Ok(ShareStatus::Ok)
                    }
                    // A non-file/dir/symlink oddity (fifo, socket, …) can't serve as a
                    // source — it's inside the trunk (grove's domain), so replace it.
                    LeafState::Other => {
                        if action.mutates() {
                            unlinkat(&parent_fd, &leaf, AtFlags::empty())
                                .context("unlink non-file source")?;
                            create_empty(&parent_fd, &leaf)?;
                        }
                        Ok(ShareStatus::Created)
                    }
                }
            }
        }
    })();
    match result {
        Ok(status) => ok(pass.slug, None, &share.path, status),
        Err(e) => err(pass.slug, None, &share.path, &format!("{e:#}")),
    }
}

/// Link row: classify `<wt>/<p>`, link/repoint to `../…/<trunk>/<p>`, never clobber.
// ※ never-clobber
fn link_outcome<Fd: AsFd>(
    pass: Pass,
    wt: &str,
    action: Action,
    wt_fd: Fd,
    path: &str,
) -> ShareOutcome {
    let rel = Path::new(path);
    let target = trunk_relative_target(pass.trunk_name, rel);
    let result = (|| -> Result<ShareStatus> {
        let Some((parent_fd, leaf)) = descend_parent(&wt_fd, rel, action.mutates())? else {
            return Ok(ShareStatus::Linked); // parent missing, diagnose → would-link
        };
        match classify_leaf(&parent_fd, &leaf)? {
            LeafState::Absent => {
                if action.mutates() {
                    link_leaf(&parent_fd, &leaf, &target)?;
                }
                Ok(ShareStatus::Linked)
            }
            LeafState::Symlink { target: cur } if cur == target => Ok(ShareStatus::Ok),
            // A *grove-shaped* link (points under the trunk) but at the wrong path is
            // a stale grove link — self-heal by repointing.
            LeafState::Symlink { target: cur } if pass.owns(rel, &cur) => {
                if action.mutates() {
                    repoint_leaf(&parent_fd, &leaf, &target)?;
                }
                Ok(ShareStatus::Repointed)
            }
            // A FOREIGN symlink (user-authored, points elsewhere), a real file, or a
            // dir: never silently clobbered — report a conflict (or, with `--fix`,
            // back it up then link).
            LeafState::Symlink { .. } | LeafState::RealFile | LeafState::Dir | LeafState::Other => {
                match action {
                    Action::Materialize(Fix::Force) => {
                        backup_then_link(&parent_fd, &leaf, &target).map(|_| ShareStatus::Created)
                    }
                    _ => Ok(ShareStatus::Conflict),
                }
            }
        }
    })();
    match result {
        Ok(ShareStatus::Conflict) => ShareOutcome {
            slug: pass.slug.into(),
            worktree: Some(wt.into()),
            path: path.into(),
            status: ShareStatus::Conflict,
            reason: Some(
                "a real file/dir/foreign symlink exists here; not clobbered (use --fix)".into(),
            ),
        },
        Ok(status) => ok(pass.slug, Some(wt), path, status),
        Err(e) => err(pass.slug, Some(wt), path, &format!("{e:#}")),
    }
}

/// Copy row: seed `<wt>/<p>` as an independent real file from `<trunk>/<p>`, once.
/// Written only into an empty slot (or over a stale *grove-owned* symlink — the
/// mode migration from a prior `_.symlink` declaration; data-free, safe to replace).
/// A real file, a foreign symlink, or a dir already there is the worktree's own:
/// seed-once **never clobbers**, and `--fix` has no bearing (a copy has no "wrong
/// state" to heal — it is fire-and-forget).
fn copy_outcome<Tf: AsFd, Wf: AsFd>(
    pass: Pass,
    wt: &str,
    action: Action,
    trunk_fd: Tf,
    wt_fd: Wf,
    path: &str,
) -> ShareOutcome {
    let rel = Path::new(path);
    let result = (|| -> Result<ShareStatus> {
        let Some((parent_fd, leaf)) = descend_parent(&wt_fd, rel, action.mutates())? else {
            return Ok(ShareStatus::Copied); // parent missing, diagnose → would-copy
        };
        let seed = match classify_leaf(&parent_fd, &leaf)? {
            LeafState::Absent => true,
            // A grove-owned symlink (prior `_.symlink` for this path) is data-free —
            // replace it with the seeded copy (symlink→copy migration).
            LeafState::Symlink { target } if pass.owns(rel, &target) => true,
            // Foreign symlink / real file / dir: the worktree owns it. Leave it.
            LeafState::Symlink { .. } | LeafState::RealFile | LeafState::Dir | LeafState::Other => {
                false
            }
        };
        if !seed {
            return Ok(ShareStatus::Ok);
        }
        if action.mutates() {
            let (bytes, mode) = read_source(&trunk_fd, rel)?;
            write_file(&parent_fd, &leaf, &bytes, mode)?;
        }
        Ok(ShareStatus::Copied)
    })();
    match result {
        Ok(status) => ok(pass.slug, Some(wt), path, status),
        Err(e) => err(pass.slug, Some(wt), path, &format!("{e:#}")),
    }
}

/// GC pass: a worktree top-level symlink pointing through the trunk
/// (`../<trunk>/…`) for a `<name>` no longer declared is an orphan — remove it
/// (data-free). A real file (incl. a `_.copy`'s seeded file — indistinguishable from
/// a user's, and owned by the worktree), or a symlink pointing elsewhere, is left
/// alone. Top-level only in slice 1 (nested-share GC is a later refinement).
fn gc_worktree<Fd: AsFd>(
    pass: Pass,
    wt: &str,
    wt_path: &Path,
    wt_fd: Fd,
    declared: &[Share],
    action: Action,
    out: &mut Vec<ShareOutcome>,
) {
    let declared_paths: HashSet<&str> = declared.iter().map(|s| s.path.as_str()).collect();
    // Enumerate names via the path (read-only); the unlink decision is fd-pinned.
    let entries = match std::fs::read_dir(wt_path) {
        Ok(entries) => entries,
        Err(e) => {
            return out.push(err(
                pass.slug,
                Some(wt),
                "",
                &format!("read worktree: {e:#}"),
            ));
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if declared_paths.contains(name_str) {
            continue; // still declared — leave it
        }
        // Grove-shaped is the one ownership test in this module: the same predicate
        // that lets a stale link be repointed lets an undeclared one be collected, so
        // a link laid through the legacy trunk name is GC'd rather than stranded.
        let gc = if action.mutates() {
            gc_leaf(&wt_fd, &name, pass)
        } else {
            is_grove_link(&wt_fd, &name, pass)
        };
        if let Ok(true) = gc {
            out.push(ok(pass.slug, Some(wt), name_str, ShareStatus::Gc));
        }
    }
}

// --- the four primitives, each on a pinned dirfd --------------------------------

/// The current state of a leaf, classified WITHOUT following a symlink.
enum LeafState {
    Absent,
    Symlink { target: PathBuf },
    RealFile,
    Dir,
    Other,
}

/// Open `<base>` as a directory, refusing a symlinked final component (`NOFOLLOW`).
fn open_dir(base: &Path) -> Result<OwnedFd> {
    open(
        base,
        OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("open dir {}", base.display()))
}

/// One `NOFOLLOW` dir hop from a pinned parent — the ONLY place a parent component
/// is opened, so the one place `NOFOLLOW` can be forgotten. `ELOOP` if `seg` is a
/// symlink (it is never traversed): the symlinked-parent-escape defense.
fn open_dir_nofollow<Fd: AsFd>(parent: Fd, seg: &OsStr) -> Result<OwnedFd> {
    openat(
        parent,
        seg,
        OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("open dir {} (nofollow)", seg.display()))
}

/// Descend the PARENT components of `rel` from `root`, returning `(parent-dirfd,
/// leaf)`. Each hop is `open_dir_nofollow` → `ELOOP` on a symlinked component.
/// `create` mkdir's missing parents (materialize); when `false` (diagnose) a missing
/// parent returns `None` (no mutation) — the caller treats the leaf as would-create.
fn descend_parent<Fd: AsFd>(
    root: Fd,
    rel: &Path,
    create: bool,
) -> Result<Option<(OwnedFd, OsString)>> {
    let mut segs: Vec<&OsStr> = rel
        .components()
        .map(std::path::Component::as_os_str)
        .collect();
    let leaf = segs.pop().context("share path has no leaf")?.to_os_string();
    let mut cur = open_dir_nofollow(&root, OsStr::new("."))?;
    for seg in segs {
        match openat(
            &cur,
            seg,
            OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => cur = fd,
            Err(Errno::NOENT) if create => {
                mkdirat(&cur, seg, Mode::from(0o755))
                    .with_context(|| format!("mkdir {}", seg.display()))?;
                cur = open_dir_nofollow(&cur, seg)?;
            }
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("descend {}", seg.display())),
        }
    }
    Ok(Some((cur, leaf)))
}

/// `statat(SYMLINK_NOFOLLOW)` → `FileType`. Read-only; used by diagnose AND materialize.
fn classify_leaf<Fd: AsFd>(dirfd: Fd, leaf: &OsStr) -> Result<LeafState> {
    let stat = match statat(&dirfd, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(s) => s,
        Err(Errno::NOENT) => return Ok(LeafState::Absent),
        Err(e) => return Err(e).with_context(|| format!("stat {}", leaf.display())),
    };
    Ok(match FileType::from_raw_mode(stat.st_mode) {
        FileType::Symlink => {
            let raw = readlinkat(&dirfd, leaf, Vec::new())
                .with_context(|| format!("readlink {}", leaf.display()))?;
            LeafState::Symlink {
                target: PathBuf::from(OsString::from_vec(raw.into_bytes())),
            }
        }
        FileType::RegularFile => LeafState::RealFile,
        FileType::Directory => LeafState::Dir,
        _ => LeafState::Other,
    })
}

/// `symlinkat(target, dirfd, leaf)` — exclusive (`EEXIST` surfaces as an error; the
/// caller only links a classified-`Absent` leaf).
fn link_leaf<Fd: AsFd>(dirfd: Fd, leaf: &OsStr, target: &Path) -> Result<()> {
    symlinkat(target, &dirfd, leaf)
        .with_context(|| format!("symlink {} -> {}", leaf.display(), target.display()))
}

/// Repoint a wrong/dangling link atomically: temp-`symlinkat` then same-dirfd
/// `renameat` over the leaf — never a torn/half link, no path re-resolution.
fn repoint_leaf<Fd: AsFd>(dirfd: Fd, leaf: &OsStr, target: &Path) -> Result<()> {
    let tmp = tmp_sibling(leaf);
    let _ = unlinkat(&dirfd, &tmp, AtFlags::empty()); // clear a stale temp, best-effort
    symlinkat(target, &dirfd, &tmp).with_context(|| format!("symlink tmp {}", tmp.display()))?;
    renameat(&dirfd, &tmp, &dirfd, leaf)
        .with_context(|| format!("rename {} -> {}", tmp.display(), leaf.display()))
}

/// Create the empty trunk source: `openat(CREATE|EXCL|WRONLY|NOFOLLOW, 0o600)`.
/// `EXCL` closes the classify→create TOCTOU; `NOFOLLOW` refuses a planted redirect.
fn create_empty<Fd: AsFd>(dirfd: Fd, leaf: &OsStr) -> Result<()> {
    let fd = openat(
        &dirfd,
        leaf,
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from(0o600),
    )
    .with_context(|| format!("create source {}", leaf.display()))?;
    drop(fd);
    Ok(())
}

/// Read a `_.copy` source (`<trunk>/<rel>`) → `(bytes, perm-bits)`. Descends the
/// trunk with the same `NOFOLLOW` discipline; the leaf is opened `RDONLY|NOFOLLOW`,
/// so a source that is (or became) a symlink/dir is refused rather than followed.
/// The source pass guarantees a regular-file source exists before any worktree copy.
fn read_source<Fd: AsFd>(trunk_fd: Fd, rel: &Path) -> Result<(Vec<u8>, Mode)> {
    match read_present(trunk_fd, rel)?.context("copy source missing under the trunk")? {
        Seed::File(bytes, mode) => Ok((bytes, mode)),
        Seed::Link(_) => bail!("copy source is not a regular file"),
    }
}

/// A present source, as a trunk change seeds the new trunk's from it: a file's bytes and
/// perm bits, or a symlink's target — the user's own choice of source, carried as is.
enum Seed {
    File(Vec<u8>, Mode),
    Link(PathBuf),
}

/// [`read_source`], with an absent source an `Ok(None)` rather than an error — the
/// one failure a trunk change may answer by creating the new source empty — and a
/// symlink source its target, never followed.
fn read_present<Fd: AsFd>(trunk_fd: Fd, rel: &Path) -> Result<Option<Seed>> {
    let Some((parent_fd, leaf)) = descend_parent(&trunk_fd, rel, false)? else {
        return Ok(None);
    };
    match classify_leaf(&parent_fd, &leaf)? {
        LeafState::RealFile => {}
        LeafState::Dir => bail!("copy source is a directory; only files are supported"),
        LeafState::Absent => return Ok(None),
        LeafState::Symlink { target } => return Ok(Some(Seed::Link(target))),
        LeafState::Other => bail!("copy source is not a regular file"),
    }
    let stat = statat(&parent_fd, &leaf, AtFlags::SYMLINK_NOFOLLOW)
        .with_context(|| format!("stat source {}", leaf.display()))?;
    let fd = openat(
        &parent_fd,
        &leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("open source {}", leaf.display()))?;
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read source {}", leaf.display()))?;
    Ok(Some(Seed::File(
        bytes,
        Mode::from_bits_truncate(stat.st_mode),
    )))
}

/// Write a `_.copy` leaf atomically: a temp sibling created `CREATE|EXCL|WRONLY|
/// NOFOLLOW` on the pinned dirfd (carrying the source's perm bits), filled, then
/// `renameat` over the leaf — replacing an absent slot or a stale grove symlink with
/// a whole file, never a torn one. Mirrors the `repoint_leaf` temp+rename idiom.
fn write_file<Fd: AsFd>(dirfd: Fd, leaf: &OsStr, bytes: &[u8], mode: Mode) -> Result<()> {
    let tmp = tmp_sibling(leaf);
    let _ = unlinkat(&dirfd, &tmp, AtFlags::empty()); // clear a stale temp, best-effort
    let fd = openat(
        &dirfd,
        &tmp,
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        mode,
    )
    .with_context(|| format!("create temp {}", tmp.display()))?;
    std::fs::File::from(fd)
        .write_all(bytes)
        .with_context(|| format!("write {}", tmp.display()))?;
    renameat(&dirfd, &tmp, &dirfd, leaf)
        .with_context(|| format!("rename {} -> {}", tmp.display(), leaf.display()))
}

/// GC a leaf IFF it is a symlink grove's own — one [`Pass::owns`]. A real
/// file or foreign-target symlink is left untouched. `unlinkat` removes the LINK,
/// never its target.
fn gc_leaf<Fd: AsFd>(dirfd: Fd, leaf: &OsStr, pass: Pass) -> Result<bool> {
    if is_grove_link(&dirfd, leaf, pass)? {
        unlinkat(&dirfd, leaf, AtFlags::empty())
            .with_context(|| format!("gc {}", leaf.display()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn is_grove_link<Fd: AsFd>(dirfd: Fd, leaf: &OsStr, pass: Pass) -> Result<bool> {
    Ok(
        matches!(classify_leaf(dirfd, leaf)?, LeafState::Symlink { target } if pass.owns(Path::new(leaf), &target)),
    )
}

/// Back up a real-file conflict (`Fix::Force`): rename the real file to a
/// collision-free `<leaf>.grove-bak[-N]` (never overwriting a prior backup), then
/// link. Returns the backup name. Same dirfd throughout — no second descent.
fn backup_then_link<Fd: AsFd>(dirfd: Fd, leaf: &OsStr, target: &Path) -> Result<OsString> {
    let bak = free_backup_name(&dirfd, leaf)?;
    renameat(&dirfd, leaf, &dirfd, &bak)
        .with_context(|| format!("back up {} -> {}", leaf.display(), bak.display()))?;
    link_leaf(&dirfd, leaf, target)?;
    Ok(bak)
}

/// First `<leaf>.grove-bak[-N]` that doesn't already exist (no timestamp — clocks
/// are unavailable here, and a stable suffix is more legible anyway).
fn free_backup_name<Fd: AsFd>(dirfd: Fd, leaf: &OsStr) -> Result<OsString> {
    let base = format!("{}.grove-bak", leaf.to_string_lossy());
    for n in 0..1000 {
        let cand = if n == 0 {
            OsString::from(&base)
        } else {
            OsString::from(format!("{base}-{n}"))
        };
        if matches!(classify_leaf(&dirfd, &cand)?, LeafState::Absent) {
            return Ok(cand);
        }
    }
    bail!("no free backup name for {}", leaf.display())
}

/// Grove-controlled relative symlink target from a worktree-rooted share path: one
/// `..` per directory level above the leaf, then `<trunk>/<p>`. Both `<wt>` and the
/// trunk are siblings in the code dir, so the answer is purely a function of
/// `<p>`'s depth. e.g. `.env` → `../main/.env`; `config/db.toml` →
/// `../../main/config/db.toml`. Pure — no I/O.
///
/// CONSTRAINT: the sibling assumption is load-bearing. A worktree anywhere else (a
/// warm-pool slot, which lives under the root dir rather than beside the trunk) gets
/// targets that resolve somewhere else entirely and dangle — materialize into such a
/// tree only after it moves to canonical sibling depth, or teach this function a
/// worktree-address axis first. See `docs/worktrees.html#on-disk-layout`.
fn trunk_relative_target(trunk: &str, p: &Path) -> PathBuf {
    let mut target = PathBuf::new();
    for _ in 0..p.components().count() {
        target.push("..");
    }
    target.push(trunk);
    target.push(p);
    target
}

/// Is the symlink at worktree-relative `rel` grove's own — its target the exact climb
/// [`trunk_relative_target`] writes there, one `..` per component of `rel`, then
/// `<trunk>/…`, with the trunk spelled either by its branch-derived name or as
/// [`LEGACY_TRUNK`]? Such a link is a (possibly stale) grove link we may repoint or GC;
/// anything else — an in-tree `.trunk/…` included — is a user's foreign symlink we must
/// never clobber. Purely lexical (no I/O, no canonicalization).
fn is_under_trunk(trunk: &str, rel: &Path, target: &Path) -> bool {
    let mut c = target.components();
    rel.components()
        .all(|_| c.next() == Some(Component::ParentDir))
        && matches!(
            c.next(),
            Some(Component::Normal(s)) if s == OsStr::new(trunk) || s == OsStr::new(LEGACY_TRUNK)
        )
}

fn tmp_sibling(leaf: &OsStr) -> OsString {
    OsString::from(format!(".{}.grove-tmp", leaf.to_string_lossy()))
}

fn present_worktrees(home: &Path, slug: &str) -> Vec<String> {
    worktrees::list(home, slug)
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.present)
        .map(|w| w.name)
        .collect()
}

fn ok(slug: &str, worktree: Option<&str>, path: &str, status: ShareStatus) -> ShareOutcome {
    ShareOutcome {
        slug: slug.into(),
        worktree: worktree.map(Into::into),
        path: path.into(),
        status,
        reason: None,
    }
}

fn err(slug: &str, worktree: Option<&str>, path: &str, reason: &str) -> ShareOutcome {
    ShareOutcome {
        slug: slug.into(),
        worktree: worktree.map(Into::into),
        path: path.into(),
        status: ShareStatus::Error,
        reason: Some(reason.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A home with one cloned root `o/r` (bare + a `main` trunk checkout) and a
    /// worktree. `main` is the trunk directory throughout: it is what `name_for` of
    /// the fixture source's default branch comes to.
    fn home_with_root(tmp: &TempDir) -> PathBuf {
        crate::testfix::home_with_root_and_worktree(tmp)
    }

    fn declare(home: &Path, paths: &[&str]) {
        manifest::add_share(&manifest_path(home), "o/r", "symlink", paths).unwrap();
    }

    fn declare_copy(home: &Path, paths: &[&str]) {
        manifest::add_share(&manifest_path(home), "o/r", "copy", paths).unwrap();
    }

    fn mkfifo(path: &Path) {
        let made = std::process::Command::new("mkfifo").arg(path).status();
        assert!(made.unwrap().success());
    }

    fn wt(home: &Path, name: &str) -> PathBuf {
        home.join("code/o/r").join(name)
    }

    fn find<'a>(out: &'a [ShareOutcome], wt: Option<&str>, path: &str) -> &'a ShareOutcome {
        out.iter()
            .find(|o| o.worktree.as_deref() == wt && o.path == path)
            .unwrap_or_else(|| panic!("no outcome for {wt:?} {path}"))
    }

    fn snapshot_into(dir: &Path, base: &Path, acc: &mut Vec<(PathBuf, bool, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_path_buf();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            if meta.is_symlink() {
                acc.push((
                    rel,
                    true,
                    std::fs::read_link(&p).unwrap().into_os_string().into_vec(),
                ));
            } else if meta.is_dir() {
                acc.push((rel, false, Vec::new()));
                snapshot_into(&p, base, acc);
            } else {
                acc.push((rel, false, std::fs::read(&p).unwrap_or_default()));
            }
        }
    }

    /// Recursive (path, `is_symlink`, target-or-bytes) snapshot — for proving a
    /// read-only walk mutates nothing. Symlinks are recorded by target, not followed.
    fn snapshot(root: &Path) -> Vec<(PathBuf, bool, Vec<u8>)> {
        let mut acc = Vec::new();
        snapshot_into(root, root, &mut acc);
        acc.sort();
        acc
    }

    #[test]
    fn share_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&ShareStatus::Conflict).unwrap(),
            "\"conflict\""
        );
        assert_eq!(serde_json::to_string(&ShareStatus::Gc).unwrap(), "\"gc\"");
    }

    #[test]
    fn share_outcome_omits_none_fields() {
        let json = serde_json::to_value(ok("o/r", None, ".env", ShareStatus::Ok)).unwrap();
        assert!(json.get("worktree").is_none(), "None worktree omitted");
        assert!(json.get("reason").is_none(), "None reason omitted");
    }

    #[test]
    fn trunk_relative_target_is_pure() {
        assert_eq!(
            trunk_relative_target("main", Path::new(".env")),
            Path::new("../main/.env")
        );
        assert_eq!(
            trunk_relative_target("canary", Path::new("config/db.toml")),
            Path::new("../../canary/config/db.toml")
        );
    }

    /// The grove-shaped test is against *this root's* trunk name, so a link through
    /// some other root's trunk name is a user's foreign symlink here — never repointed.
    #[test]
    fn a_grove_shaped_link_is_one_through_this_trunk() {
        let [env, db] = [".env", "config/db.toml"].map(Path::new);
        assert!(is_under_trunk("main", env, Path::new("../main/.env")));
        assert!(is_under_trunk(
            "canary",
            db,
            Path::new("../../canary/config/db.toml")
        ));
        assert!(!is_under_trunk(
            "main",
            db,
            Path::new("../main/config/db.toml")
        ));
        assert!(!is_under_trunk("main", env, Path::new("../canary/.env")));
        assert!(!is_under_trunk("main", env, Path::new("/etc/passwd")));
        // The legacy trunk directory is grove's whatever the trunk is named today.
        assert!(is_under_trunk("canary", env, Path::new("../.trunk/.env")));
        assert!(!is_under_trunk(
            "main",
            env,
            Path::new("../.trunk-backup/.env")
        ));
    }

    /// A link laid before the trunk was named by its branch points through the legacy
    /// directory. It is grove's own, so materialize repoints it onto the branch-named
    /// trunk — the migration needs no separate walk, only a share pass.
    #[test]
    fn materialize_repoints_a_legacy_trunk_link() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::os::unix::fs::symlink("../.trunk/.env", wt(&home, "feat").join(".env")).unwrap();
        declare(&home, &[".env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, Some("feat"), ".env").status,
            ShareStatus::Repointed
        );
        assert_eq!(
            std::fs::read_link(wt(&home, "feat").join(".env")).unwrap(),
            Path::new("../main/.env")
        );
    }

    /// The exact current target is already right: repointing it would churn the link
    /// (and the report) on every pass, so it is left byte-identical.
    #[test]
    fn materialize_leaves_an_exact_trunk_link_alone() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::os::unix::fs::symlink("../main/.env", wt(&home, "feat").join(".env")).unwrap();
        declare(&home, &[".env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(find(&out, Some("feat"), ".env").status, ShareStatus::Ok);
        assert_eq!(
            std::fs::read_link(wt(&home, "feat").join(".env")).unwrap(),
            Path::new("../main/.env")
        );
    }

    /// Accepting the legacy name widens what counts as grove's own by exactly one
    /// directory: a link through anything else — here a sibling worktree — is still
    /// the user's, and still a conflict.
    #[test]
    fn a_link_through_a_sibling_worktree_is_still_a_conflict() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::os::unix::fs::symlink("../other/.env", wt(&home, "feat").join(".env")).unwrap();
        declare(&home, &[".env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, Some("feat"), ".env").status,
            ShareStatus::Conflict
        );
        assert_eq!(
            std::fs::read_link(wt(&home, "feat").join(".env")).unwrap(),
            Path::new("../other/.env"),
            "the user's symlink target is preserved"
        );
    }

    /// Grove-shaped means exactly the climb grove writes: a user's in-tree link whose
    /// first component merely spells the trunk (trunk.io's `.trunk/configs/…`) climbs
    /// out of nothing, so it is neither collected nor repointed.
    #[test]
    fn an_in_tree_link_spelling_the_trunk_is_the_users() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let feat = wt(&home, "feat");
        for dir in ["main", ".trunk/configs"] {
            std::fs::create_dir_all(feat.join(dir)).unwrap();
        }
        std::fs::write(feat.join("main/cfg"), "").unwrap();
        std::fs::write(feat.join(".trunk/configs/x"), "").unwrap();
        std::os::unix::fs::symlink("main/cfg", feat.join("cfg")).unwrap();
        std::os::unix::fs::symlink(".trunk/configs/x", feat.join("x")).unwrap();
        declare(&home, &["cfg"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, Some("feat"), "cfg").status,
            ShareStatus::Conflict
        );
        assert!(out.iter().all(|o| o.status != ShareStatus::Gc));
        assert_eq!(
            std::fs::read_link(feat.join("cfg")).unwrap(),
            Path::new("main/cfg")
        );
        assert_eq!(
            std::fs::read_link(feat.join("x")).unwrap(),
            Path::new(".trunk/configs/x")
        );
    }

    #[test]
    fn classify_states() {
        let tmp = TempDir::new().unwrap();
        let dir = open_dir(tmp.path()).unwrap();
        assert!(matches!(
            classify_leaf(&dir, OsStr::new("absent")).unwrap(),
            LeafState::Absent
        ));

        std::fs::write(tmp.path().join("real"), "x").unwrap();
        assert!(matches!(
            classify_leaf(&dir, OsStr::new("real")).unwrap(),
            LeafState::RealFile
        ));

        std::os::unix::fs::symlink("../main/.env", tmp.path().join("link")).unwrap();
        assert!(matches!(
            classify_leaf(&dir, OsStr::new("link")).unwrap(),
            LeafState::Symlink { target } if target == Path::new("../main/.env")
        ));

        // A dangling link classifies as Symlink (target never followed/canonicalized).
        std::os::unix::fs::symlink("nowhere", tmp.path().join("dangling")).unwrap();
        assert!(matches!(
            classify_leaf(&dir, OsStr::new("dangling")).unwrap(),
            LeafState::Symlink { .. }
        ));
    }

    #[test]
    fn materialize_creates_source_and_links_worktree() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        // A fifo can serve as no source; inside the trunk it is grove's to replace.
        mkfifo(&wt(&home, "main").join(".fifo"));
        declare(&home, &[".env", ".fifo"]);

        let dry = diagnose(&home, Some("o/r")).unwrap();
        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        for p in [".env", ".fifo"] {
            assert_eq!(find(&dry, None, p).status, ShareStatus::Created, "{p}");
            assert_eq!(find(&out, None, p).status, ShareStatus::Created, "{p}");
            assert_eq!(find(&out, Some("feat"), p).status, ShareStatus::Linked);
            assert!(
                wt(&home, "main").join(p).is_file(),
                "empty source file in trunk"
            );
            assert_eq!(
                std::fs::read_link(wt(&home, "feat").join(p)).unwrap(),
                Path::new("../main").join(p)
            );
        }
    }

    #[test]
    fn materialize_is_a_live_share() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        declare(&home, &[".env"]);
        materialize(&home, Some("o/r"), Fix::Safe).unwrap();

        // Write the source; the worktree symlink resolves to it (live, not a copy).
        std::fs::write(wt(&home, "main").join(".env"), "SECRET").unwrap();
        assert_eq!(
            std::fs::read_to_string(wt(&home, "feat").join(".env")).unwrap(),
            "SECRET"
        );
    }

    #[test]
    fn materialize_leaves_an_existing_source_untouched_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "main").join(".env"), "PRESET").unwrap();
        declare(&home, &[".env"]);

        let first = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&first, None, ".env").status,
            ShareStatus::Ok,
            "existing source left alone"
        );
        assert_eq!(
            std::fs::read_to_string(wt(&home, "main").join(".env")).unwrap(),
            "PRESET"
        );

        let again = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert!(
            again.iter().all(|o| o.status == ShareStatus::Ok),
            "healthy tree → all no-ops on re-run: {again:?}"
        );
    }

    #[test]
    fn materialize_repoints_a_stale_grove_link() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        // A *grove-shaped* link (points under the trunk) at the wrong path — a stale
        // grove link from a prior name. Self-heal by repointing.
        std::os::unix::fs::symlink("../main/old-name", wt(&home, "feat").join(".env")).unwrap();
        declare(&home, &[".env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, Some("feat"), ".env").status,
            ShareStatus::Repointed
        );
        assert_eq!(
            std::fs::read_link(wt(&home, "feat").join(".env")).unwrap(),
            Path::new("../main/.env")
        );
    }

    /// D5: a symlink placed at the `<trunk>/<p>` *source* is the user's own choice of
    /// source — respected, never unlinked/replaced. A `_.symlink` share reads through
    /// it to the user's file.
    #[test]
    fn source_respects_a_user_symlink_in_the_trunk() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let real = tmp.path().join("real-secret");
        std::fs::write(&real, "USER SECRET").unwrap();
        // The user points the trunk source at their own file, via a symlink.
        std::os::unix::fs::symlink(&real, wt(&home, "main").join(".env")).unwrap();
        declare(&home, &[".env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, None, ".env").status,
            ShareStatus::Ok,
            "source symlink respected, not replaced"
        );
        assert!(
            wt(&home, "main").join(".env").is_symlink(),
            "user symlink preserved in the trunk"
        );
        // The worktree link reads THROUGH the trunk symlink to the user's file.
        assert_eq!(
            std::fs::read_to_string(wt(&home, "feat").join(".env")).unwrap(),
            "USER SECRET"
        );
    }

    #[test]
    fn materialize_treats_a_foreign_symlink_as_a_conflict() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        // A user's deliberate symlink pointing OUTSIDE the trunk — never clobbered.
        std::os::unix::fs::symlink("/tmp/my-secrets", wt(&home, "feat").join(".env")).unwrap();
        declare(&home, &[".env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, Some("feat"), ".env").status,
            ShareStatus::Conflict
        );
        assert_eq!(
            std::fs::read_link(wt(&home, "feat").join(".env")).unwrap(),
            Path::new("/tmp/my-secrets"),
            "the user's symlink target is preserved"
        );
    }

    #[test]
    fn materialize_never_clobbers_a_real_file() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "feat").join(".env"), "USER EDITED").unwrap();
        declare(&home, &[".env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        let o = find(&out, Some("feat"), ".env");
        assert_eq!(o.status, ShareStatus::Conflict);
        assert!(
            !wt(&home, "feat").join(".env").is_symlink(),
            "still a real file"
        );
        assert_eq!(
            std::fs::read_to_string(wt(&home, "feat").join(".env")).unwrap(),
            "USER EDITED"
        );
    }

    /// A conflict with no backup name left — all thousand taken, or one too long to
    /// stat — is an error row that leaves the real file where it is.
    #[test]
    fn fix_force_backs_up_then_links_uniquely_or_not_at_all() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let feat = wt(&home, "feat");
        std::fs::write(feat.join(".env"), "ORIGINAL").unwrap();
        // A prior backup already exists — the new one must not overwrite it.
        std::fs::write(feat.join(".env.grove-bak"), "OLD BACKUP").unwrap();
        std::fs::write(feat.join(".full"), "KEPT").unwrap();
        std::fs::write(feat.join(".full.grove-bak"), "").unwrap();
        for n in 1..1000 {
            std::fs::write(feat.join(format!(".full.grove-bak-{n}")), "").unwrap();
        }
        let long = format!(".{}", "l".repeat(249));
        std::fs::write(feat.join(&long), "KEPT").unwrap();
        declare(&home, &[".env", ".full", &long]);

        let out = materialize(&home, Some("o/r"), Fix::Force).unwrap();
        for (path, why) in [(".full", "no free backup name"), (&*long, "stat")] {
            let row = find(&out, Some("feat"), path);
            assert_eq!(row.status, ShareStatus::Error, "{path}");
            assert!(row.reason.as_deref().unwrap().contains(why), "{row:?}");
            assert_eq!(std::fs::read_to_string(feat.join(path)).unwrap(), "KEPT");
        }
        assert_ne!(
            find(&out, Some("feat"), ".env").status,
            ShareStatus::Conflict
        );
        assert!(wt(&home, "feat").join(".env").is_symlink(), "now linked");
        // The original bytes are preserved in a fresh backup; the old one is intact.
        assert_eq!(
            std::fs::read_to_string(wt(&home, "feat").join(".env.grove-bak")).unwrap(),
            "OLD BACKUP"
        );
        assert_eq!(
            std::fs::read_to_string(wt(&home, "feat").join(".env.grove-bak-1")).unwrap(),
            "ORIGINAL"
        );
    }

    #[test]
    fn materialize_handles_a_nested_share_path() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        declare(&home, &["config/app.env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, Some("feat"), "config/app.env").status,
            ShareStatus::Linked
        );
        assert!(wt(&home, "main").join("config/app.env").is_file());
        assert_eq!(
            std::fs::read_link(wt(&home, "feat").join("config/app.env")).unwrap(),
            Path::new("../../main/config/app.env")
        );
    }

    #[test]
    fn materialize_refuses_a_destination_under_a_symlinked_parent() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        // A symlinked parent in the worktree that would redirect the write outside.
        std::os::unix::fs::symlink(&outside, wt(&home, "feat").join("sub")).unwrap();
        declare(&home, &["sub/x"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, Some("feat"), "sub/x").status,
            ShareStatus::Error,
            "ELOOP on the symlinked parent, never linked"
        );
        assert!(
            !outside.join("x").exists(),
            "nothing written outside the worktree"
        );
    }

    #[test]
    fn ensure_source_refuses_a_symlink_redirect_in_the_trunk() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let outside = tmp.path().join("evil");
        std::fs::write(&outside, "DO NOT TOUCH").unwrap();
        // A symlinked parent inside the trunk pointing at an outside file.
        std::os::unix::fs::symlink(&outside, wt(&home, "main").join("config")).unwrap();
        declare(&home, &["config/x"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(find(&out, None, "config/x").status, ShareStatus::Error);
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "DO NOT TOUCH",
            "outside file intact"
        );
    }

    /// A slug, root or worktree the walk cannot open is one error row saying why, and no
    /// link is laid: a bad slug never builds a path, a trunk that is missing or no
    /// directory has nothing to link through, nor has a code dir grove may not read,
    /// an unreadable manifest declares nothing, and a worktree grove may not open is
    /// skipped.
    #[test]
    fn what_the_walk_cannot_open_is_an_error_row() {
        use std::os::unix::fs::PermissionsExt;
        fn mode(path: &Path, m: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)).unwrap();
        }
        type Case = (&'static str, Option<&'static str>, &'static str, fn(&Path));
        let cases: [Case; 6] = [
            ("../x", None, "invalid slug", |_| {}),
            ("o/r", None, "open root", |home| {
                mode(&code_dir(home, "o/r"), 0o100);
            }),
            ("o/r", None, "trunk missing", |home| {
                std::fs::remove_dir_all(wt(home, "main")).unwrap();
            }),
            ("o/r", None, "open main", |home| {
                std::fs::remove_dir_all(wt(home, "main")).unwrap();
                std::fs::write(wt(home, "main"), "").unwrap();
            }),
            ("o/r", None, "resolve trunk", |home| {
                std::fs::write(manifest_path(home), "not = = toml").unwrap();
            }),
            ("o/r", Some("feat"), "open worktree", |home| {
                mode(&wt(home, "feat"), 0);
            }),
        ];
        for (slug, worktree, why, break_it) in cases {
            let tmp = TempDir::new().unwrap();
            let home = home_with_root(&tmp);
            declare(&home, &[".env"]);
            break_it(&home);

            let out = materialize(&home, Some(slug), Fix::Safe).unwrap();
            mode(&code_dir(&home, "o/r"), 0o755);
            mode(&wt(&home, "feat"), 0o755);
            let row = out.iter().find(|o| o.status.is_error()).expect(why);
            assert_eq!(row.worktree.as_deref(), worktree, "{why}");
            assert!(row.reason.as_deref().unwrap().contains(why), "{row:?}");
            assert!(!wt(&home, "feat").join(".env").exists(), "{why}");
        }
    }

    /// The GC's listing is by path, after the open: a worktree that turns unreadable in
    /// between is the same error row, never a pass that silently collects nothing.
    #[test]
    fn a_worktree_the_gc_cannot_list_is_an_error_row() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let feat = wt(&home_with_root(&tmp), "feat");
        let fd = open_dir(&feat).unwrap();
        std::fs::set_permissions(&feat, std::fs::Permissions::from_mode(0o300)).unwrap();
        let pass = Pass {
            slug: "o/r",
            trunk_name: "main",
            prev_trunk: "main",
            pending: false,
        };
        let mut out = Vec::new();
        gc_worktree(pass, "feat", &feat, &fd, &[], Action::Diagnose, &mut out);
        std::fs::set_permissions(&feat, std::fs::Permissions::from_mode(0o755)).unwrap();
        let [o] = &out[..] else { panic!("{out:?}") };
        assert!(
            o.worktree.as_deref() == Some("feat")
                && o.reason.as_deref().unwrap().starts_with("read worktree"),
            "{o:?}"
        );
    }

    #[test]
    fn gc_on_undeclare_removes_only_grove_links() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        declare(&home, &[".env", ".secret"]);
        materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert!(wt(&home, "feat").join(".secret").is_symlink());

        manifest::remove_share(&manifest_path(&home), "o/r", "symlink", ".secret").unwrap();
        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(find(&out, Some("feat"), ".secret").status, ShareStatus::Gc);

        assert!(
            !wt(&home, "feat").join(".secret").exists(),
            "orphan link removed"
        );
        assert!(
            wt(&home, "main").join(".secret").is_file(),
            "source data preserved"
        );
        assert!(
            wt(&home, "feat").join(".env").is_symlink(),
            "still-declared link untouched"
        );
    }

    #[test]
    fn gc_spares_user_symlinks_and_real_files() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::os::unix::fs::symlink("/tmp/elsewhere", wt(&home, "feat").join("mylink")).unwrap();
        std::fs::write(wt(&home, "feat").join("notes.md"), "keep me").unwrap();

        materialize(&home, Some("o/r"), Fix::Safe).unwrap(); // nothing declared
        assert!(
            wt(&home, "feat").join("mylink").is_symlink(),
            "foreign symlink spared"
        );
        assert_eq!(
            std::fs::read_to_string(wt(&home, "feat").join("notes.md")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn new_worktree_picks_up_an_already_declared_share() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        declare(&home, &[".env"]);
        materialize(&home, Some("o/r"), Fix::Safe).unwrap();

        crate::worktrees::create(&home, "o/r", "feat2", "feature/y", Some("main")).unwrap();
        materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert!(
            wt(&home, "feat2").join(".env").is_symlink(),
            "new worktree linked, no manifest re-edit"
        );
    }

    #[test]
    fn diagnose_mutates_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "feat").join(".env"), "conflict").unwrap(); // a conflict
        std::os::unix::fs::symlink("../main/old", wt(&home, "feat").join(".stale")).unwrap();
        // .other would link; cfg/x would create its source and link under parents that
        // are absent on both sides
        declare(&home, &[".env", ".other", ".stale", "cfg/x"]);

        let root = home.join("code/o/r");
        let before = snapshot(&root);
        let out = diagnose(&home, Some("o/r")).unwrap();
        let after = snapshot(&root);

        assert_eq!(before, after, "diagnose must not mutate the filesystem");
        for (wt, path, status) in [
            (Some("feat"), ".env", ShareStatus::Conflict),
            (Some("feat"), ".other", ShareStatus::Linked),
            (Some("feat"), ".stale", ShareStatus::Repointed),
            (None, "cfg/x", ShareStatus::Created),
            (Some("feat"), "cfg/x", ShareStatus::Linked),
        ] {
            assert_eq!(find(&out, wt, path).status, status, "{path}");
        }
    }

    #[test]
    fn materialize_wires_into_create() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        declare(&home, &[".env"]);
        materialize(&home, Some("o/r"), Fix::Safe).unwrap();

        // A worktree created after the share is declared is pre-linked by `create`
        // itself — no explicit materialize.
        crate::worktrees::create(&home, "o/r", "feat3", "feature/z", Some("main")).unwrap();
        assert!(wt(&home, "feat3").join(".env").is_symlink());
    }

    #[test]
    fn materialize_wires_into_reconcile_one() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        declare(&home, &[".env"]);

        crate::roots::reconcile_one(&home, "o/r").unwrap();
        assert!(
            wt(&home, "feat").join(".env").is_symlink(),
            "reconcile materialized the share"
        );
    }

    // --- copy shares (`_.copy`) ---------------------------------------------

    #[test]
    fn copy_seeds_an_independent_real_file_from_the_source() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "main").join("seed.env"), "SEED=1").unwrap();
        declare_copy(&home, &["seed.env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, Some("feat"), "seed.env").status,
            ShareStatus::Copied
        );

        let dst = wt(&home, "feat").join("seed.env");
        assert!(
            dst.is_file() && !dst.is_symlink(),
            "a real file, not a link"
        );
        assert_eq!(
            std::fs::read_to_string(&dst).unwrap(),
            "SEED=1",
            "seeded from source"
        );
    }

    #[test]
    fn copy_is_independent_of_the_source_after_seeding() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "main").join("seed.env"), "ORIGINAL").unwrap();
        declare_copy(&home, &["seed.env"]);
        materialize(&home, Some("o/r"), Fix::Safe).unwrap();

        // Mutating the source does NOT flow into the copy (the symlink contrast).
        std::fs::write(wt(&home, "main").join("seed.env"), "CHANGED").unwrap();
        assert_eq!(
            std::fs::read_to_string(wt(&home, "feat").join("seed.env")).unwrap(),
            "ORIGINAL",
            "the copy is a snapshot, not a live link"
        );
    }

    #[test]
    fn copy_is_seed_once_and_never_overwrites_a_worktree_edit() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "main").join("seed.env"), "SEED").unwrap();
        declare_copy(&home, &["seed.env"]);
        materialize(&home, Some("o/r"), Fix::Safe).unwrap();

        // The worktree edits its own copy; a re-run leaves it alone (status Ok).
        std::fs::write(wt(&home, "feat").join("seed.env"), "MY LOCAL EDIT").unwrap();
        let again = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&again, Some("feat"), "seed.env").status,
            ShareStatus::Ok
        );
        assert_eq!(
            std::fs::read_to_string(wt(&home, "feat").join("seed.env")).unwrap(),
            "MY LOCAL EDIT"
        );
    }

    #[test]
    fn copy_persists_on_undeclare_not_gc() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "main").join("seed.env"), "SEED").unwrap();
        declare_copy(&home, &["seed.env"]);
        materialize(&home, Some("o/r"), Fix::Safe).unwrap();

        // Undeclare — a copy is the worktree's own (indistinguishable from a user
        // file), so GC leaves it in place (unlike a grove symlink).
        manifest::remove_share(&manifest_path(&home), "o/r", "copy", "seed.env").unwrap();
        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert!(
            !out.iter()
                .any(|o| o.path == "seed.env" && o.status == ShareStatus::Gc),
            "a copy is never GC'd"
        );
        assert!(
            wt(&home, "feat").join("seed.env").is_file(),
            "copy survives undeclare"
        );
    }

    #[test]
    fn copy_migrates_a_prior_grove_symlink() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "main").join("seed.env"), "SEED").unwrap();
        // A grove-owned symlink from a prior `_.symlink` declaration of this path.
        std::os::unix::fs::symlink("../main/seed.env", wt(&home, "feat").join("seed.env")).unwrap();
        declare_copy(&home, &["seed.env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, Some("feat"), "seed.env").status,
            ShareStatus::Copied
        );
        let dst = wt(&home, "feat").join("seed.env");
        assert!(
            !dst.is_symlink() && dst.is_file(),
            "the grove link became a real copy"
        );
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "SEED");
    }

    #[test]
    fn copy_never_clobbers_a_user_file_or_foreign_symlink() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "main").join("seed.env"), "SEED").unwrap();
        std::fs::write(wt(&home, "feat").join("seed.env"), "user owned").unwrap();
        declare_copy(&home, &["seed.env"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(
            find(&out, Some("feat"), "seed.env").status,
            ShareStatus::Ok,
            "an existing file is the worktree's own — left untouched"
        );
        assert_eq!(
            std::fs::read_to_string(wt(&home, "feat").join("seed.env")).unwrap(),
            "user owned"
        );
    }

    #[test]
    fn copy_refuses_a_destination_under_a_symlinked_parent() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(wt(&home, "main").join("sub")).unwrap();
        std::fs::write(wt(&home, "main").join("sub/x"), "SEED").unwrap();
        // A symlinked parent in the worktree that would redirect the write outside.
        std::os::unix::fs::symlink(&outside, wt(&home, "feat").join("sub")).unwrap();
        declare_copy(&home, &["sub/x"]);

        let out = materialize(&home, Some("o/r"), Fix::Safe).unwrap();
        assert_eq!(find(&out, Some("feat"), "sub/x").status, ShareStatus::Error);
        assert!(
            !outside.join("x").exists(),
            "nothing written outside the worktree"
        );
    }

    #[test]
    fn copy_diagnose_mutates_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "main").join("seed.env"), "SEED").unwrap();
        declare_copy(&home, &["seed.env", "cfg/seed.env"]);

        let root = home.join("code/o/r");
        let before = snapshot(&root);
        let out = diagnose(&home, Some("o/r")).unwrap();
        let after = snapshot(&root);
        assert_eq!(before, after, "diagnose must not seed a copy");
        for path in ["seed.env", "cfg/seed.env"] {
            assert_eq!(find(&out, Some("feat"), path).status, ShareStatus::Copied);
        }
    }

    #[test]
    fn copy_wires_into_create() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::write(wt(&home, "main").join("seed.env"), "SEED").unwrap();
        declare_copy(&home, &["seed.env"]);
        materialize(&home, Some("o/r"), Fix::Safe).unwrap();

        // A worktree created after the copy share is declared is pre-seeded by `create`.
        crate::worktrees::create(&home, "o/r", "feat4", "feature/w", Some("main")).unwrap();
        let dst = wt(&home, "feat4").join("seed.env");
        assert!(
            dst.is_file() && !dst.is_symlink(),
            "new worktree pre-seeded with a copy"
        );
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "SEED");
    }

    /// `unlay` takes back only a file it can match to its trunk source: one whose source
    /// is absent, or is no regular file, is the user's and stays. A share whose parent
    /// the worktree lacks was never laid, and the shares after it are still taken back.
    #[test]
    fn unlay_keeps_a_file_it_cannot_match_to_a_trunk_source() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::fs::create_dir(wt(&home, "feat").join("cfg")).unwrap();
        mkfifo(&wt(&home, "main").join("odd"));
        std::os::unix::fs::symlink("../main/.env", wt(&home, "feat").join(".env")).unwrap();
        declare(&home, &["gone/x", ".env"]);
        declare_copy(&home, &["cfg/x", "odd"]);
        for p in ["cfg/x", "odd"] {
            std::fs::write(wt(&home, "feat").join(p), "mine").unwrap();
        }

        unlay(&home, "o/r", "feat").unwrap();
        assert!(std::fs::symlink_metadata(wt(&home, "feat").join(".env")).is_err());
        for p in ["cfg/x", "odd"] {
            assert_eq!(
                std::fs::read_to_string(wt(&home, "feat").join(p)).unwrap(),
                "mine"
            );
        }
    }

    /// A carried symlink source spelled `./<p>` names the old trunk's own file as surely
    /// as `<p>`: carried verbatim it would dangle, so it holds the carry.
    #[test]
    fn a_dot_relative_link_source_points_inside_the_old_trunk() {
        let tmp = TempDir::new().unwrap();
        let home = home_with_root(&tmp);
        std::os::unix::fs::symlink("./.real", wt(&home, "feat").join(".k")).unwrap();
        declare(&home, &[".k"]);

        let out = carry(&home, "o/r", "feat");
        let why = find(&out, None, ".k").reason.as_deref().unwrap();
        assert!(why.contains("inside the old trunk"), "{why}");
        assert!(std::fs::symlink_metadata(wt(&home, "main").join(".k")).is_err());
    }
}
