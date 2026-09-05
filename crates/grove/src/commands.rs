//! The commands that touch state, and the single-realizer gate every one of them
//! takes.
//!
//! **Carried law 2.** Declaration is universal — the manifest is written by whoever
//! runs the command. Realization is not: it belongs to the daemon whenever one is
//! reachable, and only to the CLI when none is.
//!
//! ```text
//!            Up                     Busy                    Offline
//! add     declare + nudge        declare only           realize in-process
//! remove  delegate               REFUSE (exit 4)        remove in-process
//! doctor  POST /api/doctor       error (exit 4)         run in-process
//! sync    POST /api/roots/sync   REFUSE (exit 4)        sync in-process
//! ```
//!
//! `Busy` is the subtle arm and the reason [`Reachability`] is a tri-state rather than
//! a boolean. A daemon that accepts the connection but does not answer in time is
//! still a daemon with a realizer: declaring and stopping lets its watcher pick the
//! change up, while realizing in-process would race it into a dual clone. **Carried
//! law 3** is the destructive half of the same argument — a remove has no
//! declare-only analogue (deleting in-process races the realizer; undeclaring alone
//! leaves the bare for `adopt` to resurrect), so `Busy` refuses outright.
//!
//! Every function here takes `home` and an [`ApiClient`] as arguments rather than
//! reading the environment, so the gate is testable at *this* altitude — which is
//! where the v1 review found the acceptance missing.

use std::path::Path;

use crate::api::{ApiClient, Reachability};
use crate::{CliError, render};

/// `grove clone add <url>` — declare the repo, then realize it only if no daemon is
/// reachable.
// stele:landmark single-realizer
pub fn clone_add(home: &Path, api: &ApiClient, url: &str) -> Result<(), CliError> {
    let slug = grove_ops::roots::slug_from_url(url).map_err(io)?;
    let manifest = grove_ops::roots::manifest_path(home);

    match api.reachable() {
        // Declare, then trigger the daemon to realize it now — a reliable nudge, not
        // a hope that its fs watcher notices the write (which races at startup).
        Reachability::Up => {
            grove_ops::manifest::add_root(&manifest, &slug, url).map_err(io)?;
            api.reconcile()?;
            println!("declared {slug}; grove server is realizing it");
            Ok(())
        }
        Reachability::Busy => {
            grove_ops::manifest::add_root(&manifest, &slug, url).map_err(io)?;
            println!("declared {slug}; grove server is busy and will realize it shortly");
            Ok(())
        }
        // Offline: declare and realize just this root — clone only the one new bare.
        Reachability::Offline => {
            let applied = grove_ops::roots::add(home, &slug, url)?;
            render::applied(&applied);
            render::fail_if_any_failed(&applied)
        }
    }
}

/// `grove clone remove <slug> [--force]` — undeclare and delete the on-disk repo.
///
/// Guarded unless `force`: the root is surveyed for uncommitted tracked changes and
/// unpushed commits across every worktree under it, and a find is a refusal naming
/// them. `grove tree remove` already refuses a single dirty checkout (git's own
/// guard), and the command whose blast radius is N of them must not protect less.
// stele:landmark busy-refuses-destructive
pub fn clone_remove(home: &Path, api: &ApiClient, slug: &str, force: bool) -> Result<(), CliError> {
    match api.reachable() {
        Reachability::Up => {
            api.remove_root(slug, force)?;
            println!("removing {slug}; grove server is deleting it");
            Ok(())
        }
        Reachability::Busy => Err(CliError::Daemon(
            "grove server is busy; retry `grove clone remove` in a moment".into(),
        )),
        Reachability::Offline => {
            grove_ops::roots::remove(home, slug, removal(force))?;
            println!("removed {slug}");
            Ok(())
        }
    }
}

const fn removal(force: bool) -> grove_ops::roots::Removal {
    if force {
        grove_ops::roots::Removal::Forced
    } else {
        grove_ops::roots::Removal::Guarded
    }
}

