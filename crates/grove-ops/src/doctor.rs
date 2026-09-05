//! Doctor: the operator's window on a home, and the only command whose job is to
//! *look* rather than converge.
//!
//! Two halves, deliberately separate:
//!
//! - [`run`] — the share/pool half, carried from v1: converge (or merely diagnose)
//!   the declared `[env]._` shares and read pool levels beside them. It **writes**
//!   unless `dry_run`, and `--fix` is what turns a would-clobber into a backup.
//! - [`checks`] — the git-plumbing half, new in v2. Read-only, always: does the
//!   manifest parse and validate, is the install tree out from under the workspace,
//!   is each root's bare and trunk there, is each declared worktree present and on
//!   the branch it declares, and what is on disk that nothing declares.
//!
//! The second half answers what no other read can: a root whose bare survived but
//! whose trunk was deleted, or a worktree quietly sitting on the wrong branch, is
//! otherwise invisible to the one command whose whole purpose is to find it. The
//! vocabulary is contract — see `docs/api.md` § Doctor. Findings are
//! **report-only** — `--fix` is scoped to shares plus the one legacy-layout
//! migration, because every other plumbing finding here is either a human's edit to
//! reconcile with (drift, a bad declaration) or a job the reconciler already owns (a
//! missing clone), and doctor silently re-cloning under an operator asking "what is
//! wrong?" is the opposite of a diagnosis.
//!
//! The migration is the exception because nothing else can make it: reconcile is
//! additive and would clone a second root beside the legacy one rather than rename
//! what is there, so a root laid down under the old layout has no other path forward.
//! It runs in [`run`] — the writing half — and the [`CheckKind::LegacyLayout`] row
//! that [`root_checks`] emits afterwards is what reports the outcome: `ok` once the
//! root is on the current layout, and the finding again, with the reason, when the
//! migration refused or failed.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::layout::{LEGACY_BARE, LEGACY_TRUNK, is_legacy_bare};
use crate::{Result, env, git, manifest, pool, roots, worktrees};

/// Doctor's share/pool half, shared verbatim by the daemon's `POST /api/doctor` route
/// and the offline CLI (`grove doctor`): diagnose only (`dry_run`) or materialize the
/// worktree shares (Safe, or Force under `fix`), then read pool levels alongside —
/// doctor is the operator's window on an under-filled pool. Returns the
/// `(report, pools)` pair; per-root engine `statuses` are the caller's to add (the
/// daemon adds its engines'; the offline CLI derives its own from disk).
pub fn run(
    home: &Path,
    slug: Option<&str>,
    dry_run: bool,
    fix: bool,
) -> Result<(Vec<env::ShareOutcome>, Vec<pool::PoolStatus>)> {
    // Before the share pass, and only under `--fix`: a legacy root's trunk moves out
    // from under the links, so the pass that writes them has to run after the move.
    // `dry_run` wins, as everywhere else here — a diagnosis mutates nothing.
    let mut report = if fix && !dry_run {
        migrate_legacy(home, slug)
    } else {
        Vec::new()
    };
    report.extend(if dry_run {
        env::diagnose(home, slug)?
    } else {
        let fix = if fix { env::Fix::Force } else { env::Fix::Safe };
        env::materialize(home, slug, fix)?
    });
    let pools = pool::status(home, slug)?;
    Ok((report, pools))
}

/// Retire the legacy layout on every root in scope, reporting one error row per root
/// that could not be migrated.
///
/// A row rather than an `Err`: one root grove will not touch must not blank the
/// report for every other, and the share report is where a doctor row that carries a
/// failing verdict already lives — an error there is what makes `grove doctor --fix`
/// exit non-zero. A root that was never legacy, or that migrated cleanly, contributes
/// nothing; the [`CheckKind::LegacyLayout`] row is where the operator reads that.
fn migrate_legacy(home: &Path, slug: Option<&str>) -> Vec<env::ShareOutcome> {
    let slugs = match slug {
        Some(slug) => vec![slug.to_owned()],
        None => roots::list(home)
            .map(|roots| roots.into_iter().map(|root| root.slug).collect())
            .unwrap_or_default(),
    };
    slugs
        .into_iter()
        .filter_map(|slug| {
            migrate_root(home, &slug).err().map(|e| env::ShareOutcome {
                slug,
                worktree: None,
                path: String::new(),
                status: env::ShareStatus::Error,
                reason: Some(format!("legacy layout: {e:#}")),
            })
        })
        .collect()
}

