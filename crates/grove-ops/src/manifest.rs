//! The manifest: `manifest.toml`, the durable *desired state* of which repos
//! (roots) exist. Edited format-preservingly via `toml_edit` so the server's
//! writes never clobber a human's comments/formatting. Structure + spacing are
//! canonicalized (roots as standalone tables sorted by slug, sub-tables regrouped,
//! worktrees by name, one blank line between tables — see `sort_doc` + `render`) for
//! stable diffs and clean merges: on the daemon's own writes via `write_doc`, and on a
//! human's hand-edit / git-sync via `canonicalize` (the Watcher's per-save hook). Sole
//! owner of the format — the CLI edits it offline and the daemon edits it in-process.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};
use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, Table, value};

/// A declared repository.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Root {
    pub slug: String,
    pub url: String,
}

/// A declared worktree, nested under its root (`[roots."<slug>".worktrees."<name>"]`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Worktree {
    pub name: String,
    pub branch: String,
    /// What it forked from; absent for worktrees adopted from git (unknown fork point).
    pub base: Option<String>,
}

/// A declared static share under `[roots."<slug>".env]` — a file the root shares
/// into every worktree, as a live `_.symlink` (link to the `.trunk` source) or a
/// seeded `_.copy` (an independent real file). `_.setup` (a per-worktree setup
/// command) is reserved for a later slice. See `docs/worktrees.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Share {
    pub path: String,
    pub mode: ShareMode,
}

/// How a share is materialized into a worktree: a live `Symlink` to the `.trunk`
/// source, or an independent `Copy` seeded from it (seed-once — the worktree owns
/// the copy thereafter, so it is never overwritten or GC'd).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareMode {
    Copy,
    Symlink,
}

/// A slug becomes a directory under `GROVE_HOME/code/` and a TOML key, so it must
/// be a safe relative path: non-empty, only `Normal` components (no `..`, no
/// absolute/root/`.`/prefix part), no backslash or control chars. This is the
/// chokepoint against path traversal from a crafted clone URL or a hand-edited /
/// git-synced manifest (e.g. `../../etc`).
pub fn validate_slug(slug: &str) -> Result<()> {
    let safe = !slug.is_empty()
        && !slug.contains('\\')
        && !slug.bytes().any(|b| b.is_ascii_control())
        && Path::new(slug)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if safe {
        Ok(())
    } else {
        bail!("invalid slug {slug:?}: must be a relative path with no '..' or absolute parts");
    }
}

/// A worktree `name` is one directory level under the root + a TOML key, so it
/// must be a single safe path segment — exactly one `Normal` component (stricter
/// than a slug: no `/`), no `..`/absolute/control/backslash — and never one of
/// grove's own RESERVED dirs (`.git`/`.trunk`/`.pool`), the same rejection
/// [`validate_share_path`] makes on a share's first segment.
///
/// The reserved gate is load-bearing, not tidiness. A worktree declared at
/// `<root>/.pool` is a *sibling of the slots and their parent*: `promote` sorts
/// slots by path, picks `<root>/.pool` ahead of `<root>/.pool/slot-0`, and
/// `git worktree move`s the entire warm pool into the user's new worktree —
/// warm-pool state destroyed, two registered worktrees nested inside a third.
/// `.trunk` is milder but also real: the declaration persists while the git-side
/// `worktree add` fails on the existing dir, leaving a permanent `failed` row on
/// every reconcile. Rejecting here covers both entrances at once — the API
/// (`add_worktree`) and a hand-edited/git-synced manifest (`list_worktrees`, which
/// skips names that fail this).
pub fn validate_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    let single_segment =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    let safe = !name.is_empty()
        && !name.contains('\\')
        && !name.bytes().any(|b| b.is_ascii_control())
        && !crate::worktrees::RESERVED.contains(&name)
        && single_segment;
    if safe {
        Ok(())
    } else {
        bail!(
            "invalid worktree name {name:?}: must be a single path segment with no '/' or '..', \
             and not one of grove's reserved dirs (.git/.trunk/.pool)"
        );
    }
}

/// A `branch`/`base` reaches `git worktree add` as a positional commit-ish. Reject
/// a value beginning with `-` so a crafted declaration can't smuggle a git flag
/// (e.g. `--orphan`). `git::worktree_add`'s `--` separator is the backstop for
/// hand-edited/git-synced manifests that bypass this; this rejects at the
/// declaration boundary, where a clear error beats a confusing git failure.
fn validate_ref_arg(kind: &str, v: &str) -> Result<()> {
    if v.is_empty() || v.starts_with('-') {
        bail!("invalid {kind} {v:?}: must be non-empty and not begin with '-'");
    }
    Ok(())
}

/// A share `path` is a RELATIVE path of one-or-more safe segments — nested
/// (`config/app.json`) is allowed, unlike a worktree `name`. It is `validate_slug`'s
/// twin with two added rejections: no segment may begin with `-` (anti-flag, like
/// [`validate_ref_arg`]), and the first segment may not be a RESERVED dir
/// (`.git`/`.trunk`/`.pool`) — a share must never alias grove's own dirs or link
/// into the bare. (`.env` is a dotfile *leaf*, not a reserved dir, so it passes.)
/// It must also be **canonical** — no `.`/empty (`//`)/trailing-`/` segments — so
/// the stored string equals what `grove_ops::env` materializes (else the link pass
/// and GC pass, which derive paths differently, would disagree on the same share).
/// The string gate; `grove_ops::env`'s `O_NOFOLLOW` descent is the filesystem gate.
pub fn validate_share_path(p: &str) -> Result<()> {
    let path = Path::new(p);
    let first_reserved = match path.components().next() {
        Some(Component::Normal(s)) => {
            crate::worktrees::RESERVED.contains(&s.to_str().unwrap_or(""))
        }
        _ => false,
    };
    // Every component must be a `Normal` segment that doesn't begin with `-`; any
    // `..`/absolute/`.`/prefix part fails the match.
    let all_segments_safe = path.components().all(|c| match c {
        Component::Normal(s) => !s.to_string_lossy().starts_with('-'),
        _ => false,
    });
    // Raw canonicality: `Path::components` silently drops `.`/`//`/trailing-`/`, so
    // require the literal string to have no such segments — keeps raw == normalized.
    let canonical = p
        .split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != "..");
    let safe = !p.is_empty()
        && !p.contains('\\')
        && !p.bytes().any(|b| b.is_ascii_control())
        && !first_reserved
        && canonical
        && all_segments_safe;
    if safe {
        Ok(())
    } else {
        bail!(
            "invalid share path {p:?}: must be a canonical relative path with no '..', \
             absolute parts, '.'/empty segments, leading '-', or reserved-dir prefix"
        );
    }
}

/// One declaration [`audit`] rejected, and why.
///
/// `slug` is the root key **as written**, so an unsafe one is reportable without
/// ever becoming a path; `item` names the worktree or share path inside it when the
/// problem is not the root itself.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Invalid {
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    pub reason: String,
}