/// `grove tree add <slug> <branch> [--base]` — the same gate as `clone add`.
///
/// The worktree's directory name is [`grove_ops::worktrees::name_for`] of the branch:
/// every checkout under a root, the trunk included, is a direct sibling (carried law
/// 7), so a `feature/x` checkout cannot nest a directory the layout does not allow.
///
/// Without `--base` the new branch starts at the **trunk** — the branch this root
/// integrates on, which is what a reader assumes "off the top of the repo" means and
/// is not the same as git's `HEAD` while a `trunk` edit is still converging. It is
/// resolved here rather than left to `worktrees::create`, so all three arms declare
/// the same base and the line printed names it. A root that isn't on disk yet
/// resolves to nothing, and the base stays unset for the daemon to fill.
pub fn tree_add(
    home: &Path,
    api: &ApiClient,
    slug: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<(), CliError> {
    let name = grove_ops::worktrees::name_for(branch);
    let base = base.map(str::to_owned).or_else(|| {
        grove_ops::roots::trunk(home, slug)
            .ok()
            .map(|trunk| trunk.branch)
    });
    let from = base
        .as_deref()
        .map_or_else(String::new, |base| format!(" based on {base}"));
    let declare = || {
        grove_ops::manifest::add_worktree(
            &grove_ops::roots::manifest_path(home),
            slug,
            &name,
            branch,
            base.as_deref(),
        )
        .map_err(io)
    };

    match api.reachable() {
        Reachability::Up => {
            declare()?;
            api.reconcile()?;
            println!("declared worktree {name}{from}; grove server is creating it");
        }
        Reachability::Busy => {
            declare()?;
            println!(
                "declared worktree {name}{from}; grove server is busy and will create it shortly"
            );
        }
        Reachability::Offline => {
            grove_ops::worktrees::create(home, slug, &name, branch, base.as_deref())?;
            println!("created worktree {name} ({branch}){from}");
        }
    }
    Ok(())
}

/// `grove tree remove <slug> <name>` — the same gate as `clone remove`.
pub fn tree_remove(home: &Path, api: &ApiClient, slug: &str, name: &str) -> Result<(), CliError> {
    match api.reachable() {
        Reachability::Up => {
            api.remove_tree(slug, name)?;
            println!("removing worktree {name}; grove server is deleting it");
            Ok(())
        }
        Reachability::Busy => Err(CliError::Daemon(
            "grove server is busy; retry `grove tree remove` in a moment".into(),
        )),
        Reachability::Offline => {
            grove_ops::worktrees::remove(home, slug, name)?;
            println!("removed worktree {name}");
            Ok(())
        }
    }
}

/// `grove tree list <slug>` — declared ⋈ actual worktrees of a root.
///
/// **Resolved in v2:** v1's `tree list` was the one command that never probed, always
/// reading the disk directly. That made it the only place a user could not see what
/// the daemon knew — a root mid-clone listed as if it were simply empty, and the pool
/// level and engine status (neither derivable from disk) were invisible. It now asks
/// the daemon first, and the ask is **best-effort in every direction**: this is a
/// read, so unlike the mutating commands there is no realizer to race, nothing to
/// refuse, and — the part v2's first cut got wrong — nothing that may turn an
/// unrelated listener on the bind into a failed listing.
///
/// So the snapshot decides only the header, never whether an answer is given:
///
/// - a decodable row → print its status and pool level, which no disk read can
///   recover;
/// - a row whose status is `unavailable` → the daemon is telling us its own reads did
///   not land inside their budget ([`grove_daemon`]'s `stream::view`), so its empty
///   worktree list is *no answer*, not an empty root. Fall through to the disk.
/// - no row, no envelope, a 503 from the readiness gate, a timeout, nothing
///   listening → fall through to the disk, exactly as v1 always did.
///
/// The failure mode being closed is the quiet one: rendering an `unavailable` row's
/// empty list printed `no worktrees for <slug>` at exit 0 over a root whose worktrees
/// were sitting on disk the whole time.
pub fn tree_list(home: &Path, api: &ApiClient, slug: &str) -> Result<(), CliError> {
    for line in listing(home, api, slug)? {
        println!("{line}");
    }
    Ok(())
}

/// The lines [`tree_list`] prints, built rather than printed so the choice of source
/// is assertable: which rows an operator sees is the whole behaviour here, and a
/// regression that silently renders the daemon's empty list is invisible to a test
/// that only checks the call returned `Ok`.
fn listing(home: &Path, api: &ApiClient, slug: &str) -> Result<Vec<String>, CliError> {
    let mut lines = Vec::new();
    if let Some(root) = snapshot_row(api, slug) {
        lines.push(format!(
            "root {}: {} — pool {}/{}",
            root.slug,
            root.status.as_str(),
            root.pool.observed,
            root.pool.target
        ));
        // Under the header, before the checkouts: a degraded root's reason is the
        // whole reason to read this listing, and the daemon is the only place it
        // exists — nothing on disk records why a reconcile refused.
        if let Some(error) = &root.error {
            lines.push(format!("  {error}"));
        }
        if root.status != grove_api::RootStatus::Unavailable {
            // The snapshot's own answer for which branch this root integrates on. A
            // daemon that predates `trunk_branch` sends nothing, and a row named
            // after an empty branch would say less than no row at all.
            let name = grove_ops::worktrees::name_for(&root.trunk_branch);
            lines.extend(checkouts(
                (!name.is_empty()).then_some((name.as_str(), root.trunk_branch.as_str())),
                root.worktrees.iter().map(|w| Checkout {
                    name: &w.name,
                    branch: &w.branch,
                    present: w.present,
                    declared: w.declared,
                }),
            ));
            if root.worktrees.is_empty() {
                lines.push(format!("no worktrees for {slug}"));
            }
            return Ok(lines);
        }
    }

    let worktrees = grove_ops::worktrees::list(home, slug)?;
    // Gated on the bare being there, and best-effort past it: `git::default_branch`
    // answers `main` for a bare it cannot read — right for a fresh bare that has no
    // HEAD yet, but it would also invent a trunk row for a root with nothing on disk
    // at all.
    let trunk = grove_ops::roots::bare_dir(home, slug)
        .is_dir()
        .then(|| grove_ops::roots::trunk(home, slug).ok())
        .flatten();
    lines.extend(checkouts(
        trunk.as_ref().map(|t| (t.name.as_str(), t.branch.as_str())),
        worktrees.iter().map(|w| Checkout {
            name: &w.name,
            branch: &w.branch,
            present: w.present,
            declared: w.declared,
        }),
    ));
    if worktrees.is_empty() {
        lines.push(format!("no worktrees for {slug}"));
    }
    Ok(lines)
}

/// This root's row from `GET /api/roots`, or `None` for every way that can fail to
/// arrive.
///
/// The probe stays in front of the snapshot here for a budget reason rather than a
/// realizer one: [`timeouts::SNAPSHOT`] is five times [`timeouts::PROBE`], so asking
/// a stalled daemon directly would make the disk fallback wait out the longer of the
/// two on exactly the daemon that has nothing to add. `Busy` and `Offline` are both
/// simply "no row" — nothing to refuse, because this is a read.
///
/// Past the probe, everything is best-effort. A transport failure is silent — the
/// disk read below is the whole answer. A daemon that *did* answer and could not be
/// understood says so on stderr: that is a real fault (something else holding
/// `GROVE_BIND`, or a version skew), and swallowing it entirely would leave the
/// operator wondering why the status header vanished.
///
/// [`timeouts::PROBE`]: crate::timeouts::PROBE
/// [`timeouts::SNAPSHOT`]: crate::timeouts::SNAPSHOT
fn snapshot_row(api: &ApiClient, slug: &str) -> Option<grove_api::routes::RootView> {
    if api.reachable() != Reachability::Up {
        return None;
    }
    match api.snapshot() {
        Ok(snapshot) => snapshot.roots.into_iter().find(|root| root.slug == slug),
        Err(CliError::Daemon(_)) => None,
        Err(e) => {
            eprintln!("grove: {e}; listing {slug} from disk instead");
            None
        }
    }
}

/// One checkout as a listing row reads it — the two sources' rows ([`grove_api`]'s
/// `WorktreeView` and `grove_ops`' `WorktreeStatus`) narrowed to what a line says, so
/// both arms render through the same code rather than through two `format!`s that
/// drift apart.
struct Checkout<'a> {
    name: &'a str,
    branch: &'a str,
    present: bool,
    declared: bool,
}

