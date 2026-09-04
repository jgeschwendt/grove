//! The phase-4 end-to-end scenario: one root's whole life through the real daemon.
//!
//! declare → nudge → clone → pool fill → promote → share materialized → sync →
//! remove, over HTTP where an HTTP route exists and through the engine handle where
//! one does not yet, with the event stream asserted along the way.
//!
//! This is the only test in the tree that runs the *assembled* daemon — listener,
//! guards, watcher, engine set, lanes and engines together — so what it pins is the
//! wiring rather than any one part's semantics. Those live in `tests/engine.rs` and
//! beside the modules themselves.
//!
//! Hermetic: an ephemeral loopback port, a `TempDir` home, and a local fixture repo
//! as the remote. `slow_` because it performs several real clones and checkouts.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use grove_api::events::{Event, TaskKind, TaskOutcome};
use grove_api::routes::{RemoveRootData, RemoveWorktreeData};
use grove_api::{Envelope, RootStatus};
use grove_daemon::{AppState, Config, Daemon, RootSet};
use grove_ops::clock::SystemClock;
use grove_ops::testfix;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::broadcast;

const SLUG: &str = "o/r";
const BUDGET: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(20);

struct Harness {
    addr: SocketAddr,
    state: AppState,
    client: reqwest::Client,
    events: broadcast::Receiver<Event>,
    home: PathBuf,
}