/// Migrate one root off the legacy layout, in place.
///
/// Every step is idempotent and ordered so that a crash between any two of them
/// resumes on the next `--fix` rather than leaving a shape neither half recognizes:
///
/// 1. the bare moves to `.bare`, and git re-derives the gitlink each checkout holds
///    (the checkouts have not moved, so only the bare's own path changed);
/// 2. the trunk moves to the directory its branch names, and git re-derives the admin
///    pointer back at it — resolved only now, because the resolution reads the bare
///    that step 1 just moved;
/// 3. the bare's `HEAD` is set to the trunk branch, which is where every later reader
///    learns what this root integrates on;
/// 4. the shares materialize, repointing each `../.trunk/<p>` link onto the trunk's
///    new name — `Safe`, because a migration is the wrong moment to start backing up
///    a real file the operator put there; `run`'s own pass, which `--fix` authorized,
///    is where that is decided;
/// 5. the trunk is read as a git checkout. Nothing above proves the result works, and
///    a migration that reported success over a tree git can no longer open would be
///    worse than one that never ran.
fn migrate_root(home: &Path, slug: &str) -> anyhow::Result<()> {
    use anyhow::{Context as _, bail};

    let root = roots::root_dir(home, slug);
    let legacy_bare = root.join(LEGACY_BARE);
    let legacy_trunk = root.join(LEGACY_TRUNK);
    if !is_legacy_bare(&legacy_bare) && !legacy_trunk.is_dir() {
        return Ok(());
    }
    let bare = roots::bare_dir(home, slug);

    if is_legacy_bare(&legacy_bare) {
        if bare.exists() {
            bail!(
                "both {} and {} are present — a half-finished migration. Grove will not \
                 guess which one is the real bare; move or delete the wrong one by hand",
                legacy_bare.display(),
                bare.display()
            );
        }
        std::fs::rename(&legacy_bare, &bare)
            .with_context(|| format!("rename {} -> {}", legacy_bare.display(), bare.display()))?;
        git::worktree_repair(&bare, &[])?;
    }
    if !bare.is_dir() {
        bail!(
            "{} has no bare at {}, so there is nothing to migrate its trunk onto",
            slug,
            bare.display()
        );
    }

    let want = roots::trunk(home, slug).with_context(|| format!("resolve the trunk for {slug}"))?;
    if legacy_trunk.is_dir() {
        if want.dir.exists() {
            bail!(
                "{} is already there, so {} has nowhere to move; move it aside by hand",
                want.dir.display(),
                legacy_trunk.display()
            );
        }
        std::fs::rename(&legacy_trunk, &want.dir).with_context(|| {
            format!(
                "rename {} -> {}",
                legacy_trunk.display(),
                want.dir.display()
            )
        })?;
        git::worktree_repair(&bare, &[want.dir.as_path()])?;
    }

    git::set_head(&bare, &want.branch)?;
    env::materialize(home, Some(slug), env::Fix::Safe)
        .with_context(|| format!("materialize the shares of {slug}"))?;
    git::status(&want.dir).with_context(|| {
        format!(
            "{} does not read as a git checkout after the migration",
            want.dir.display()
        )
    })?;
    Ok(())
}

/// One plumbing finding — or one passing check, which is a finding too: an operator
/// reading a report needs to know a check *ran*.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Check {
    pub check: CheckKind,
    pub status: CheckStatus,
    /// The root the check is about; absent only for the whole-file manifest check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// The worktree name, share path, or other item inside the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Prose for a human: the validator's message, the branch actually checked out,
    /// the path that is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Check {
    /// Whether this row is something an operator has to act on. The CLI's *renderer*
    /// reads it — a passing kind collapses to one summary line, a finding prints in
    /// full — and `checks` itself never branches on it.
    ///
    /// Deliberately **not** wired to the exit code: the checks are report-only (see
    /// the module doc), and most findings here are ordinary transient drift the
    /// reconciler already owns, so exiting non-zero on them would make `grove doctor`
    /// useless as a gate. The verdict stays on the share report, exactly as v1's.
    #[must_use]
    pub const fn is_finding(&self) -> bool {
        !matches!(self.status, CheckStatus::Ok)
    }

    fn new(check: CheckKind, status: CheckStatus) -> Self {
        Self {
            check,
            status,
            slug: None,
            name: None,
            detail: None,
        }
    }

    fn of(check: CheckKind, status: CheckStatus, slug: &str) -> Self {
        Self {
            slug: Some(slug.to_owned()),
            ..Self::new(check, status)
        }
    }

    #[must_use]
    fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[must_use]
    fn detailed(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// What was inspected. The wire spellings are contract (`check_kind` in
/// `contracts/wire-vocab.json`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// `manifest.toml` parses, and every declaration in it passes the validators.
    Manifest,
    /// The install tree — `versions/`, `current` and their siblings — is not sitting
    /// under the workspace root, where it used to live before the two were split.
    InstallUnderHome,
    /// The pre-branch-named layout: a bare at `.git`, or a trunk checkout at
    /// `.trunk`. Report-only like every kind here, but the one whose finding names a
    /// remedy grove itself performs — `grove doctor --fix` migrates the root in
    /// place, and this row reads `ok` once it has.
    LegacyLayout,
    /// The root's pass as a whole. Only ever a finding: a root whose lane would not
    /// answer inside doctor's per-root budget contributes one of these instead of the
    /// rows below, so a wedged root is named rather than silently absent from a
    /// whole-home report. A root that answers never emits it.
    Root,
    /// `<root>/.bare` — the bare clone.
    Bare,
    /// The trunk checkout — the branch this root integrates on, and the source every
    /// share links to.
    Trunk,
    /// A declared worktree: realized in git, and on the branch it declares.
    Worktree,
    /// Declared ⋈ actual: a worktree git knows about that the manifest does not.
    Drift,
}