/// Every declaration in the manifest that the validators reject.
///
/// The read side is deliberately *lenient* — [`list`], [`list_worktrees`] and
/// [`list_shares`] skip what they cannot safely use, so one hand-edited traversal key
/// can never wedge the reconciler or reach the filesystem. That leniency is silent by
/// construction: a typo'd worktree name simply never converges, and nothing says so.
/// This is the loud half, and doctor's `manifest` check is its only consumer — it
/// reports, and changes nothing.
///
/// An `Err` here is the file itself failing to parse, which is a finding of its own
/// rather than a failure of the audit.
pub fn audit(path: &Path) -> Result<Vec<Invalid>> {
    let doc = read_doc(path)?;
    let mut out = Vec::new();
    let Some(roots) = doc.get("roots").and_then(Item::as_table) else {
        return Ok(out);
    };

    for (slug, item) in roots {
        let mut invalid = |item: Option<&str>, reason: String| {
            out.push(Invalid {
                slug: slug.to_string(),
                item: item.map(ToOwned::to_owned),
                reason,
            });
        };
        if let Err(e) = validate_slug(slug) {
            invalid(None, format!("{e:#}"));
            // Everything below is keyed by this slug; reporting the key once is the
            // whole finding, and the entries under it are unreachable anyway.
            continue;
        }
        if item.get("url").and_then(|v| v.as_str()).is_none() {
            invalid(
                None,
                "root has no `url` string; it is never realized".into(),
            );
        }
        for (name, worktree) in worktrees_table(&doc, slug).into_iter().flatten() {
            if let Err(e) = validate_name(name) {
                invalid(Some(name), format!("{e:#}"));
                continue;
            }
            match worktree.get("branch").and_then(|v| v.as_str()) {
                None => invalid(
                    Some(name),
                    "worktree has no `branch` string; it is never realized".into(),
                ),
                Some(branch) => {
                    if let Err(e) = validate_ref_arg("branch", branch) {
                        invalid(Some(name), format!("{e:#}"));
                    }
                }
            }
            if let Some(base) = worktree.get("base").and_then(|v| v.as_str())
                && let Err(e) = validate_ref_arg("base", base)
            {
                invalid(Some(name), format!("{e:#}"));
            }
        }
        for (directive, paths) in shares_table(&doc, slug) {
            for p in paths {
                if let Err(e) = validate_share_path(&p) {
                    invalid(Some(&p), format!("`_.{directive}`: {e:#}"));
                }
            }
        }
    }

    out.sort_by(|a, b| a.slug.cmp(&b.slug).then_with(|| a.item.cmp(&b.item)));
    Ok(out)
}

/// The raw `_.symlink`/`_.copy` arrays under a root, unvalidated — [`list_shares`]'
/// input before it drops what it cannot use.
fn shares_table(doc: &DocumentMut, slug: &str) -> Vec<(&'static str, Vec<String>)> {
    let directives = env_table(doc, slug)
        .and_then(|env| env.get("_"))
        .and_then(Item::as_table);
    ["symlink", "copy"]
        .into_iter()
        .map(|directive| {
            let paths = directives
                .and_then(|d| d.get(directive))
                .and_then(Item::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            (directive, paths)
        })
        .collect()
}

/// All declared roots, sorted by slug. Entries with an unsafe slug are skipped —
/// a hand-edited traversal key never reaches the reconciler as a filesystem path.
// stele:landmark files-authoritative
pub fn list(path: &Path) -> Result<Vec<Root>> {
    let doc = read_doc(path)?;
    let mut roots = Vec::new();

    if let Some(table) = doc.get("roots").and_then(Item::as_table) {
        for (slug, item) in table {
            if validate_slug(slug).is_err() {
                continue;
            }
            if let Some(url) = item.get("url").and_then(|v| v.as_str()) {
                roots.push(Root {
                    slug: slug.to_string(),
                    url: url.to_string(),
                });
            }
        }
    }

    roots.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(roots)
}

/// Declare a root (idempotent — updates the url if the slug already exists).
pub fn add_root(path: &Path, slug: &str, url: &str) -> Result<()> {
    validate_slug(slug)?;
    with_manifest_mut(path, |doc| {
        let roots = doc
            .entry("roots")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("`roots` is not a table")?;
        roots.set_implicit(true); // emit only [roots."slug"], not a bare [roots]

        // Update url in place — don't replace the table, or nested worktrees vanish.
        let root = roots
            .entry(slug)
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("root is not a table")?;
        root["url"] = value(url);
        Ok(())
    })
}

/// Undeclare a root. No-op if absent.
pub fn remove_root(path: &Path, slug: &str) -> Result<()> {
    with_manifest_mut(path, |doc| {
        if let Some(roots) = doc.get_mut("roots").and_then(Item::as_table_mut) {
            roots.remove(slug);
        }
        Ok(())
    })
}

/// Declared worktrees under `slug`, sorted by name.
///
/// Unsafe declarations are skipped, not surfaced: an unsafe `name`, and — the same
/// posture applied to the values that become git arguments — an unsafe `branch` or
/// `base`. The write side ([`add_worktree`]) already rejects both, but a hand-edited
/// or git-synced manifest is declared input on this side, and everything read here
/// is handed to `git worktree add` by the reconciler. `doctor`'s audit is where an
/// operator sees the rejected row named.
pub fn list_worktrees(path: &Path, slug: &str) -> Result<Vec<Worktree>> {
    let doc = read_doc(path)?;
    let mut worktrees = Vec::new();

    if let Some(table) = worktrees_table(&doc, slug) {
        for (name, item) in table {
            if validate_name(name).is_err() {
                continue;
            }
            let base = item.get("base").and_then(|v| v.as_str());
            if base.is_some_and(|b| validate_ref_arg("base", b).is_err()) {
                continue;
            }
            if let Some(branch) = item.get("branch").and_then(|v| v.as_str()) {
                if validate_ref_arg("branch", branch).is_err() {
                    continue;
                }
                worktrees.push(Worktree {
                    name: name.to_string(),
                    branch: branch.to_string(),
                    base: base.map(String::from),
                });
            }
        }
    }

    worktrees.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(worktrees)
}

/// Declare a worktree under a root (idempotent). Errors if the root isn't declared.
pub fn add_worktree(
    path: &Path,
    slug: &str,
    name: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<()> {
    validate_slug(slug)?;
    validate_name(name)?;
    validate_ref_arg("branch", branch)?;
    if let Some(base) = base {
        validate_ref_arg("base", base)?;
    }
    with_manifest_mut(path, |doc| {
        let root = root_table_mut(doc, slug)?;

        let worktrees = root
            .entry("worktrees")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("`worktrees` is not a table")?;
        worktrees.set_implicit(true);

        // Update in place — don't replace the table, or a previously-recorded
        // `base` (and any future field) vanishes. `base` is a historical fork
        // point, not a mutable knob, and `None` means "unspecified", not "clear":
        // so re-declaring with no base preserves an earlier one. Only an explicit
        // `Some` overwrites it. This is `add_root`'s in-place fix, applied to the
        // leaf — idempotent means re-declaring preserves siblings, not wipes them.
        let entry = worktrees
            .entry(name)
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("worktree is not a table")?;
        entry["branch"] = value(branch);
        if let Some(base) = base {
            entry["base"] = value(base);
        }
        Ok(())
    })
}

/// Undeclare a worktree. No-op if absent.
pub fn remove_worktree(path: &Path, slug: &str, name: &str) -> Result<()> {
    with_manifest_mut(path, |doc| {
        // No-op if the root is undeclared (`root_table_mut` errs) — a remove never
        // bails on an absent root, unlike the `add_*`/`set_*` sites.
        if let Ok(root) = root_table_mut(doc, slug)
            && let Some(worktrees) = root.get_mut("worktrees").and_then(Item::as_table_mut)
        {
            worktrees.remove(name);
        }
        Ok(())
    })
}

fn worktrees_table<'a>(doc: &'a DocumentMut, slug: &str) -> Option<&'a Table> {
    doc.get("roots")
        .and_then(|r| r.get(slug))
        .and_then(|s| s.get("worktrees"))
        .and_then(Item::as_table)
}