/// A root's checkouts as lines, the trunk first: the one an operator opens most, then
/// the worktrees kept beside it.
///
/// `trunk` is that checkout's `(name, branch)`, or `None` when the source could not
/// say. It is rendered from the resolved trunk rather than found among `worktrees`
/// because neither source lists it: the trunk is the checkout the root owns, and
/// adopting it into `worktrees.<name>` is what would let a `tree remove` delete it.
/// The marker is what names it at all — since a trunk is named by its branch like
/// every other checkout, nothing in the row says which one is grove's own. A trunk
/// that *is* in the list anyway (a root mid-migration, whose manifest still declares
/// a worktree on the branch grove integrates on) is marked in place, never printed a
/// second time.
fn checkouts<'a>(
    trunk: Option<(&str, &str)>,
    worktrees: impl Iterator<Item = Checkout<'a>>,
) -> Vec<String> {
    let mut listed = false;
    let mut rows: Vec<String> = worktrees
        .map(|w| {
            let marker = if trunk.is_some_and(|(name, _)| name == w.name) {
                listed = true;
                " (trunk)"
            } else {
                ""
            };
            format!(
                "{}  {}{}{marker}",
                w.name,
                w.branch,
                note(w.present, w.declared)
            )
        })
        .collect();
    if let Some((name, branch)) = trunk.filter(|_| !listed) {
        rows.insert(0, format!("{name}  {branch} (trunk)"));
    }
    rows
}

/// The drift a listing flags: in git but undeclared, or declared but unrealized.
const fn note(present: bool, declared: bool) -> &'static str {
    match (present, declared) {
        (true, false) => " (undeclared)",
        (false, true) => " (missing)",
        _ => "",
    }
}