impl CheckKind {
    /// Every kind, in declaration order — what `contracts/wire-vocab.json` pins.
    pub const ALL: [Self; 8] = [
        Self::Manifest,
        Self::InstallUnderHome,
        Self::LegacyLayout,
        Self::Root,
        Self::Bare,
        Self::Trunk,
        Self::Worktree,
        Self::Drift,
    ];
}

/// How a check came out.
///
/// Coarser than a message and finer than a boolean: a UI groups on this, and the
/// distinction that matters to an operator is *why* something is not `ok` — a
/// declaration that can never realize (`invalid`) is a different job from a checkout
/// that drifted onto another branch (`mismatch`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The check ran and passed.
    Ok,
    /// Declared, and not on disk / not in git.
    Missing,
    /// Present, but not what was declared — a worktree on another branch, or
    /// detached.
    Mismatch,
    /// The declaration itself is rejected by the validators, so it can never realize.
    Invalid,
    /// On disk and undeclared. Additive reconcile adopts these; until it does, they
    /// are drift worth seeing.
    Undeclared,
    /// The check could not run — git would not answer, or the root exceeded its
    /// budget.
    Unavailable,
}

impl CheckStatus {
    /// Every status, in declaration order — what `contracts/wire-vocab.json` pins.
    pub const ALL: [Self; 6] = [
        Self::Ok,
        Self::Missing,
        Self::Mismatch,
        Self::Invalid,
        Self::Undeclared,
        Self::Unavailable,
    ];
}

/// The whole-home plumbing pass: the manifest and layout checks plus every declared
/// root's.
///
/// The daemon does **not** call this — it fans the per-root half out on each root's
/// own lane, under a budget (carried law 6: every per-root git reader is serialized
/// against that root's writers, doctor included). This is the offline composition,
/// for a CLI with no daemon to ask.
#[must_use]
pub fn checks(home: &Path, slug: Option<&str>) -> Vec<Check> {
    let mut out = manifest_checks(home);
    // Scoping doctor to one slug narrows the *roots* it walks; the layout is one fact
    // about the home, so it answers on every invocation or an operator could scope
    // their way past the one finding that explains a lost checkout.
    out.extend(install_checks(home));
    let slugs = match slug {
        Some(slug) => vec![slug.to_owned()],
        None => roots::list(home)
            .map(|roots| roots.into_iter().map(|root| root.slug).collect())
            .unwrap_or_default(),
    };
    for slug in slugs {
        out.extend(root_checks(home, &slug));
    }
    out
}

/// The stage-5 migration from `docs/plans/install-home.md`, verbatim.
///
/// Doctor is report-only, so this string *is* the fix: an operator reads it and runs
/// it. `grove doctor` closes that block and is deliberately absent here — it is the
/// verification, and it is what printed this line.
const INSTALL_MIGRATION: &str = "mkdir -p ~/.local/share/grove; \
     mv ~/.grove/{versions,current,previous,channel,update.lock} ~/.local/share/grove/; \
     ln -sf ~/.local/share/grove/current/bin/grove ~/.local/bin/grove; \
     grove off && grove on";