/// The declared warm-pool target for a root (`[roots."<slug>".pool] size = N`), or
/// `0` when absent (the opt-in default — no pool unless the manifest asks for one).
/// Read-side leniency mirrors `list_*`: a malformed/negative/oversized `size` reads
/// as `0` rather than erroring, so a hand-edit never wedges the engine.
pub fn pool_size(path: &Path, slug: &str) -> Result<u32> {
    let doc = read_doc(path)?;
    Ok(doc
        .get("roots")
        .and_then(|r| r.get(slug))
        .and_then(|s| s.get("pool"))
        .and_then(|p| p.get("size"))
        .and_then(Item::as_integer)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0))
}

/// Declare a root's warm-pool target (`[roots."<slug>".pool] size = N`). Errors if
/// the root isn't declared. Idempotent — overwrites only `size`, preserving any
/// future sibling knobs (`branch`/`base`) and the rest of the doc.
pub fn set_pool_size(path: &Path, slug: &str, size: u32) -> Result<()> {
    validate_slug(slug)?;
    with_manifest_mut(path, |doc| {
        let root = root_table_mut(doc, slug)?;
        root.entry("pool")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("`pool` is not a table")?["size"] = value(i64::from(size));
        Ok(())
    })
}

/// Declared static shares under `[roots."<slug>".env]._.symlink` / `._.copy`,
/// sorted by path. Unsafe paths are skipped (the `list_worktrees` read-side posture
/// — a hand-edited traversal never reaches the materializer as a filesystem path).
/// Literal `KEY = value` env vars and the reserved `_.setup` directive are ignored.
///
/// **Symlink takes precedence:** a path declared as both `_.symlink` and `_.copy`
/// resolves to a single symlink share (the live link wins, deterministically), so
/// the materializer never sees two contradictory intents for one leaf.
///
/// Note: `_.symlink`/`_.copy` are TOML *dotted keys* — they nest as `env` → `_` →
/// `symlink`, not literal `"_.symlink"` keys (mise's `_` directive-namespace convention).
pub fn list_shares(path: &Path, slug: &str) -> Result<Vec<Share>> {
    let doc = read_doc(path)?;
    let directives = env_table(&doc, slug)
        .and_then(|env| env.get("_"))
        .and_then(Item::as_table);

    let mut out: Vec<Share> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (directive, mode) in [("symlink", ShareMode::Symlink), ("copy", ShareMode::Copy)] {
        let Some(arr) = directives
            .and_then(|d| d.get(directive))
            .and_then(Item::as_array)
        else {
            continue;
        };
        for p in arr.iter().filter_map(|v| v.as_str()) {
            // `seen.insert` both de-dups within a directive and enforces the
            // symlink-over-copy precedence across them.
            if validate_share_path(p).is_ok() && seen.insert(p.to_string()) {
                out.push(Share {
                    path: p.to_string(),
                    mode,
                });
            }
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Declare one or more shares under a root (idempotent — merged, de-duplicated,
/// sorted). Errors if the root isn't declared or any path is unsafe (the mutation
/// side bails, unlike the lenient read side). `mode` is the directive name
/// (`"symlink"` or `"copy"`).
pub fn add_share(path: &Path, slug: &str, mode: &str, paths: &[&str]) -> Result<()> {
    validate_slug(slug)?;
    for p in paths {
        validate_share_path(p)?;
    }
    with_manifest_mut(path, |doc| {
        let root = root_table_mut(doc, slug)?;

        let env = root
            .entry("env")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("`env` is not a table")?;
        // `_` renders as a dotted key (`_.symlink = [...]`), not a `[env._]` section.
        let directives = env
            .entry("_")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("`_` directives is not a table")?;
        directives.set_dotted(true);

        let arr = directives
            .entry(mode)
            .or_insert(value(Array::new()))
            .as_array_mut()
            .with_context(|| format!("`_.{mode}` is not an array"))?;

        // Merge in place (rebuild the value list, dedup + sort) — siblings/comments
        // elsewhere in the doc are untouched.
        let mut merged: BTreeSet<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        merged.extend(paths.iter().map(|p| (*p).to_string()));
        *arr = merged.into_iter().collect();
        Ok(())
    })
}

/// Undeclare one share path from a root's `_.<mode>` list. No-op if absent; an empty
/// list is left in place (the next materialize GCs the orphaned worktree symlinks).
pub fn remove_share(path: &Path, slug: &str, mode: &str, p: &str) -> Result<()> {
    with_manifest_mut(path, |doc| {
        // No-op if the root is undeclared (`root_table_mut` errs) — a remove never
        // bails on an absent root, unlike the `add_*`/`set_*` sites.
        if let Ok(root) = root_table_mut(doc, slug)
            && let Some(arr) = root
                .get_mut("env")
                .and_then(Item::as_table_mut)
                .and_then(|e| e.get_mut("_"))
                .and_then(Item::as_table_mut)
                .and_then(|d| d.get_mut(mode))
                .and_then(Item::as_array_mut)
        {
            arr.retain(|v| v.as_str() != Some(p));
        }
        Ok(())
    })
}

fn env_table<'a>(doc: &'a DocumentMut, slug: &str) -> Option<&'a Table> {
    doc.get("roots")
        .and_then(|r| r.get(slug))
        .and_then(|s| s.get("env"))
        .and_then(Item::as_table)
}

/// The one read-modify-write seam every mutator runs through — it owns the manifest
/// mutation invariant so no mutator re-spells it:
///
/// 1. **Lock.** Acquire an exclusive advisory flock on a *sibling* `.lock` file (never
///    the manifest itself — see [`with_lock`]), serializing every writer: two CLI
///    invocations, or the CLI and a running daemon. The lock spans the whole RMW, so
///    two writers can't both load v1 and lose an edit.
/// 2. **Read.** Parse the current doc; an empty/missing file yields a fresh
///    `DocumentMut` (`read_doc`'s default), so the first mutation bootstraps the file.
/// 3. **Edit.** Run `f` against the mutable doc.
/// 4. **Write — only on `Ok`.** Publish via `write_doc`'s canonicalizing atomic
///    temp-file + rename. On `Err`, `f`'s error propagates *before* any write: a failed
///    mutation releases the lock and writes nothing (the manifest is left untouched).
///
/// Reads (`list`) need no lock: the atomic temp+rename means a reader always sees a
/// whole file (old-or-new), never a torn half-write.
fn with_manifest_mut<T>(path: &Path, f: impl FnOnce(&mut DocumentMut) -> Result<T>) -> Result<T> {
    with_lock(path, || {
        let mut doc = read_doc(path)?;
        let out = f(&mut doc)?;
        write_doc(path, &mut doc)?;
        Ok(out)
    })
}

/// The mutable `[roots."<slug>"]` table, or the `root {slug} is not declared` error the
/// `add_*`/`set_*` mutators surface when a caller edits a root that was never declared.
/// The five root-scoped mutators navigate through this instead of re-spelling the
/// `doc["roots"][slug]` chain; the `remove_*` sites call it under `if let Ok(..)` to
/// keep their "no-op if absent" behavior rather than bailing.
fn root_table_mut<'a>(doc: &'a mut DocumentMut, slug: &str) -> Result<&'a mut Table> {
    doc.get_mut("roots")
        .and_then(Item::as_table_mut)
        .and_then(|r| r.get_mut(slug))
        .and_then(Item::as_table_mut)
        .with_context(|| format!("root {slug} is not declared"))
}