impl Harness {
    /// A ready daemon serving `home`, with its engine room running.
    ///
    /// The reconcile mailbox is deliberately **not** claimed: that is what tells
    /// `serve` to start the watcher and the engine set rather than hand the raw seam
    /// to a caller. The shutdown route is disarmed so the scenario cannot lose its
    /// own server, and the filesystem watch stays off — the nudge is the reliable
    /// path, and this test drives it explicitly.
    async fn start(home: PathBuf) -> Self {
        let config = Config::new(&home, SocketAddr::from(([127, 0, 0, 1], 0)))
            .unwrap()
            .with_shutdown_enabled(false);
        let daemon = Daemon::bind(config, Arc::new(SystemClock)).await.unwrap();
        let addr = daemon.local_addr().unwrap();
        let state = daemon.state().clone();
        // Subscribed before serving, so the watcher's boot announcement is not lost.
        let events = state.events.subscribe();
        state.boot.mark_ready();
        tokio::spawn(async move { daemon.serve().await });
        Self {
            addr,
            state,
            // No proxy: an `HTTP_PROXY` in the environment must not sit between a
            // test and its own loopback server.
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            events,
            home,
        }
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Envelope<T> {
        let response = self
            .client
            .post(format!("http://{}{path}", self.addr))
            .json(body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert!(status.is_success(), "{path} answered {status}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    /// The engine set, once `serve` has installed it.
    async fn engines(&self) -> &RootSet {
        until("the engine room to start", async || {
            self.state.engines.get()
        })
        .await
    }

    /// Wait for an event the predicate accepts.
    async fn await_event(&mut self, what: &str, accept: impl Fn(&Event) -> bool) {
        loop {
            let event = tokio::time::timeout(BUDGET, self.events.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
                .expect("the bus stayed live");
            if accept(&event) {
                return;
            }
        }
    }
}

async fn until<T>(what: &str, mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    for _ in 0..(BUDGET.as_millis() / POLL.as_millis()) {
        if let Some(value) = probe().await {
            return value;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("timed out waiting for {what}");
}

fn commit(repo: &Path, file: &str) {
    std::fs::write(repo.join(file), file).unwrap();
    let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
    testfix::git(repo, &[&id[..], &["add", "."]].concat());
    testfix::git(repo, &[&id[..], &["commit", "-q", "-m", file]].concat());
}

#[tokio::test]
async fn slow_a_root_is_declared_realized_promoted_synced_and_removed() {
    let tmp = TempDir::new().unwrap();

    // ── declare ─────────────────────────────────────────────────────────────────
    // What an offline `grove clone add` leaves behind: a manifest entry and nothing
    // on disk. One warm slot and one shared file, so the pool and the worktree
    // environment are part of the scenario rather than a separate one.
    let src = tmp.path().join("src");
    testfix::fixture_repo(&src);
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let manifest = grove_ops::roots::manifest_path(&home);
    grove_ops::manifest::add_root(&manifest, SLUG, src.to_str().unwrap()).unwrap();
    grove_ops::manifest::set_pool_size(&manifest, SLUG, 1).unwrap();
    grove_ops::manifest::add_share(&manifest, SLUG, "symlink", &[".env"]).unwrap();

    let mut harness = Harness::start(home.clone()).await;

    // ── nudge ───────────────────────────────────────────────────────────────────
    // The reliable path the CLI uses after declaring: the route acknowledges that
    // convergence was *scheduled*, never that it finished.
    let ack: Envelope<Value> = harness.post("/api/roots/reconcile", &json!({})).await;
    assert_eq!(
        ack.into_result().unwrap(),
        json!({"reconcile": "scheduled"})
    );

    // ── clone ───────────────────────────────────────────────────────────────────
    harness
        .await_event(
            "the root to be announced",
            |event| matches!(event, Event::RootsChanged { roots } if roots == &[SLUG.to_owned()]),
        )
        .await;
    harness
        .await_event("the reconcile to finish", |event| {
            matches!(
                event,
                Event::TaskFinished { kind: TaskKind::Reconcile, outcome: TaskOutcome::Ok, slug }
                    if slug == SLUG
            )
        })
        .await;
    assert!(grove_ops::roots::bare_dir(&home, SLUG).is_dir());
    let trunk = grove_ops::roots::trunk_dir(&home, SLUG);
    assert!(trunk.is_dir());

    let engine = until("an engine for the declared root", async || {
        harness.engines().await.engine(SLUG).await
    })
    .await;
    assert_eq!(engine.status().await.unwrap(), RootStatus::Ready);

    // ── warm the pool ───────────────────────────────────────────────────────────
    // Nobody asked for this: the declared target is read by the reconcile task and
    // the fill follows it, lowest priority, on the same lane.
    until("the pool to reach its declared target", async || {
        (grove_ops::worktrees::pool_count(&home, SLUG).unwrap() >= 1).then_some(())
    })
    .await;

    // ── promote, and the share it materializes ──────────────────────────────────
    // Declared then nudged, exactly as `grove tree add` does against a running
    // daemon: the reconcile that realizes the worktree is what claims the slot. The
    // mark inside the slot is how we know it MOVED rather than a cold checkout
    // landing at the same path.
    let slot_mark = grove_ops::roots::root_dir(&home, SLUG).join(".pool/slot-0/CLAIMED");
    std::fs::write(&slot_mark, "warm").unwrap();
    grove_ops::manifest::add_worktree(&manifest, SLUG, "feat", "feature/x", Some("main")).unwrap();
    let _: Envelope<Value> = harness.post("/api/roots/reconcile", &json!({})).await;

    // The share is the last step of a claim (attach → move → declare →
    // materialize), so waiting on it waits for the whole thing to land.
    let worktree = grove_ops::roots::root_dir(&home, SLUG).join("feat");
    let link = worktree.join(".env");
    until(
        "the declared worktree to be realized with its share",
        async || {
            std::fs::symlink_metadata(&link)
                .is_ok_and(|m| m.is_symlink())
                .then_some(())
        },
    )
    .await;
    assert!(worktree.join("README.md").is_file(), "the slot moved");
    assert!(
        worktree.join("CLAIMED").is_file(),
        "a warm slot was claimed rather than cold-created"
    );
    // …and the mark goes away, so it does not make the tree dirty for the
    // (deliberately non-forced) `worktrees/remove` at the end of the scenario.
    std::fs::remove_file(worktree.join("CLAIMED")).unwrap();
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new("../main/.env"),
        "…and points through the sibling trunk source"
    );

    // ── sync ────────────────────────────────────────────────────────────────────
    commit(&src, "REMOTE.md");
    engine.sync().await.unwrap();
    harness
        .await_event("the sync to finish", |event| {
            matches!(
                event,
                Event::TaskFinished {
                    kind: TaskKind::Sync,
                    ..
                }
            )
        })
        .await;
    assert!(
        trunk.join("REMOTE.md").is_file(),
        "the trunk fast-forwarded onto the new tip"
    );
    assert_eq!(engine.sync_info().await.unwrap().note, None);

    // ── undeclare the share, and watch the link be collected ────────────────────
    // The carve-out from "reconcile never deletes": a grove-created symlink holds no
    // data, so an undeclared share's link is GC'd on the next converge. It is also a
    // precondition for the remove below — git refuses to remove a worktree carrying
    // untracked files, and grove's own share link is untracked.
    grove_ops::manifest::remove_share(&manifest, SLUG, "symlink", ".env").unwrap();
    let _: Envelope<Value> = harness.post("/api/roots/reconcile", &json!({})).await;
    until("the orphaned share link to be collected", async || {
        std::fs::symlink_metadata(&link).is_err().then_some(())
    })
    .await;
    assert!(
        trunk.join(".env").is_file(),
        "the source in the trunk is a real file and is never collected"
    );

    // ── remove the worktree ─────────────────────────────────────────────────────
    let removed: Envelope<RemoveWorktreeData> = harness
        .post(
            "/api/worktrees/remove",
            &json!({"slug": SLUG, "name": "feat"}),
        )
        .await;
    assert_eq!(removed.into_result().unwrap().removed, "feat");
    assert!(!worktree.exists(), "the worktree is gone from disk");
    assert!(
        grove_ops::manifest::list_worktrees(&manifest, SLUG)
            .unwrap()
            .is_empty(),
        "…and undeclared, git-first so a later reconcile cannot re-adopt it"
    );

    // ── remove the root ─────────────────────────────────────────────────────────
    // Delete on disk, then undeclare — and the route nudges the watcher after
    // itself, so the engine set learns to stop driving a root that is no longer
    // declared.
    let removed: Envelope<RemoveRootData> = harness
        .post("/api/roots/remove", &json!({"slug": SLUG}))
        .await;
    assert_eq!(removed.into_result().unwrap().removed, SLUG);
    assert!(!grove_ops::roots::root_dir(&home, SLUG).exists());
    assert!(grove_ops::roots::list(&home).unwrap().is_empty());

    harness
        .await_event(
            "the empty set to be announced",
            |event| matches!(event, Event::RootsChanged { roots } if roots.is_empty()),
        )
        .await;
    until("the engine to be torn down", async || {
        harness
            .engines()
            .await
            .slugs()
            .await
            .is_empty()
            .then_some(())
    })
    .await;

    // …and the removed root is not resurrected by the reconcile pass that follows.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !grove_ops::roots::root_dir(&harness.home, SLUG).exists(),
        "delete-before-undeclare held: nothing re-cloned the root"
    );

    harness.state.shutdown.fire();
}