/// Is the install still living under the workspace root?
///
/// `versions/` and `current` belong to `$GROVE_INSTALL`; a home that still carries
/// them is the pre-split layout, and the cost is concrete — `uninstall.sh` removes the
/// install root, and under the old layout that took every checkout with the binary.
/// One row either way, like the manifest check: an operator needs to know the layout
/// was *looked at*.
///
/// `symlink_metadata` rather than `exists`, because a `current` left dangling by a
/// half-finished move is precisely the state worth naming — `exists` follows the link
/// and reports the home clean.
///
/// `mismatch` and not `undeclared`: the home is present and is not the shape the
/// layout declares. `undeclared` promises the reader that "additive reconcile adopts
/// these", which reconcile will never do for an install tree.
#[must_use]
pub fn install_checks(home: &Path) -> Vec<Check> {
    let stale: Vec<&str> = ["current", "versions"]
        .into_iter()
        .filter(|entry| home.join(entry).symlink_metadata().is_ok())
        .collect();
    if stale.is_empty() {
        return vec![Check::new(CheckKind::InstallUnderHome, CheckStatus::Ok)];
    }
    vec![
        Check::new(CheckKind::InstallUnderHome, CheckStatus::Mismatch)
            .named(stale.join(", "))
            .detailed(INSTALL_MIGRATION),
    ]
}

/// Does `manifest.toml` parse, and does every declaration in it validate?
///
/// One `ok` row when it does; one `invalid` row per rejected declaration when it does
/// not, and a single `invalid` row naming the parse error when the file itself will
/// not read. Never per-root — the manifest is one file, and a caller scoping doctor to
/// one slug still wants to know the file it read is sound.
#[must_use]
pub fn manifest_checks(home: &Path) -> Vec<Check> {
    let path = roots::manifest_path(home);
    match manifest::audit(&path) {
        Err(e) => vec![
            Check::new(CheckKind::Manifest, CheckStatus::Invalid)
                .detailed(format!("{} does not parse: {e:#}", path.display())),
        ],
        Ok(invalid) if invalid.is_empty() => vec![Check::new(CheckKind::Manifest, CheckStatus::Ok)],
        Ok(invalid) => invalid
            .into_iter()
            .map(|bad| {
                let check = Check::of(CheckKind::Manifest, CheckStatus::Invalid, &bad.slug)
                    .detailed(bad.reason);
                match bad.item {
                    Some(item) => check.named(item),
                    None => check,
                }
            })
            .collect(),
    }
}

/// One root's git plumbing: the layout it is on, bare, trunk, every declared
/// worktree, and the undeclared ones git knows about.
///
/// **Runs git**, so the daemon calls it on the root's lane. Infallible by
/// construction — a read that fails is itself a finding (`unavailable`), never an
/// error that blanks the rest of the report.
#[must_use]
pub fn root_checks(home: &Path, slug: &str) -> Vec<Check> {
    // First, because it explains the two rows below it: a root still on the legacy
    // layout has no `.bare` and no branch-named trunk, so both would otherwise read
    // as `missing` with nothing saying why.
    let mut out = vec![legacy_check(home, slug)];

    let bare = roots::bare_dir(home, slug);
    let has_bare = bare.is_dir();
    out.push(presence(CheckKind::Bare, slug, has_bare, &bare));
    let trunk = roots::trunk_dir(home, slug);
    out.push(presence(CheckKind::Trunk, slug, trunk.is_dir(), &trunk));

    // With no bare there is no git to ask, and a row per declared worktree saying
    // "could not read" would bury the one finding that explains them all.
    if !has_bare {
        return out;
    }

    match worktrees::list(home, slug) {
        Err(e) => out.push(
            Check::of(CheckKind::Worktree, CheckStatus::Unavailable, slug)
                .detailed(format!("could not list worktrees: {e}")),
        ),
        Ok(trees) => out.extend(trees.into_iter().map(|wt| worktree_check(slug, &wt))),
    }
    out
}

/// Is this root still on the layout that predates naming the trunk by its branch?
///
/// One row per root, `ok` when it is not: `--fix` rewrites directory names under an
/// operator, and a report that said nothing about a root about to be rewritten would
/// be withholding the one thing they most need before running it.
///
/// Report-only, like every check here. The migration itself runs in [`run`], so the
/// row an operator reads after a `--fix` is this check re-run against the result:
/// `ok` when the root came out on the current layout, and the finding again — with
/// the reason, in the share report's error row — when it did not.
fn legacy_check(home: &Path, slug: &str) -> Check {
    let root = roots::root_dir(home, slug);
    let legacy_bare = root.join(LEGACY_BARE);
    let legacy_trunk = root.join(LEGACY_TRUNK);
    let bare = roots::bare_dir(home, slug);
    let finding = |detail: String| {
        Check::of(CheckKind::LegacyLayout, CheckStatus::Mismatch, slug).detailed(detail)
    };

    if is_legacy_bare(&legacy_bare) && bare.exists() {
        return finding(format!(
            "both {} and {} are present — a half-finished migration. `grove doctor --fix` \
             will not guess which one is the real bare; move or delete the wrong one by hand",
            legacy_bare.display(),
            bare.display()
        ));
    }
    let found: Vec<String> = [
        is_legacy_bare(&legacy_bare).then(|| format!("{} is a bare repo", legacy_bare.display())),
        legacy_trunk
            .is_dir()
            .then(|| format!("{} is the trunk checkout", legacy_trunk.display())),
    ]
    .into_iter()
    .flatten()
    .collect();
    if found.is_empty() {
        return Check::of(CheckKind::LegacyLayout, CheckStatus::Ok, slug);
    }
    finding(format!(
        "{} — the layout that predates naming the trunk by its branch; \
         `grove doctor --fix` migrates this root in place",
        found.join(", and ")
    ))
}