/// The flock primitive [`with_manifest_mut`] and [`canonicalize`] build on: an exclusive
/// advisory lock on a sibling `.lock` file, so concurrent writers — two CLI invocations,
/// or the CLI and a running daemon — can't lose each other's edits. The lock is
/// per-open-file-description, so it serializes threads too, and releases when the handle
/// is dropped (RAII — a bail mid-edit or a panic never wedges the next writer).
fn with_lock<T>(manifest_path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = manifest_path.with_extension("toml.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).context("create manifest dir")?;
    }
    // O_NOFOLLOW: refuse a symlinked lock path so a pre-placed symlink can't
    // redirect us into truncating an arbitrary file.
    //
    // O_CLOEXEC is load-bearing, not hygiene: the lock lives on the open file
    // *description*, which every `Command::spawn` in this process inherits. Grove now
    // holds this lock in the same process that spawns git — a `roots::remove` on one
    // thread, a `git clone` on another — and git's own children (ssh ControlPersist,
    // credential helpers) outlive the command that started them. Without it, `drop(lock)`
    // below releases nothing while such a grandchild lives, and every later manifest
    // write wedges — in this process and in every CLI invocation — with no diagnostic.
    // (`rustix::fs::open` passes exactly the flags given; `std` adds O_CLOEXEC for you.)
    // Pinned by `the_manifest_lock_does_not_leak_into_a_spawned_child`.
    let lock = open(
        &lock_path,
        OFlags::CLOEXEC | OFlags::CREATE | OFlags::WRONLY | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .with_context(|| format!("open manifest lock {}", lock_path.display()))?;
    acquire(&lock, &lock_path)?;
    let result = f();
    drop(lock); // releases the advisory lock
    result
}

/// How long a writer waits for the manifest lock before giving up.
///
/// Every critical section this lock guards is an in-memory `toml_edit` edit plus an
/// atomic rename — microseconds — so a wait this long means the holder is wedged,
/// not busy. Bounded rather than blocking because the lock is global: an unbounded
/// wait turns one stuck holder into every mutation on every root hanging forever,
/// with the HTTP request never answering and the CLI printing nothing. Inside the
/// client's own op budget, so the operator gets grove's diagnosis rather than a
/// transport timeout.
const LOCK_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

/// Take the exclusive lock, bounded by [`LOCK_BUDGET`].
///
/// Non-blocking + retry rather than a blocking `flock`: there is no way to bound the
/// blocking form, and `alarm`/`SIGALRM` is not a thing a library may install in a
/// process it does not own. The poll interval is short enough to be invisible at the
/// scale of a real critical section.
fn acquire(lock: &impl rustix::fd::AsFd, lock_path: &Path) -> Result<()> {
    let clock = crate::clock::SystemClock;
    let deadline = crate::clock::Clock::deadline(&clock, LOCK_BUDGET);
    loop {
        match flock(lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(()),
            Err(rustix::io::Errno::WOULDBLOCK | rustix::io::Errno::INTR) => {
                if deadline.expired(&clock) {
                    bail!(
                        "manifest lock {} is held by another process (waited {}s)",
                        lock_path.display(),
                        LOCK_BUDGET.as_secs()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e).context("lock manifest"),
        }
    }
}

fn read_doc(path: &Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(s) => s.parse::<DocumentMut>().context("parse manifest"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e).context("read manifest"),
    }
}

fn write_doc(path: &Path, doc: &mut DocumentMut) -> Result<()> {
    sort_doc(doc);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create manifest dir")?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, render(doc)).context("write manifest")?;
    std::fs::rename(&tmp, path).context("atomic rename manifest")?;
    Ok(())
}

/// The canonical on-disk text for a (sorted) document: `toml_edit`'s render with
/// spacing normalized. Used by both [`write_doc`] and [`canonicalize`] so a grove
/// write and a hand-edit's re-sort land byte-identical output (the loop-safety the
/// Watcher relies on).
fn render(doc: &DocumentMut) -> String {
    normalize_blank_lines(&doc.to_string())
}

/// Normalize vertical spacing: collapse any run of blank lines to a single one, and
/// guarantee exactly one blank line before each table header `[…]` (except at the top
/// of the file). So a save lands consistent spacing however the hand-edit was spaced.
/// String/comment/bracket state is tracked *across* lines so a `[`-leading line inside
/// a multi-line array **or a `"""…"""`/`'''…'''` string value** is never mistaken for a
/// header (and blank lines *inside* such a string are preserved verbatim, never
/// collapsed — collapsing them would corrupt the value). Idempotent: re-normalizing
/// normalized text is a no-op.
fn normalize_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_blank = false;
    let mut depth: i32 = 0;
    let mut in_ml: Option<char> = None; // open multi-line string delimiter (`"`/`'`)
    for line in s.lines() {
        // A line that BEGINS inside a multi-line string is verbatim content: never a
        // header, never blank-collapsed. Still scan it — the string may close here
        // and real TOML (brackets, the next header) resume on the same line.
        if in_ml.is_some() {
            scan_line(line, &mut depth, &mut in_ml);
            depth = depth.max(0);
            out.push_str(line);
            out.push('\n');
            pending_blank = false;
            continue;
        }
        if line.trim().is_empty() {
            pending_blank = true;
            continue;
        }
        let is_header = depth == 0 && line.trim_start().starts_with('[');
        if !out.is_empty() && (pending_blank || is_header) {
            out.push('\n'); // exactly one blank line
        }
        out.push_str(line);
        out.push('\n');
        pending_blank = false;
        scan_line(line, &mut depth, &mut in_ml);
        depth = depth.max(0);
    }
    out
}

/// Scan one line, updating the cross-line `depth` (net unclosed `[`/`{`, brackets
/// inside strings/comments ignored) and `in_ml` (the open multi-line string delimiter,
/// `"` for `"""` or `'` for `'''`, carried to the next line). Single-line `"…"`/`'…'`
/// strings and `#` comments are consumed within the line. This is what lets
/// `normalize_blank_lines` tell a table header from a `[`-leading line inside a value.
fn scan_line(line: &str, depth: &mut i32, in_ml: &mut Option<char>) {
    let b = line.as_bytes();
    let mut i = 0;
    let mut single: Option<char> = None; // open single-line string delimiter
    while i < b.len() {
        // Inside a multi-line string (opened here or on a prior line): consume to its
        // triple-close. A basic (`"`) string honors `\` escapes; a literal (`'`) doesn't.
        if let Some(q) = *in_ml {
            if starts_triple(b, i, q) {
                *in_ml = None;
                i += 3;
            } else if q == '"' && b[i] == b'\\' {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(q) = single {
            if q == '"' && b[i] == b'\\' {
                i += 2;
            } else {
                if b[i] == q as u8 {
                    single = None;
                }
                i += 1;
            }
            continue;
        }
        match b[i] {
            q @ (b'"' | b'\'') => {
                if starts_triple(b, i, q as char) {
                    *in_ml = Some(q as char);
                    i += 3;
                } else {
                    single = Some(q as char);
                    i += 1;
                }
            }
            b'#' => break, // comment runs to end of line
            b'[' | b'{' => {
                *depth += 1;
                i += 1;
            }
            b']' | b'}' => {
                *depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
}

/// Are bytes `i..i+3` all the quote char `q` — the open/close of a `"""`/`'''` string?
fn starts_triple(b: &[u8], i: usize, q: char) -> bool {
    let q = q as u8;
    b.get(i) == Some(&q) && b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q)
}

/// Sort the manifest in place, rewriting **only when that changes the bytes** —
/// the hook a watcher calls on every manifest *save* (a hand-edit / git-sync), where
/// no other mutation runs to trigger [`write_doc`]'s sort. Returns whether it rewrote.
///
/// **Loop-safe by construction.** The Watcher re-runs this on the file-change event
/// our own write produces, so we must only ever write a string that re-sorts to
/// *itself* — otherwise: write → event → write → … forever. `sort_doc` is byte-stable
/// on a clean manifest but can take a pass or two to settle a messy hand-edit (mixed
/// dotted-key/standalone roots), so iterate to a fixpoint first and write only that.
/// If it won't settle within a small bound, leave the file untouched: an unsorted
/// manifest is strictly better than a write loop. A missing file is a no-op; a
/// malformed one errors (can't sort what won't parse).
pub fn canonicalize(path: &Path) -> Result<bool> {
    with_lock(path, || {
        let original = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).context("read manifest"),
        };

        let mut current = original.clone();
        let mut settled = false;
        for _ in 0..8 {
            let mut doc: DocumentMut = current.parse().context("parse manifest")?;
            sort_doc(&mut doc);
            let next = render(&doc);
            if next == current {
                settled = true; // re-rendering `current` yields `current` — a fixpoint
                break;
            }
            current = next;
        }
        // Only write a verified fixpoint: the Watcher's re-run on it is a guaranteed
        // no-op. A non-settling messy edit is left as the user saved it.
        if !settled || current == original {
            return Ok(false);
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, &current).context("write manifest")?;
        std::fs::rename(&tmp, path).context("atomic rename manifest")?;
        Ok(true)
    })
}

/// Canonicalize structure on every save so diffs are stable and a git-synced manifest
/// merges cleanly: every root rendered as a standalone `[roots."slug"]` table (dotted
/// `"slug".url = …` entries normalized away), roots sorted by slug with each root's
/// sub-tables (`pool`/`env`/`hooks`/`worktrees`) regrouped contiguously beneath it, and
/// worktrees sorted by name. A root's scalar keys keep their order, and share arrays
/// (`_.symlink`/`_.copy`) are already sorted at declaration. Spacing is normalized
/// separately in [`render`]. Comments/decor travel with their item. Not always a
/// single-pass fixpoint on a messy hand-edit — [`canonicalize`] iterates it to settle.
fn sort_doc(doc: &mut DocumentMut) {
    let Some(roots) = doc.get_mut("roots").and_then(Item::as_table_mut) else {
        return;
    };
    // Normalize representation so every root is a standalone `[roots."slug"]` table,
    // never a dotted `"slug".url = …` entry under a bare `[roots]`. Only once they are
    // all sub-tables can they be globally slug-sorted — TOML requires a table's dotted
    // keys to precede its sub-table headers, which otherwise pins a dotted root above a
    // standalone one regardless of slug.
    roots.set_implicit(true);
    for (_slug, root) in roots.iter_mut() {
        if let Some(table) = root.as_table_mut() {
            table.set_dotted(false);
        }
    }

    // Render order is driven by each standalone table's `doc_position`. Reassigning
    // only the roots' positions can't pull a root's *scattered* sub-tables back under
    // it, so walk the now-key-sorted tree depth-first and hand out fresh, dense,
    // increasing positions — making every root's subtree contiguous and the roots
    // globally slug-sorted. Anchor at the roots' current minimum position so any
    // non-`roots` top-level table keeps its place around the block.
    let base = roots
        .iter()
        .filter_map(|(_, item)| item.as_table().and_then(Table::position))
        .min()
        .unwrap_or(0);
    let mut pos = base;
    position_subtables(roots, &mut pos);
}

/// Sort a table's sub-tables by key and assign each (depth-first) the next sequential
/// `doc_position` from `*pos`, so a parent and its sub-tables render as one contiguous,
/// key-ordered block. Dotted sub-tables (a `_` directive group) are skipped — they
/// render inline as `_.symlink = …`, and giving them a position would split them into
/// a standalone `[…._]` header.
fn position_subtables(table: &mut Table, pos: &mut isize) {
    table.sort_values();
    for (_, item) in table.iter_mut() {
        if let Some(sub) = item.as_table_mut() {
            if sub.is_dotted() {
                continue;
            }
            sub.set_position(Some(*pos));
            *pos += 1;
            position_subtables(sub, pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn manifest(dir: &TempDir) -> std::path::PathBuf {
        dir.path().join("manifest.toml")
    }

    #[test]
    fn validate_name_requires_a_single_safe_segment() {
        for ok in ["my-feature", "wt1", "..foo"] {
            assert!(validate_name(ok).is_ok(), "{ok:?} should be ok");
        }
        // `.git`/`.trunk`/`.pool` are grove's own dirs: a worktree declared at one
        // of them either wedges reconcile (`.trunk`) or lets `promote` move the whole
        // warm pool into a user worktree (`.pool`).
        for bad in [
            "", "a/b", "..", ".", "/abs", "a\\b", ".git", ".trunk", ".pool",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn worktree_add_list_remove_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();

        add_worktree(&path, "o/r", "feat", "feature/x", Some("main")).unwrap();
        assert_eq!(
            list_worktrees(&path, "o/r").unwrap(),
            vec![Worktree {
                name: "feat".into(),
                branch: "feature/x".into(),
                base: Some("main".into())
            }]
        );

        remove_worktree(&path, "o/r", "feat").unwrap();
        assert_eq!(list_worktrees(&path, "o/r").unwrap(), vec![]);
    }

    #[test]
    fn add_root_preserves_nested_worktrees() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u1").unwrap();
        add_worktree(&path, "o/r", "feat", "feature/x", None).unwrap();

        // Re-declaring the root (e.g. a url update) must NOT wipe its worktrees.
        add_root(&path, "o/r", "u2").unwrap();
        assert_eq!(list(&path).unwrap()[0].url, "u2");
        assert_eq!(list_worktrees(&path, "o/r").unwrap().len(), 1);
    }

    #[test]
    fn add_worktree_is_idempotent_and_preserves_base() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();
        add_worktree(&path, "o/r", "feat", "feature/x", Some("main")).unwrap();

        // Re-declaring with no base must not wipe the recorded fork point...
        add_worktree(&path, "o/r", "feat", "feature/x", None).unwrap();
        let wts = list_worktrees(&path, "o/r").unwrap();
        assert_eq!(wts.len(), 1, "no duplicate entry");
        assert_eq!(
            wts[0].base.as_deref(),
            Some("main"),
            "base survived re-declare"
        );

        // ...but an explicit base still overwrites.
        add_worktree(&path, "o/r", "feat", "feature/x", Some("dev")).unwrap();
        assert_eq!(
            list_worktrees(&path, "o/r").unwrap()[0].base.as_deref(),
            Some("dev")
        );
    }

    #[test]
    fn add_worktree_requires_a_declared_root_and_validates() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        assert!(
            add_worktree(&path, "o/r", "feat", "b", None).is_err(),
            "no root"
        );
        add_root(&path, "o/r", "u").unwrap();
        assert!(
            add_worktree(&path, "o/r", "../escape", "b", None).is_err(),
            "bad name"
        );
    }

    #[test]
    fn add_worktree_rejects_flag_shaped_branch_and_base() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();
        // A branch/base beginning with `-` could be parsed as a `git` flag.
        assert!(add_worktree(&path, "o/r", "feat", "--orphan", None).is_err());
        assert!(add_worktree(&path, "o/r", "feat", "ok", Some("--no-checkout")).is_err());
        // Nothing got written for the rejected declarations.
        assert_eq!(list_worktrees(&path, "o/r").unwrap(), vec![]);
    }

    /// The READ side rejects them too. A hand-edited manifest is declared input, and
    /// what the reconciler reads here goes straight to `git worktree add` — where
    /// `branch = "-D"` with a base reaches `git branch -D <base>` and force-deletes
    /// it from the bare. Skipped, like an unsafe name: doctor's audit is where the
    /// operator sees the row named.
    #[test]
    fn list_worktrees_skips_a_flag_shaped_branch_or_base() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        std::fs::write(
            &path,
            "[roots.\"o/r\"]\nurl = \"u\"\n\
             [roots.\"o/r\".worktrees.evil]\nbranch = \"-D\"\nbase = \"main\"\n\
             [roots.\"o/r\".worktrees.alsoevil]\nbranch = \"feat\"\nbase = \"-D\"\n\
             [roots.\"o/r\".worktrees.fine]\nbranch = \"feature/x\"\n",
        )
        .unwrap();

        let names: Vec<String> = list_worktrees(&path, "o/r")
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, vec!["fine".to_string()]);
    }

    #[test]
    fn add_then_list_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);

        assert_eq!(list(&path).unwrap(), vec![]);
        add_root(&path, "owner/repo", "https://x/owner/repo.git").unwrap();

        assert_eq!(
            list(&path).unwrap(),
            vec![Root {
                slug: "owner/repo".into(),
                url: "https://x/owner/repo.git".into()
            }]
        );
    }

    #[test]
    fn remove_drops_the_root() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "a/b", "u1").unwrap();
        add_root(&path, "c/d", "u2").unwrap();

        remove_root(&path, "a/b").unwrap();

        let slugs: Vec<_> = list(&path).unwrap().into_iter().map(|r| r.slug).collect();
        assert_eq!(slugs, vec!["c/d"]);
    }

    #[test]
    fn add_is_idempotent_and_updates_url() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "a/b", "u1").unwrap();
        add_root(&path, "a/b", "u2").unwrap();

        let roots = list(&path).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].url, "u2");
    }

    #[test]
    fn add_root_rejects_traversal_slugs() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        for bad in ["../escape", "a/../../b", "/abs", "..", "", "a\\b"] {
            assert!(add_root(&path, bad, "u").is_err(), "should reject {bad:?}");
        }
        // a literal dotted name is fine (not traversal)
        assert!(add_root(&path, "a/..foo", "u").is_ok());
    }

    #[test]
    fn list_skips_unsafe_hand_edited_slugs() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        // Hand-edit a manifest with a traversal key alongside a good one.
        std::fs::write(
            &path,
            "[roots.\"../../evil\"]\nurl = \"u1\"\n[roots.\"o/r\"]\nurl = \"u2\"\n",
        )
        .unwrap();
        let slugs: Vec<_> = list(&path).unwrap().into_iter().map(|r| r.slug).collect();
        assert_eq!(slugs, vec!["o/r"], "traversal key filtered out");
    }

    #[test]
    fn concurrent_add_root_loses_no_writes() {
        use std::thread;
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);

        let handles: Vec<_> = (0..20)
            .map(|i| {
                let p = path.clone();
                thread::spawn(move || add_root(&p, &format!("o/r{i:02}"), "u").unwrap())
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        // Without the lock, the read-modify-write races would drop some entries.
        assert_eq!(
            list(&path).unwrap().len(),
            20,
            "no lost updates under contention"
        );
    }

    #[test]
    fn concurrent_mixed_mutations_lose_no_writes() {
        use std::thread;
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();

        // A worktree-add and a pool-resize, racing on one root's nested tables —
        // the flock must serialize the load→edit→write so neither clobbers the other.
        let handles: Vec<_> = (0..20)
            .map(|i| {
                let p = path.clone();
                thread::spawn(move || {
                    if i % 2 == 0 {
                        add_worktree(&p, "o/r", &format!("w{i:02}"), "b", None).unwrap();
                    } else {
                        set_pool_size(&p, "o/r", i).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            list_worktrees(&path, "o/r").unwrap().len(),
            10,
            "every add_worktree landed — no resize clobbered a worktree write"
        );
    }

    #[test]
    fn reads_never_tear_under_concurrent_writes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (p, stop) = (path.clone(), stop.clone());
            thread::spawn(move || {
                let mut i = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    add_worktree(&p, "o/r", &format!("w{i:03}"), "b", None).unwrap();
                    i = i.wrapping_add(1);
                }
            })
        };

        // A reader racing the writer must always parse a whole manifest (atomic
        // temp+rename means it sees old-or-new, never a torn half-write).
        for _ in 0..500 {
            list_worktrees(&path, "o/r").expect("reader saw a complete, parseable manifest");
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn a_failed_mutation_releases_the_lock() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        // No declared root → add_worktree bails *inside* the lock scope.
        assert!(add_worktree(&path, "o/r", "feat", "b", None).is_err());
        // A subsequent mutation must not deadlock — the dropped File released the flock.
        add_root(&path, "o/r", "u").unwrap();
        add_worktree(&path, "o/r", "feat", "b", None).unwrap();
        assert_eq!(list_worktrees(&path, "o/r").unwrap().len(), 1);
    }

    #[test]
    fn lock_uses_a_sibling_file_not_the_manifest() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        let lock = path.with_extension("toml.lock");
        assert!(!lock.exists(), "no lockfile before any mutation");

        add_root(&path, "o/r", "u").unwrap();
        assert!(lock.exists(), "lockfile created on first mutation");
        assert_ne!(lock, path, "the lock is a sibling, not the manifest itself");

        // Reused (not recreated) on the next mutation — and the manifest still parses.
        add_root(&path, "c/d", "u").unwrap();
        assert!(lock.exists());
        assert_eq!(list(&path).unwrap().len(), 2);
    }

    #[test]
    fn add_preserves_existing_comments() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        std::fs::write(&path, "# my repos\n[roots.\"a/b\"]\nurl = \"u1\"\n").unwrap();

        add_root(&path, "c/d", "u2").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my repos"), "comment survived the edit");
        assert!(text.contains("c/d"));
        assert_eq!(list(&path).unwrap().len(), 2);
    }

    #[test]
    fn write_sorts_roots_by_slug_and_worktrees_by_name() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        // Declare out of order; the canonicalizing save orders them.
        add_root(&path, "z/last", "u").unwrap();
        add_root(&path, "a/first", "u").unwrap();
        add_root(&path, "m/mid", "u").unwrap();
        add_worktree(&path, "m/mid", "wt-z", "branch/z", None).unwrap();
        add_worktree(&path, "m/mid", "wt-a", "branch/a", None).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let (a, m, z) = (
            text.find("a/first").unwrap(),
            text.find("m/mid").unwrap(),
            text.find("z/last").unwrap(),
        );
        assert!(a < m && m < z, "roots sorted by slug:\n{text}");
        assert!(
            text.find("wt-a").unwrap() < text.find("wt-z").unwrap(),
            "worktrees sorted by name:\n{text}"
        );
    }

    #[test]
    fn canonicalize_sorts_a_hand_edit_then_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        // A user hand-saves an out-of-order manifest.
        std::fs::write(
            &path,
            "[roots.\"z/z\"]\nurl = \"u\"\n\n[roots.\"a/a\"]\nurl = \"u\"\n",
        )
        .unwrap();

        assert!(canonicalize(&path).unwrap(), "reordered ⇒ rewritten");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.find("a/a").unwrap() < text.find("z/z").unwrap(),
            "sorted:\n{text}"
        );
        // The self-triggered re-run reads an already-sorted file ⇒ no write (the
        // watcher loop-breaker).
        assert!(!canonicalize(&path).unwrap(), "already sorted ⇒ no rewrite");
    }

    #[test]
    fn canonicalize_preserves_non_roots_tables() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        // A top-level table that is NOT under [roots] (scratch / future global config).
        std::fs::write(
            &path,
            "[\"scratch/thing\"]\nfoo = \"bar\"\n\n[roots.\"b/b\"]\nurl = \"u\"\n\n[roots.\"a/a\"]\nurl = \"u\"\n",
        )
        .unwrap();

        canonicalize(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[\"scratch/thing\"]") && text.contains("foo = \"bar\""),
            "non-roots table byte-preserved:\n{text}"
        );
        assert!(
            text.find("a/a").unwrap() < text.find("b/b").unwrap(),
            "roots sorted"
        );
    }

    #[test]
    fn canonicalize_missing_file_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        assert!(!canonicalize(&manifest(&tmp)).unwrap());
    }

    #[test]
    fn canonicalize_reaches_a_fixpoint_on_a_messy_mixed_manifest() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        // Mixed dotted-key + standalone tables, out of order — the shape a hand-edit
        // can leave. The invariant under test (the watcher's loop-safety): whatever
        // canonicalize writes must re-sort to itself, so a second pass writes nothing.
        std::fs::write(
            &path,
            "[roots]\n\"o/b\".url = \"u\"\n\"o/a\".pool.size = 2\n\"o/a\".url = \"u\"\n\n\
             [roots.\"o/a\".worktrees.w]\nbranch = \"x\"\n\n[roots.\"n/z\"]\nurl = \"u\"\n",
        )
        .unwrap();

        // Must terminate (the bug was an unbounded write loop on exactly this shape).
        let _ = canonicalize(&path).unwrap();
        assert!(
            !canonicalize(&path).unwrap(),
            "post-canonicalize file is a byte-fixpoint — the watcher's re-run is a no-op"
        );
        // And the data is intact + still parses to the same root set.
        let mut slugs: Vec<String> = list(&path).unwrap().into_iter().map(|r| r.slug).collect();
        slugs.sort();
        assert_eq!(slugs, ["n/z", "o/a", "o/b"]);
    }

    #[test]
    fn canonicalize_normalizes_spacing() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        // No blank between the env block and the next root; three blank lines before
        // the last. Want: exactly one blank line before every table header.
        std::fs::write(
            &path,
            "[roots.\"a/a\"]\nurl = \"u\"\n[roots.\"a/a\".env]\n_.symlink = [\".env\"]\n\n\n\n[roots.\"b/b\"]\nurl = \"u\"\n",
        )
        .unwrap();

        canonicalize(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("\n\n\n"),
            "no run of 2+ blank lines:\n{text}"
        );
        assert!(
            text.contains("_.symlink = [\".env\"]\n\n[roots.\"b/b\"]"),
            "exactly one blank line before each header:\n{text}"
        );
        assert!(!canonicalize(&path).unwrap(), "spacing is a fixpoint");
    }

    #[test]
    fn canonicalize_regroups_and_sorts_scattered_dotted_roots() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        // The hard shape: dotted `[roots]` url entries + standalone sub-tables, a root's
        // pieces scattered and interleaved with another root, all out of slug order.
        std::fs::write(
            &path,
            "[roots]\n\"o/jlg\".url = \"b\"\n\n[roots.\"o/jlg\".env]\n_.symlink = [\".env\"]\n\
             [roots.\"a/cc\"]\nurl = \"d\"\n\n[roots.\"o/jlg\".hooks]\nsetup = \"s\"\n",
        )
        .unwrap();

        canonicalize(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // Globally slug-sorted, and jlg's scattered sub-tables regrouped under it.
        let cc = text.find("[roots.\"a/cc\"]").unwrap();
        let jlg = text.find("[roots.\"o/jlg\"]").unwrap();
        let env = text.find("[roots.\"o/jlg\".env]").unwrap();
        let hooks = text.find("[roots.\"o/jlg\".hooks]").unwrap();
        assert!(cc < jlg, "a/cc sorts before o/jlg:\n{text}");
        assert!(
            jlg < env && env < hooks,
            "jlg's sub-tables regrouped under it:\n{text}"
        );
        assert!(
            !text.contains("[roots]\n"),
            "dotted entries normalized to standalone:\n{text}"
        );
        assert!(!canonicalize(&path).unwrap(), "reaches a fixpoint");
    }

    #[test]
    fn normalize_blank_lines_leaves_multiline_array_brackets_alone() {
        // A `[`-leading line inside a multi-line array is array data, not a header —
        // it must not get a blank line forced before it.
        let input = "[roots.\"o/r\".setup]\nlink = [\n  \"a\",\n  \"b\",\n]\n";
        assert_eq!(
            normalize_blank_lines(input),
            input,
            "multi-line array untouched"
        );
    }

    #[test]
    fn normalize_blank_lines_leaves_a_multiline_string_intact() {
        // A `"""…"""` value whose content has a `[`-leading line AND an internal blank
        // line: the `[` line must not get a blank forced before it (it isn't a header),
        // and the blank INSIDE the string must be preserved (collapsing it would
        // corrupt the value). The whole thing round-trips unchanged and is idempotent.
        let input =
            "[roots.\"o/r\".hooks]\nsetup = \"\"\"\nfirst\n[not-a-header]\n\nafter\n\"\"\"\n";
        let out = normalize_blank_lines(input);
        assert!(
            !out.contains("\n\n[not-a-header]"),
            "no blank injected before a bracket line inside the string:\n{out}"
        );
        assert!(
            out.contains("[not-a-header]\n\nafter"),
            "internal blank line preserved:\n{out}"
        );
        assert_eq!(out, input, "the value is untouched");
        assert_eq!(normalize_blank_lines(&out), out, "idempotent");
    }

    #[test]
    fn sort_on_save_is_stable() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "a/x", "u").unwrap();
        add_root(&path, "b/y", "u").unwrap();
        let once = std::fs::read_to_string(&path).unwrap();

        // A no-op-shaped edit (re-add the same url) re-saves; an already-sorted
        // document must round-trip byte-identical.
        add_root(&path, "a/x", "u").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            once,
            "stable re-save"
        );
    }

    // --- shares (`[env]._.symlink`) ----------------------------------------

    #[test]
    fn validate_share_path_accepts_safe_relative_and_dotfile_paths() {
        for ok in [
            ".env",
            ".env.secrets",
            "tsconfig.base.json",
            "config/app.json",
            "a/b/c.toml",
            "a/..foo",
        ] {
            assert!(validate_share_path(ok).is_ok(), "{ok:?} should be ok");
        }
    }

    #[test]
    fn validate_share_path_rejects_traversal_absolute_control_backslash() {
        for bad in [
            "",
            "..",
            "a/../../b",
            "../escape",
            "/etc/passwd",
            "a\\b",
            "a\x01b",
            ".",
        ] {
            assert!(
                validate_share_path(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_share_path_rejects_noncanonical_segments() {
        // `.`/`//`/trailing-`/` would make the stored string differ from what env.rs
        // materializes (link pass vs GC pass would disagree → link-then-GC).
        for bad in ["a/.", "a/./b", "x//y", "a/", "./a", "."] {
            assert!(
                validate_share_path(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_share_path_rejects_leading_dash_segments() {
        for bad in ["-rf", "--force", "config/--force"] {
            assert!(
                validate_share_path(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_share_path_rejects_reserved_dir_first_segment() {
        for bad in [".git/config", ".trunk/x", ".pool/y"] {
            assert!(
                validate_share_path(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        // The reserved check is on the first SEGMENT as a dir, not dotfile leaves.
        assert!(validate_share_path(".env").is_ok());
        assert!(
            validate_share_path("sub/.git").is_ok(),
            ".git as a non-first segment is fine"
        );
    }

    #[test]
    fn list_shares_parses_symlink_directive() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();
        add_share(&path, "o/r", "symlink", &["tsconfig.base.json", ".env"]).unwrap();

        assert_eq!(
            list_shares(&path, "o/r").unwrap(),
            vec![
                Share {
                    path: ".env".into(),
                    mode: ShareMode::Symlink
                },
                Share {
                    path: "tsconfig.base.json".into(),
                    mode: ShareMode::Symlink
                },
            ],
            "sorted, both parsed from the `_.symlink` dotted key"
        );
    }

    #[test]
    fn list_shares_parses_copy_directive_and_mixes_modes() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();
        add_share(&path, "o/r", "symlink", &[".env"]).unwrap();
        add_share(&path, "o/r", "copy", &["seed.toml"]).unwrap();

        assert_eq!(
            list_shares(&path, "o/r").unwrap(),
            vec![
                Share {
                    path: ".env".into(),
                    mode: ShareMode::Symlink
                },
                Share {
                    path: "seed.toml".into(),
                    mode: ShareMode::Copy
                },
            ],
            "both directives parsed, each with its mode"
        );
    }

    #[test]
    fn list_shares_resolves_a_dual_declared_path_to_symlink() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();
        // The same path declared in both directives (hand-edit / git-sync) — symlink wins.
        add_share(&path, "o/r", "symlink", &[".env"]).unwrap();
        add_share(&path, "o/r", "copy", &[".env"]).unwrap();

        assert_eq!(
            list_shares(&path, "o/r").unwrap(),
            vec![Share {
                path: ".env".into(),
                mode: ShareMode::Symlink
            }],
            "a path in both `_.symlink` and `_.copy` resolves to a single symlink share"
        );
    }

    #[test]
    fn list_shares_skips_unsafe_hand_edited_paths() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        // Hand-edit a traversal share alongside a good one.
        std::fs::write(
            &path,
            "[roots.\"o/r\"]\nurl = \"u\"\n[roots.\"o/r\".env]\n_.symlink = [\"../../evil\", \".env\"]\n",
        )
        .unwrap();
        assert_eq!(
            list_shares(&path, "o/r").unwrap(),
            vec![Share {
                path: ".env".into(),
                mode: ShareMode::Symlink
            }],
            "traversal share filtered out"
        );
    }

    #[test]
    fn add_then_remove_share_roundtrips_and_preserves_siblings() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();
        add_worktree(&path, "o/r", "feat", "feature/x", None).unwrap();

        add_share(&path, "o/r", "symlink", &[".env", "tsconfig.base.json"]).unwrap();
        // Re-adding is idempotent (merge + dedup).
        add_share(&path, "o/r", "symlink", &[".env"]).unwrap();
        assert_eq!(list_shares(&path, "o/r").unwrap().len(), 2);

        remove_share(&path, "o/r", "symlink", ".env").unwrap();
        assert_eq!(
            list_shares(&path, "o/r").unwrap(),
            vec![Share {
                path: "tsconfig.base.json".into(),
                mode: ShareMode::Symlink
            }]
        );
        // The unrelated worktree + url survived the env edits.
        assert_eq!(list(&path).unwrap()[0].url, "u");
        assert_eq!(list_worktrees(&path, "o/r").unwrap().len(), 1);
    }

    #[test]
    fn add_share_rejects_unsafe_path() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();
        assert!(add_share(&path, "o/r", "symlink", &["../escape"]).is_err());
        assert!(
            list_shares(&path, "o/r").unwrap().is_empty(),
            "nothing written"
        );
    }

    // --- pool (`[pool] size`) ----------------------------------------------

    #[test]
    fn pool_size_defaults_to_zero_when_absent() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();
        assert_eq!(pool_size(&path, "o/r").unwrap(), 0, "opt-in default");
        assert_eq!(pool_size(&path, "missing").unwrap(), 0, "absent root → 0");
    }

    #[test]
    fn set_pool_size_roundtrips_and_preserves_siblings() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        add_root(&path, "o/r", "u").unwrap();
        add_worktree(&path, "o/r", "feat", "feature/x", None).unwrap();

        set_pool_size(&path, "o/r", 3).unwrap();
        assert_eq!(pool_size(&path, "o/r").unwrap(), 3);

        // Overwrite in place; the table form renders, the worktree/url survive.
        set_pool_size(&path, "o/r", 1).unwrap();
        assert_eq!(pool_size(&path, "o/r").unwrap(), 1);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[roots.\"o/r\".pool]"), "table form: {text}");
        assert_eq!(list(&path).unwrap()[0].url, "u");
        assert_eq!(list_worktrees(&path, "o/r").unwrap().len(), 1);
    }

    #[test]
    fn set_pool_size_requires_a_declared_root() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        assert!(set_pool_size(&path, "o/r", 2).is_err(), "no root");
    }

    /// Carried law 6's globally-serialized manifest must fail *open* after the writer
    /// is done. The lock lives on the open file description, so a child spawned while
    /// it is held keeps holding it — and the daemon holds this lock in the same process
    /// it spawns git from. Without `O_CLOEXEC` the second acquisition below blocks
    /// until `/bin/sleep` exits, which in the real failure is a long-lived ssh
    /// `ControlPersist` or credential helper: the manifest wedges permanently, process-
    /// wide and cross-process, with no diagnostic.
    #[test]
    fn the_manifest_lock_does_not_leak_into_a_spawned_child() {
        use std::sync::mpsc;
        use std::time::Duration;

        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);

        let mut child = with_lock(&path, || {
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .context("spawn the fd-inheriting child")
        })
        .unwrap();

        // The lock is released; only the child could still be holding it. Off-thread
        // with a deadline, because the failure mode is an unbounded block, not an error.
        let (tx, rx) = mpsc::channel();
        let probe = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(with_lock(&probe, || Ok(())));
        });
        let reacquired = rx.recv_timeout(Duration::from_secs(10));

        let _ = child.kill();
        let _ = child.wait();
        assert!(
            reacquired.is_ok_and(|r| r.is_ok()),
            "the lock is still held by the spawned child — the open lacks O_CLOEXEC"
        );
    }

    #[test]
    fn pool_size_reads_zero_for_a_malformed_hand_edit() {
        let tmp = TempDir::new().unwrap();
        let path = manifest(&tmp);
        std::fs::write(
            &path,
            "[roots.\"o/r\"]\nurl = \"u\"\n[roots.\"o/r\".pool]\nsize = -4\n",
        )
        .unwrap();
        assert_eq!(
            pool_size(&path, "o/r").unwrap(),
            0,
            "negative → 0, not a panic"
        );
    }
}