/// `grove sync <slug>` — fetch the root and fast-forward its trunk checkout.
///
/// The three-way gate is doctor's, not `clone add`'s, and the difference is worth
/// stating: a sync is a **git write on the root** (fetch, fast-forward, prune stranded
/// warm slots), so there is nothing to merely declare. Either the realizer that owns
/// this root's lane does it, or — with no realizer — this process does.
///
/// - `Up` → POST and print the ack. The daemon's sync is accept-only, so there is no
///   report to render here; what it did is read from `grove tree list` (the snapshot
///   carries `syncing` and the resulting `sync_note`) or from a UI on
///   `GET /api/events`. Printing a report the CLI did not observe would be a fiction.
/// - `Busy` → refuse, exit 4. Carried law 3's argument, applied to a writer rather
///   than a destroyer: a daemon that connects but does not answer is still the
///   realizer, and fetching in-process beside it puts two writers in one root's git.
///   Unlike an add there is no declare-only half to fall back on — a sync is not a
///   declaration — so the honest answer is "not now".
/// - `Offline` → run `roots::sync` here and print the [`SyncReport`] the daemon's arm
///   cannot give: fetched, the trunk's outcome, the tip, and what the prune recycled.
///
/// **Exit 0 on a `diverged` or `dirty` trunk.** Carried law 9 makes those a *report*,
/// not a failure — the sync did exactly what it promised, which is to fetch and never
/// force. Exiting non-zero would also make the two realizing arms disagree about the
/// same state, since the `Up` arm cannot see the outcome at all.
///
/// [`SyncReport`]: grove_ops::roots::SyncReport
pub fn sync(home: &Path, api: &ApiClient, slug: &str) -> Result<(), CliError> {
    match api.reachable() {
        Reachability::Up => {
            api.sync(slug)?;
            println!("sync accepted for {slug}; grove server is fetching it");
            Ok(())
        }
        Reachability::Busy => Err(CliError::Daemon(
            "grove server is busy; retry `grove sync` in a moment".into(),
        )),
        Reachability::Offline => {
            let report = grove_ops::roots::sync(home, slug)?;
            render::sync_report(slug, &report);
            Ok(())
        }
    }
}

/// `grove apply` — realize the whole manifest **locally, always**.
///
/// The one command with no gate. It is the offline realizer by definition: adopt
/// undeclared on-disk bares, then reconcile every declared root — and an operator
/// reaching for `apply` is reaching for the local one.
///
/// **It is ungated, not safe.** Every other realizing command asks `GET /api/health`
/// who owns realization; this one does not, so with a daemon up it runs
/// `roots::reconcile_one` off any lane while that root's engine may be inside the same
/// call. `grove-ops` holds no per-root mutex (`lm:lane-is-callers`), so the two race
/// git's index and worktree locks. Carried as a hazard on this crate rather than
/// papered over: on a box with a daemon, the gated path to the same convergence is
/// `grove clone add` / `POST /api/roots/reconcile`.
// stele:landmark apply-is-ungated
pub fn apply(home: &Path) -> Result<(), CliError> {
    let applied = grove_ops::apply(home)?;
    render::applied(&applied);
    render::fail_if_any_failed(&applied)
}

/// `grove doctor [slug] [--dry-run] [--fix]`.
///
/// `Busy` errors rather than declaring or realizing: doctor needs a *synchronous
/// answer*, a busy daemon cannot give one, and converging in-process would race it.
pub fn doctor(
    home: &Path,
    api: &ApiClient,
    slug: Option<&str>,
    dry_run: bool,
    fix: bool,
) -> Result<(), CliError> {
    let data = match api.reachable() {
        Reachability::Up => api.doctor(slug, dry_run, fix)?,
        Reachability::Busy => {
            return Err(CliError::Daemon(
                "grove server is busy; retry `grove doctor` in a moment".into(),
            ));
        }
        Reachability::Offline => {
            let (report, pools) = grove_ops::doctor::run(home, slug, dry_run, fix)?;
            grove_api::DoctorData {
                report,
                pools,
                statuses: offline_statuses(home, slug),
                checks: grove_ops::doctor::checks(home, slug),
            }
        }
    };

    render::report(&data.report);
    render::pools(&data.pools);
    render::statuses(&data.statuses);
    render::checks(&data.checks);
    render::fail_on_unresolved(&data.report)
}

/// Offline there are no engines, so `cloning`/`degraded` are not observable — derive
/// `ready`/`missing` from disk per declared root (bare + trunk present ⇒ ready).
///
/// A manifest that will not read yields no statuses rather than an error: doctor's own
/// manifest check is what reports that, and blanking the whole report over it would
/// hide every other finding.
fn offline_statuses(home: &Path, slug: Option<&str>) -> Vec<grove_api::RootStatusEntry> {
    grove_ops::roots::list(home)
        .unwrap_or_default()
        .into_iter()
        .filter(|root| slug.is_none_or(|s| s == root.slug))
        .map(|root| {
            let status = if grove_ops::roots::bare_dir(home, &root.slug).is_dir()
                && grove_ops::roots::trunk_dir(home, &root.slug).is_dir()
            {
                grove_api::RootStatus::Ready
            } else {
                grove_api::RootStatus::Missing
            };
            grove_api::RootStatusEntry {
                slug: root.slug,
                status,
                // Nothing on disk records why a reconcile failed, and the two
                // statuses derivable here are never the one that carries a reason.
                error: None,
            }
        })
        .collect()
}