/// The bare/trunk presence rows, which differ only in which path they name.
fn presence(kind: CheckKind, slug: &str, present: bool, path: &Path) -> Check {
    if present {
        Check::of(kind, CheckStatus::Ok, slug)
    } else {
        Check::of(kind, CheckStatus::Missing, slug)
            .detailed(format!("{} is absent", path.display()))
    }
}

/// One worktree row: declared-and-realized-on-its-branch, or the way it differs.
fn worktree_check(slug: &str, wt: &worktrees::WorktreeStatus) -> Check {
    let check = |status| Check::of(CheckKind::Worktree, status, slug).named(&wt.name);
    if !wt.declared {
        return Check::of(CheckKind::Drift, CheckStatus::Undeclared, slug)
            .named(&wt.name)
            .detailed(format!(
                "on disk at `{}` and undeclared; the next reconcile adopts it",
                wt.name
            ));
    }
    if !wt.present {
        return check(CheckStatus::Missing).detailed(format!(
            "declared on `{}` and not realized in git",
            wt.branch
        ));
    }
    // `status` is `None` only when the `git status` read itself failed — the list
    // swallows that to keep one wedged checkout from blanking the whole dashboard,
    // and here it is the finding.
    let Some(status) = wt.status.as_ref() else {
        return check(CheckStatus::Unavailable).detailed("git would not report its status");
    };
    match status.branch.as_deref() {
        Some(branch) if branch == wt.branch => check(CheckStatus::Ok),
        Some(branch) => check(CheckStatus::Mismatch).detailed(format!(
            "checked out on `{branch}`, declared `{}`",
            wt.branch
        )),
        None => check(CheckStatus::Mismatch)
            .detailed(format!("detached HEAD, declared `{}`", wt.branch)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CheckKind, CheckStatus, LEGACY_BARE, LEGACY_TRUNK, checks, install_checks, manifest_checks,
        root_checks, run,
    };
    use crate::{git, manifest, roots, testfix};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// Every kind and status has one wire spelling, and it is the `snake_case` one the
    /// fixture pins.
    #[test]
    fn the_check_vocabularies_serialize_as_declared() {
        let s = |v: &dyn Fn() -> serde_json::Value| v();
        assert_eq!(
            s(&|| serde_json::to_value(CheckKind::Manifest).unwrap()),
            "manifest"
        );
        assert_eq!(
            s(&|| serde_json::to_value(CheckStatus::Undeclared).unwrap()),
            "undeclared"
        );
        assert_eq!(CheckKind::ALL.len(), 8);
        assert_eq!(CheckStatus::ALL.len(), 6);
    }

    /// A home with nothing in it is not a home with something wrong in it: an absent
    /// manifest parses as an empty document, so the check passes.
    #[test]
    fn an_empty_home_passes_the_manifest_check() {
        let tmp = TempDir::new().unwrap();
        let out = manifest_checks(tmp.path());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].status, CheckStatus::Ok);
        assert!(!out[0].is_finding());
    }

    /// The whole point of the audit: the lenient read side skips these silently, so
    /// doctor is the only place they are ever named.
    #[test]
    fn an_unrealizable_declaration_is_reported_rather_than_skipped() {
        let tmp = TempDir::new().unwrap();
        let manifest = roots::manifest_path(tmp.path());
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            &manifest,
            r#"
[roots."../escape"]
url = "git@github.com:o/r.git"

[roots."o/r"]

[roots."o/r".worktrees.".pool"]
branch = "main"

[roots."o/r".worktrees."ok"]
branch = "-flag"

[roots."o/r".env._]
symlink = ["../../etc/passwd"]
"#,
        )
        .unwrap();

        let out = manifest_checks(tmp.path());
        let findings: Vec<(&str, Option<&str>)> = out
            .iter()
            .map(|c| (c.slug.as_deref().unwrap_or(""), c.name.as_deref()))
            .collect();

        assert!(out.iter().all(|c| c.status == CheckStatus::Invalid));
        assert!(
            findings.contains(&("../escape", None)),
            "the traversal key is named as written, never as a path: {findings:?}"
        );
        assert!(
            findings.contains(&("o/r", None)),
            "a root with no url can never realize: {findings:?}"
        );
        assert!(findings.contains(&("o/r", Some(".pool"))), "{findings:?}");
        assert!(findings.contains(&("o/r", Some("ok"))), "{findings:?}");
        assert!(
            findings.contains(&("o/r", Some("../../etc/passwd"))),
            "{findings:?}"
        );
    }

    /// A file that will not parse is one finding, not an error the caller has to
    /// handle: doctor's job is to say what is wrong with a home.
    #[test]
    fn an_unparseable_manifest_is_a_finding() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(roots::manifest_path(tmp.path()), "this is not = = toml").unwrap();
        let out = manifest_checks(tmp.path());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].check, CheckKind::Manifest);
        assert_eq!(out[0].status, CheckStatus::Invalid);
        assert!(out[0].detail.as_ref().unwrap().contains("does not parse"));
    }

    /// A half-finished move leaves `current` pointing at nothing, and that is the
    /// worst moment to be told the home is fine: `exists` follows the link and reports
    /// clean, so the check reads the link itself.
    #[test]
    fn a_dangling_current_symlink_is_still_an_install_under_the_home() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);
        std::os::unix::fs::symlink(home.join("versions/0.0.0"), home.join("current")).unwrap();
        assert!(!home.join("current").exists(), "the link dangles");

        let out = checks(&home, None);
        let found: Vec<&super::Check> = out
            .iter()
            .filter(|c| c.check == CheckKind::InstallUnderHome)
            .collect();
        assert_eq!(found.len(), 1, "one row about the home, not one per root");
        assert_eq!(found[0].status, CheckStatus::Mismatch);
        assert_eq!(found[0].name.as_deref(), Some("current"));
        let detail = found[0].detail.as_ref().unwrap();
        for line in [
            "mkdir -p ~/.local/share/grove",
            "mv ~/.grove/{versions,current,previous,channel,update.lock} \
             ~/.local/share/grove/",
            "ln -sf ~/.local/share/grove/current/bin/grove ~/.local/bin/grove",
            "grove off && grove on",
        ] {
            assert!(detail.contains(line), "{line} missing from {detail}");
        }

        assert_eq!(
            checks(&home, Some(testfix::SLUG))
                .iter()
                .filter(|c| c.check == CheckKind::InstallUnderHome)
                .count(),
            1,
            "the layout is one fact about the home — scoping to a root cannot hide it"
        );
    }

    /// The passing side, and the reason it is a row rather than a silence: a home with
    /// only its manifest carries no install, and an operator reading the report needs
    /// to see the layout was looked at.
    #[test]
    fn a_workspace_only_home_passes_the_install_check() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(roots::manifest_path(tmp.path()), "").unwrap();

        let out = install_checks(tmp.path());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].check, CheckKind::InstallUnderHome);
        assert_eq!(out[0].status, CheckStatus::Ok);
        assert!(!out[0].is_finding());
        assert!(out[0].detail.is_none(), "a pass carries no migration");
    }

    /// The v1 gap, closed: a root whose bare survived but whose trunk was deleted
    /// out from under it was invisible to doctor.
    #[test]
    fn a_vanished_trunk_is_reported_while_the_bare_stands() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);
        assert!(
            root_checks(&home, testfix::SLUG)
                .iter()
                .all(|c| c.status == CheckStatus::Ok),
            "a realized root passes every plumbing check"
        );

        std::fs::remove_dir_all(roots::trunk_dir(&home, testfix::SLUG)).unwrap();
        let out = root_checks(&home, testfix::SLUG);
        let trunk = out.iter().find(|c| c.check == CheckKind::Trunk).unwrap();
        assert_eq!(trunk.status, CheckStatus::Missing);
        assert_eq!(
            out.iter()
                .find(|c| c.check == CheckKind::Bare)
                .unwrap()
                .status,
            CheckStatus::Ok,
            "the bare is still there — that asymmetry is the finding"
        );
    }

    /// A declared root that was never cloned reports its bare and trunk and stops:
    /// there is no git to ask about worktrees, and a row per declared one saying so
    /// would bury the finding that explains them all.
    #[test]
    fn an_unrealized_root_reports_two_findings_and_no_worktree_noise() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        manifest::add_root(&roots::manifest_path(&home), "o/r", "git@example:o/r.git").unwrap();
        manifest::add_worktree(
            &roots::manifest_path(&home),
            "o/r",
            "feat",
            "feature/x",
            None,
        )
        .unwrap();

        let out = checks(&home, None);
        assert_eq!(
            out.iter()
                .filter(|c| c.status == CheckStatus::Missing)
                .count(),
            2,
            "exactly the bare and the trunk: {out:?}"
        );
        assert!(out.iter().all(|c| c.check != CheckKind::Worktree));
        assert!(
            out.iter()
                .any(|c| c.check == CheckKind::Manifest && c.status == CheckStatus::Ok),
            "the declaration itself is sound; it is merely unrealized"
        );
    }

    /// The other half of the spec'd gap: a worktree that is present but sitting on a
    /// branch nobody declared.
    #[test]
    fn a_worktree_on_the_wrong_branch_is_a_mismatch() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root_and_worktree(&tmp);
        let worktree = testfix::root_dir(&home, testfix::SLUG).join("feat");
        testfix::git(&worktree, &["switch", "-q", "-c", "somewhere-else"]);

        let out = root_checks(&home, testfix::SLUG);
        let wt = out
            .iter()
            .find(|c| c.check == CheckKind::Worktree && c.name.as_deref() == Some("feat"))
            .unwrap();
        assert_eq!(wt.status, CheckStatus::Mismatch);
        let detail = wt.detail.as_ref().unwrap();
        assert!(detail.contains("somewhere-else"), "{detail}");
    }

    /// Declared-vs-actual drift, the direction reconcile closes by adopting.
    #[test]
    fn an_undeclared_worktree_on_disk_is_drift() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);
        testfix::git(
            &roots::trunk_dir(&home, testfix::SLUG),
            &["worktree", "add", "-q", "-b", "stray", "../stray"],
        );

        let out = root_checks(&home, testfix::SLUG);
        let drift = out.iter().find(|c| c.check == CheckKind::Drift).unwrap();
        assert_eq!(drift.status, CheckStatus::Undeclared);
        assert_eq!(drift.name.as_deref(), Some("stray"));
    }

    /// A declared worktree whose directory was deleted out of band.
    #[test]
    fn a_declared_but_unrealized_worktree_is_missing() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);
        manifest::add_worktree(
            &roots::manifest_path(&home),
            testfix::SLUG,
            "feat",
            "feature/x",
            None,
        )
        .unwrap();

        let out = root_checks(&home, testfix::SLUG);
        let wt = out
            .iter()
            .find(|c| c.check == CheckKind::Worktree)
            .expect("the declared worktree is checked");
        assert_eq!(wt.status, CheckStatus::Missing);
        assert_eq!(wt.name.as_deref(), Some("feat"));
    }

    // ─── the legacy layout ──────────────────────────────────────────────────────

    /// A home holding one root in the layout grove laid down before the trunk was
    /// named by its branch: the bare at `.git`, the trunk checkout at `.trunk`, a
    /// declared worktree beside it, and a declared share linked through `../.trunk`.
    ///
    /// Built by hand rather than through `testfix`, whose fixtures build the layout
    /// that exists *now*. The shape this migration retires is written nowhere else in
    /// the tree, and one produced by grove's own writers would be a fixture that
    /// cannot regress with them.
    fn legacy_home(tmp: &TempDir) -> PathBuf {
        let src = tmp.path().join("src");
        testfix::fixture_repo(&src);

        let home = tmp.path().join("home");
        let root = home.join("code").join(testfix::SLUG);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            roots::manifest_path(&home),
            format!(
                "[roots.\"o/r\"]\nurl = \"{}\"\n\n\
                 [roots.\"o/r\".worktrees.feat]\nbranch = \"feature/x\"\n\n\
                 [roots.\"o/r\".env._]\nsymlink = [\".env\"]\n",
                src.display()
            ),
        )
        .unwrap();

        let bare = root.join(LEGACY_BARE);
        let trunk = root.join(LEGACY_TRUNK);
        let feat = root.join("feat");
        testfix::git(&home, &["clone", "-q", "--bare", path(&src), path(&bare)]);
        testfix::git(&bare, &["worktree", "add", "-q", path(&trunk), "main"]);
        testfix::git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/x",
                path(&feat),
                "main",
            ],
        );

        std::fs::write(trunk.join(".env"), "K=v\n").unwrap();
        std::os::unix::fs::symlink("../.trunk/.env", feat.join(".env")).unwrap();
        home
    }

    fn path(p: &Path) -> &str {
        p.to_str().expect("fixture paths are utf-8")
    }

    /// Report-only, and the report names the remedy: an operator who runs plain
    /// `grove doctor` learns both what is old about the root and what will fix it,
    /// and nothing on disk moves until they ask.
    #[test]
    fn a_legacy_layout_is_reported_with_the_command_that_migrates_it() {
        let tmp = TempDir::new().unwrap();
        let home = legacy_home(&tmp);
        let root = roots::root_dir(&home, testfix::SLUG);

        let out = root_checks(&home, testfix::SLUG);
        let legacy = out
            .iter()
            .find(|c| c.check == CheckKind::LegacyLayout)
            .expect("the layout is checked");
        assert_eq!(legacy.status, CheckStatus::Mismatch);
        let detail = legacy.detail.as_deref().unwrap();
        assert!(detail.contains("is a bare repo"), "{detail}");
        assert!(detail.contains("is the trunk checkout"), "{detail}");
        assert!(detail.contains("grove doctor --fix"), "{detail}");

        assert!(root.join(LEGACY_BARE).is_dir(), "the check is a read");
        assert!(root.join(LEGACY_TRUNK).is_dir(), "the check is a read");
    }

    /// The migration, end to end: the bare and the trunk take their current names,
    /// git still opens both checkouts, the share link follows the trunk, and the
    /// check that reported it now passes. Run twice, because a migration an operator
    /// re-runs after a crash must be a no-op the second time.
    #[test]
    fn fix_migrates_a_legacy_root_in_place_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let home = legacy_home(&tmp);
        let root = roots::root_dir(&home, testfix::SLUG);

        let (report, _) = run(&home, Some(testfix::SLUG), false, true).unwrap();
        assert!(
            report.iter().all(|row| !row.status.is_error()),
            "{report:?}"
        );

        assert!(roots::bare_dir(&home, testfix::SLUG).is_dir());
        assert!(!root.join(LEGACY_BARE).exists());
        assert!(!root.join(LEGACY_TRUNK).exists());

        let trunk = roots::trunk(&home, testfix::SLUG).unwrap();
        assert_eq!(trunk.name, "main", "the fixture integrates on `main`");
        assert!(trunk.dir.join("README.md").is_file());
        git::status(&trunk.dir).expect("the trunk reads as a checkout");
        git::status(&root.join("feat")).expect("the worktree reads as a checkout");
        assert_eq!(
            std::fs::read_link(root.join("feat").join(".env")).unwrap(),
            Path::new("../main/.env"),
            "the share follows the trunk to its new name"
        );

        let out = root_checks(&home, testfix::SLUG);
        assert!(
            out.iter().all(|c| c.status == CheckStatus::Ok),
            "a migrated root passes every plumbing check: {out:?}"
        );

        let (again, _) = run(&home, Some(testfix::SLUG), false, true).unwrap();
        assert!(again.iter().all(|row| !row.status.is_error()), "{again:?}");
        assert!(root.join("feat").join(".env").is_symlink());
    }

    /// A migration that died between the rename and everything after it leaves two
    /// bares. Only one of them holds the operator's history and grove cannot tell
    /// which, so `--fix` stops on that root and says so, rather than renaming a
    /// second directory over the first.
    #[test]
    fn a_bare_beside_the_legacy_one_refuses_rather_than_guessing() {
        let tmp = TempDir::new().unwrap();
        let home = legacy_home(&tmp);
        let root = roots::root_dir(&home, testfix::SLUG);
        let bare = roots::bare_dir(&home, testfix::SLUG);
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let (report, _) = run(&home, Some(testfix::SLUG), false, true).unwrap();
        assert!(
            report.iter().any(|row| row.status.is_error()
                && row
                    .reason
                    .as_deref()
                    .is_some_and(|why| why.contains("half-finished migration"))),
            "{report:?}"
        );
        assert!(root.join(LEGACY_BARE).is_dir(), "nothing was renamed");
        assert!(root.join(LEGACY_TRUNK).is_dir(), "nothing was renamed");

        let out = root_checks(&home, testfix::SLUG);
        let legacy = out
            .iter()
            .find(|c| c.check == CheckKind::LegacyLayout)
            .expect("the layout is checked");
        assert_eq!(legacy.status, CheckStatus::Mismatch);
        assert!(
            legacy
                .detail
                .as_deref()
                .unwrap()
                .contains("half-finished migration"),
            "{legacy:?}"
        );
    }
}