/// A manifest-layer failure. `grove-ops`' manifest and URL helpers are the two that
/// still hand back an untyped `anyhow` error, so there is no category to map — the
/// alternate spelling carries the whole context chain the way v1's `{e:#}` did.
fn io(e: impl std::fmt::Display) -> CliError {
    CliError::Api(format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::{clone_add, clone_remove, doctor, sync, tree_add, tree_list, tree_remove};
    use crate::api::ApiClient;
    use grove_ops::testfix;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::time::Duration;
    use tempfile::TempDir;

    const SLUG: &str = "o/r";

    /// A daemon that accepts the connection and never responds — the `Busy` shape.
    fn busy() -> ApiClient {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                std::thread::sleep(Duration::from_secs(30));
                drop(s);
            }
        });
        ApiClient::at(addr, Duration::from_millis(200))
    }

    /// A daemon answering every request with a 200 success envelope — enough for
    /// `reachable()` and for the remove routes to decode a success. Loops, because a
    /// remove dispatch probes health *then* POSTs.
    fn up() -> ApiClient {
        ApiClient::at(envelope_server(r#"{"ok":true,"data":{}}"#), BUDGET)
    }

    const BUDGET: Duration = Duration::from_secs(2);

    fn envelope_server(body: &'static str) -> SocketAddr {
        status_server("200 OK", body)
    }

    /// The same listener at an arbitrary status — a draining daemon answers the
    /// non-whitelisted routes 503 with an error envelope.
    fn status_server(status: &'static str, body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let _ = s.read(&mut [0u8; 4096]);
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        addr
    }

    /// Bind-then-drop: the port refuses connections → `Offline`.
    fn offline() -> ApiClient {
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        ApiClient::at(addr, Duration::from_millis(200))
    }

    fn manifest_of(home: &std::path::Path) -> String {
        std::fs::read_to_string(grove_ops::roots::manifest_path(home)).unwrap()
    }

    // ─── the declare-only arm (carried law 2) ────────────────────────────────

    /// The acceptance the v1 review found missing, at the dispatch altitude: a
    /// hanging daemon sends `clone add` down the declare-only arm — manifest written,
    /// NOTHING realized in-process. A regression collapsing `Busy` into `Offline`
    /// would clone here and fail this test.
    #[test]
    fn clone_add_busy_declares_only_never_realizes_in_process() {
        let home = TempDir::new().unwrap();
        clone_add(home.path(), &busy(), "https://github.com/o/r.git").unwrap();

        assert!(
            manifest_of(home.path()).contains(r#"[roots."o/r"]"#),
            "declared"
        );
        assert!(
            !home.path().join("code/o/r").exists(),
            "no in-process realization while the server is busy (dual-clone race)"
        );
    }

    #[test]
    fn tree_add_busy_declares_only_never_creates_in_process() {
        let home = TempDir::new().unwrap();
        grove_ops::manifest::add_root(
            &grove_ops::roots::manifest_path(home.path()),
            SLUG,
            "https://github.com/o/r.git",
        )
        .unwrap();

        tree_add(home.path(), &busy(), SLUG, "feature/x", Some("main")).unwrap();

        assert!(manifest_of(home.path()).contains("feature/x"), "declared");
        assert!(
            !home.path().join("code/o/r/feature-x").exists(),
            "no in-process create while the server is busy"
        );
    }

    /// The contrast case proving the dispatch actually branches: `Offline` (refused
    /// connection) DOES realize in-process.
    #[test]
    fn clone_add_offline_realizes_in_process() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        // The last two path segments are the slug `clone add` derives, so the source
        // has to *be* an `o/r` for the assertion below to name the right directory.
        let src = tmp.path().join("src/o/r");
        testfix::fixture_repo(&src);

        clone_add(&home, &offline(), src.to_str().unwrap()).unwrap();

        assert!(
            grove_ops::roots::bare_dir(&home, "o/r").is_dir(),
            "offline dispatch clones the declared root in-process"
        );
    }

    /// And `Up` declares without realizing: the daemon's own realizer does the work,
    /// which is the whole point of the nudge.
    #[test]
    fn clone_add_up_declares_and_nudges_without_realizing() {
        let home = TempDir::new().unwrap();
        clone_add(home.path(), &up(), "https://github.com/o/r.git").unwrap();

        assert!(manifest_of(home.path()).contains(r#"[roots."o/r"]"#));
        assert!(
            !home.path().join("code/o/r").exists(),
            "the server realizes it"
        );
    }

    // ─── the destructive arm (carried law 3) ─────────────────────────────────

    #[test]
    fn clone_remove_busy_refuses_and_deletes_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);

        let err = clone_remove(&home, &busy(), SLUG, false).unwrap_err();

        assert_eq!(err.exit_code(), 4, "Busy refuses with Daemon");
        assert!(
            grove_ops::roots::bare_dir(&home, "o/r").is_dir(),
            "a busy server must not delete on-disk state (would race the realizer)"
        );
        assert!(
            manifest_of(&home).contains(r#"[roots."o/r"]"#),
            "still declared"
        );
    }

    #[test]
    fn clone_remove_offline_removes_in_process() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);

        clone_remove(&home, &offline(), SLUG, false).unwrap();

        assert!(!home.join("code/o/r").exists(), "offline removes the root");
        assert!(
            !manifest_of(&home).contains(r#"[roots."o/r"]"#),
            "undeclared"
        );
    }

    #[test]
    fn clone_remove_up_delegates_without_touching_disk() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);

        clone_remove(&home, &up(), SLUG, false).unwrap();

        assert!(
            grove_ops::roots::bare_dir(&home, "o/r").is_dir(),
            "Up delegates to the server; the CLI must not delete in-process"
        );
        assert!(
            manifest_of(&home).contains(r#"[roots."o/r"]"#),
            "still declared (the server owns the undeclare)"
        );
    }

    #[test]
    fn tree_remove_busy_refuses_and_deletes_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root_and_worktree(&tmp);

        let err = tree_remove(&home, &busy(), SLUG, "feat").unwrap_err();

        assert_eq!(err.exit_code(), 4, "Busy refuses with Daemon");
        assert!(
            home.join("code/o/r/feat").is_dir(),
            "a busy server must not delete the worktree (would race the realizer)"
        );
    }

    #[test]
    fn tree_remove_offline_removes_in_process() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root_and_worktree(&tmp);

        tree_remove(&home, &offline(), SLUG, "feat").unwrap();

        assert!(!home.join("code/o/r/feat").exists(), "offline removes it");
        let worktrees = grove_ops::worktrees::list(&home, SLUG).unwrap();
        assert!(!worktrees.iter().any(|w| w.name == "feat"), "undeclared");
    }

    #[test]
    fn tree_remove_up_delegates_without_touching_disk() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root_and_worktree(&tmp);

        tree_remove(&home, &up(), SLUG, "feat").unwrap();

        assert!(
            home.join("code/o/r/feat").is_dir(),
            "Up delegates to the server; the CLI must not delete in-process"
        );
    }

    // ─── doctor ──────────────────────────────────────────────────────────────

    /// Doctor needs a synchronous answer and the daemon owns the mutation, so `Busy`
    /// is exit 4 — not a declare, not an in-process converge.
    #[test]
    fn doctor_busy_refuses() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);
        let err = doctor(&home, &busy(), Some(SLUG), true, false).unwrap_err();
        assert_eq!(err.exit_code(), 4);
    }

    /// Offline, doctor runs in-process and derives what it can from disk: a realized
    /// root reads `ready`, and the plumbing checks run — the pass v1 never shipped.
    #[test]
    fn doctor_offline_runs_in_process_with_disk_derived_statuses() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);

        doctor(&home, &offline(), Some(SLUG), true, false).unwrap();

        let statuses = super::offline_statuses(&home, Some(SLUG));
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].status, grove_api::RootStatus::Ready);

        // A declared root with nothing on disk is `missing`, the only other verdict
        // a filesystem can support.
        grove_ops::manifest::add_root(
            &grove_ops::roots::manifest_path(&home),
            "o/absent",
            "https://example.invalid/o/absent.git",
        )
        .unwrap();
        let statuses = super::offline_statuses(&home, Some("o/absent"));
        assert_eq!(statuses[0].status, grove_api::RootStatus::Missing);
    }

    // ─── sync ────────────────────────────────────────────────────────────────

    /// Move the source repo one commit ahead of everything cloned from it, so a sync
    /// has something to fast-forward onto. The fixture's `src` lives in the caller's
    /// own scratch, so committing into it touches nothing outside this test.
    fn commit_ahead(tmp: &TempDir, file: &str) {
        let src = tmp.path().join("src");
        std::fs::write(src.join(file), "ahead").unwrap();
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
        testfix::git(&src, &[&id[..], &["add", "."]].concat());
        testfix::git(&src, &[&id[..], &["commit", "-q", "-m", "ahead"]].concat());
    }

    /// Offline there is no realizer, so the CLI is one: the fetch and the
    /// fast-forward happen in this process and the trunk lands on the remote tip.
    ///
    /// `slow_` because it runs a real fetch over a real fixture clone.
    #[test]
    fn slow_sync_offline_fast_forwards_the_trunk_in_process() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);
        commit_ahead(&tmp, "AHEAD.md");
        let trunk = grove_ops::roots::trunk_dir(&home, SLUG);
        assert!(!trunk.join("AHEAD.md").exists(), "behind to begin with");

        sync(&home, &offline(), SLUG).unwrap();

        assert!(
            trunk.join("AHEAD.md").exists(),
            "the offline arm realizes the sync itself"
        );
    }

    /// A busy daemon is still this root's realizer. Refusing is the *safe* answer —
    /// fetching here would put a second writer in the same root's git while the
    /// daemon's own lane may be inside a reconcile — and unlike an add there is no
    /// declare-only half to fall back on, because a sync is not a declaration.
    #[test]
    fn sync_busy_refuses_and_fetches_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);
        commit_ahead(&tmp, "AHEAD.md");

        let err = sync(&home, &busy(), SLUG).unwrap_err();

        assert_eq!(err.exit_code(), 4, "Busy refuses with Daemon");
        assert!(
            !grove_ops::roots::trunk_dir(&home, SLUG)
                .join("AHEAD.md")
                .exists(),
            "a busy server must not be raced into the same root's git"
        );
    }

    /// `Up` delegates: the POST is the whole of the CLI's work, and the trunk is left
    /// exactly where it was for the daemon's own engine to move.
    #[test]
    fn sync_up_delegates_without_fetching_in_process() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);
        commit_ahead(&tmp, "AHEAD.md");
        let api = ApiClient::at(
            envelope_server(r#"{"ok":true,"data":{"sync":"accepted"}}"#),
            BUDGET,
        );

        sync(&home, &api, SLUG).unwrap();

        assert!(
            !grove_ops::roots::trunk_dir(&home, SLUG)
                .join("AHEAD.md")
                .exists(),
            "Up delegates; the server's engine owns the fetch"
        );
    }

    /// The daemon's 404 reaches the operator rather than being retried locally: an
    /// undeclared root has no sync, and falling back to the in-process arm on a
    /// refusal is exactly the dual-realizer the gate exists to prevent.
    #[test]
    fn sync_up_surfaces_an_undeclared_slug_rather_than_realizing() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);
        let api = ApiClient::at(
            status_server(
                "404 Not Found",
                r#"{"ok":false,"error":{"code":"not_found","message":"root not declared","data":{"slug":"no/such"}}}"#,
            ),
            BUDGET,
        );

        let err = sync(&home, &api, "no/such").unwrap_err();

        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("not_found"), "{err}");
    }

    /// Offline, an undeclared root is the ops layer's `not_ready` — exit 5, through
    /// the carried `grove_ops::Error → CliError` map, not a blanket exit 1.
    #[test]
    fn sync_offline_reports_a_root_that_is_not_ready() {
        let tmp = TempDir::new().unwrap();
        let err = sync(tmp.path(), &offline(), SLUG).unwrap_err();
        assert_eq!(err.exit_code(), 5);
    }

    // ─── tree list: the resolved inconsistency ───────────────────────────────

    /// A snapshot row with a real status: the engine status and pool level — neither
    /// of which any disk read can recover — reach the operator, the trunk leads the
    /// checkouts under its own branch's name, and the daemon's worktrees follow.
    #[test]
    fn tree_list_up_renders_the_daemon_snapshot() {
        let body = r#"{"ok":true,"data":{"roots":[{
            "slug":"o/r","url":"file:///src","status":"cloning",
            "pool":{"observed":0,"target":2},"syncing":false,
            "trunk":"/h/code/o/r/release-2","trunk_branch":"release/2","worktrees":[
              {"name":"feat","branch":"feature/x","declared":true,"present":false,
               "path":"/h/code/o/r/feat"}]}],"logs":[]}}"#;
        let api = ApiClient::at(envelope_server(body), BUDGET);
        let tmp = TempDir::new().unwrap();

        // The home is empty — a local read would error `not declared`, so reaching
        // these lines at all proves the snapshot arm ran.
        let lines = super::listing(tmp.path(), &api, SLUG).unwrap();
        assert_eq!(lines[0], "root o/r: cloning — pool 0/2");
        assert_eq!(lines[1], "release-2  release/2 (trunk)", "{lines:?}");
        assert!(lines[2].starts_with("feat  feature/x"), "{lines:?}");
    }

    /// A `degraded` root's reason is on the wire and nowhere else: reconcile refuses a
    /// legacy or occupied root without writing a thing, so the listing is the only
    /// place an operator learns which of those it is — and which command clears it.
    #[test]
    fn tree_list_prints_why_a_degraded_root_degraded() {
        let body = r#"{"ok":true,"data":{"roots":[{
            "slug":"o/r","url":"file:///src","status":"degraded",
            "error":"root is in the legacy layout (.git is a bare repo): run `grove doctor --fix` to migrate it in place",
            "pool":{"observed":0,"target":0},"syncing":false,
            "trunk":"/h/code/o/r/main","trunk_branch":"main","worktrees":[]}],"logs":[]}}"#;
        let api = ApiClient::at(envelope_server(body), BUDGET);
        let tmp = TempDir::new().unwrap();

        let lines = super::listing(tmp.path(), &api, SLUG).unwrap();

        assert_eq!(lines[0], "root o/r: degraded — pool 0/0", "{lines:?}");
        assert!(lines[1].contains("grove doctor --fix"), "{lines:?}");
    }

    /// The trunk is a checkout no worktree list carries — adopting it is what would
    /// let a `tree remove` delete grove's own — so `tree list` renders it from the
    /// resolved trunk, marked. Without the marker the row is a plain sibling: the
    /// trunk is named by its branch exactly as `feat` is, and an operator reading the
    /// list has nothing to tell them which checkout the root integrates on.
    #[test]
    fn tree_list_marks_the_trunk_row_read_from_disk() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root_and_worktree(&tmp);

        let lines = super::listing(&home, &offline(), SLUG).unwrap();

        assert_eq!(lines[0], "main  main (trunk)", "{lines:?}");
        assert!(lines[1].starts_with("feat  feature/x"), "{lines:?}");
    }

    /// Nothing on disk, nothing to call a trunk: `git::default_branch` answers `main`
    /// for a bare it cannot read — deliberate, for a fresh bare with no HEAD — so
    /// without the gate a root that was never cloned would list a trunk it does not
    /// have.
    #[test]
    fn a_root_with_no_bare_on_disk_gets_no_trunk_row() {
        let tmp = TempDir::new().unwrap();

        let lines = super::listing(tmp.path(), &offline(), SLUG).unwrap();

        assert_eq!(lines, vec![format!("no worktrees for {SLUG}")], "{lines:?}");
    }

    /// A root mid-migration can still declare a worktree on the branch grove
    /// integrates on. That row *is* the trunk, so it is marked where it stands —
    /// printing a second trunk row above it would claim two checkouts of one branch.
    #[test]
    fn a_trunk_already_in_the_list_is_marked_in_place_rather_than_repeated() {
        let body = r#"{"ok":true,"data":{"roots":[{
            "slug":"o/r","url":"file:///src","status":"ready",
            "pool":{"observed":1,"target":1},"syncing":false,
            "trunk":"/h/code/o/r/main","trunk_branch":"main","worktrees":[
              {"name":"main","branch":"main","declared":true,"present":true,
               "path":"/h/code/o/r/main"}]}],"logs":[]}}"#;
        let api = ApiClient::at(envelope_server(body), BUDGET);
        let tmp = TempDir::new().unwrap();

        let lines = super::listing(tmp.path(), &api, SLUG).unwrap();

        assert_eq!(lines[1], "main  main (trunk)", "{lines:?}");
        assert_eq!(
            lines.iter().filter(|l| l.contains("(trunk)")).count(),
            1,
            "{lines:?}"
        );
    }

    /// `Busy` and `Offline` fall back to the local read rather than refusing: a list
    /// is a read, and there is no realizer for it to race.
    #[test]
    fn tree_list_falls_back_to_the_local_read_when_no_daemon_answers() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root_and_worktree(&tmp);

        // The public entry point over the same path, so the printing wrapper is not
        // the one thing no test walks.
        tree_list(&home, &offline(), SLUG).unwrap();

        for api in [offline(), busy()] {
            let lines = super::listing(&home, &api, SLUG).unwrap();
            assert!(
                lines.iter().any(|l| l.starts_with("feat ")),
                "the on-disk worktree is the answer: {lines:?}"
            );
        }
    }

    /// **The blocker.** `unavailable` is the daemon's word for "this root's reads did
    /// not land inside their budget" — it ships an EMPTY worktree list on purpose,
    /// because "ready beside an empty list is a lie a client cannot detect". Rendering
    /// that list printed `no worktrees for o/r` at exit 0 over a root whose worktrees
    /// were on disk the whole time — a silent wrong answer, in the exact state
    /// (a busy root, mid-clone) an operator reaches for `tree list` to inspect.
    #[test]
    fn tree_list_reads_the_disk_when_the_daemon_marks_the_row_unavailable() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root_and_worktree(&tmp);
        let body = r#"{"ok":true,"data":{"roots":[{
            "slug":"o/r","url":"file:///src","status":"unavailable",
            "pool":{"observed":0,"target":0},"syncing":false,
            "trunk":"/h/code/o/r/main","worktrees":[]}],"logs":[]}}"#;
        let api = ApiClient::at(envelope_server(body), BUDGET);

        let lines = super::listing(&home, &api, SLUG).unwrap();

        assert_eq!(
            lines[0], "root o/r: unavailable — pool 0/0",
            "the header still carries what the probe was added for"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("feat ")),
            "the worktree on disk must still be listed: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("no worktrees")),
            "an unavailable row is no answer, never an empty one: {lines:?}"
        );
    }

    /// A read must never be the command that fails because something unrelated holds
    /// the bind, or because the daemon is draining. `Up` is *any* HTTP answer, so
    /// both shapes reach the decoder — and both fall through to the disk, which is
    /// where v1 always read from and always exited 0.
    #[test]
    fn tree_list_reads_the_disk_when_the_daemon_answer_does_not_decode() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root_and_worktree(&tmp);

        // A stranger on GROVE_BIND: a 200 that is not an envelope at all.
        let stranger = ApiClient::at(envelope_server("<html>hello</html>"), BUDGET);
        // And the daemon's own readiness gate: `/api/roots` is not whitelisted, so a
        // draining or degraded daemon answers 503 with an error envelope.
        let draining = ApiClient::at(
            status_server(
                "503 Service Unavailable",
                r#"{"ok":false,"error":{"code":"unavailable","message":"server stopping"}}"#,
            ),
            BUDGET,
        );

        for api in [stranger, draining] {
            let lines = super::listing(&home, &api, SLUG).unwrap();
            assert!(
                lines.iter().any(|l| l.starts_with("feat ")),
                "a read answers from the disk rather than failing: {lines:?}"
            );
        }
    }

    #[test]
    fn a_branch_folds_to_a_sibling_directory_name() {
        assert_eq!(grove_ops::worktrees::name_for("feature/x"), "feature-x");
        assert_eq!(grove_ops::worktrees::name_for("a/b/c"), "a-b-c");
        assert_eq!(grove_ops::worktrees::name_for("main"), "main");
    }
}
