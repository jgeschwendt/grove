//! The observable surface, over a real socket: `GET /api/events`, `GET /api/roots`,
//! and doctor's plumbing checks.
//!
//! What these pin that the unit tests cannot is the **wire**: that a frame arrives as
//! `event: <tag>` + `data: <json>` on a live HTTP connection, that a mutation through
//! the ordinary routes is observable on a stream a client opened beforehand, and that
//! the snapshot decodes into the `grove-api` types — the same bytes a UI decodes by
//! hand into its own structs.
//!
//! Each test binds `127.0.0.1:0` and takes its own `TempDir` home.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use grove_api::routes::{DoctorData, Snapshot};
use grove_api::{Envelope, RootStatus};
use grove_daemon::{AppState, Config, Daemon};
use grove_ops::clock::{Clock, SystemClock, TestClock};
use grove_ops::doctor::{CheckKind, CheckStatus};
use grove_ops::testfix;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tempfile::TempDir;

const BUDGET: Duration = Duration::from_secs(30);

struct Harness {
    addr: SocketAddr,
    state: AppState,
    client: reqwest::Client,
    home: PathBuf,
}

impl Harness {
    /// A ready daemon over `home` **with its engine room running** — the reconcile
    /// mailbox is left unclaimed, which is what tells `serve` to start the watcher and
    /// the engines. The shutdown route is disarmed unless a test asks for it.
    async fn start(home: &Path, shutdown: bool) -> Self {
        Self::start_with(home, shutdown, Arc::new(SystemClock)).await
    }

    /// The same, on a clock the caller drives — how a 30-second budget is exercised
    /// without a 30-second test.
    async fn start_with(home: &Path, shutdown: bool, clock: Arc<dyn Clock>) -> Self {
        let config = Config::new(home, SocketAddr::from(([127, 0, 0, 1], 0)))
            .unwrap()
            .with_shutdown_enabled(shutdown);
        let daemon = Daemon::bind(config, clock).await.unwrap();
        let addr = daemon.local_addr().unwrap();
        let state = daemon.state().clone();
        state.boot.mark_ready();
        tokio::spawn(async move { daemon.serve().await });
        Self {
            addr,
            state,
            // No proxy: an `HTTP_PROXY` in the environment must not sit between a
            // test and its own loopback server.
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            home: home.to_path_buf(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> T {
        let response = self.client.get(self.url(path)).send().await.unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        match response.json::<Envelope<T>>().await.unwrap() {
            Envelope::Ok(data) => data,
            Envelope::Err(e) => panic!("{path} answered an error envelope: {e}"),
        }
    }

    async fn post(&self, path: &str, body: &Value) -> Value {
        let response = self
            .client
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert!(status.is_success(), "{path} answered {status}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    /// Open `GET /api/events` and read it as SSE.
    async fn stream(&self) -> Frames {
        let response = self
            .client
            .get(self.url("/api/events"))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream",
            "the route must announce itself as a stream or no browser will read it"
        );
        Frames {
            response,
            buffer: String::new(),
        }
    }
}

/// An SSE reader: enough of the format to assert on it — `event:`/`data:` pairs and
/// the comment lines a heartbeat writes.
///
/// Reads the body chunk by chunk rather than as a `Stream`: the response never ends,
/// so `bytes()` would wait forever, and `chunk()` needs no extra machinery to say
/// "whatever has arrived so far".
struct Frames {
    response: reqwest::Response,
    buffer: String,
}

#[derive(Debug)]
struct Frame {
    event: String,
    data: Value,
}

impl Frames {
    /// The next `event:`-carrying frame, skipping comments.
    async fn next(&mut self) -> Frame {
        loop {
            while let Some(split) = self.buffer.find("\n\n") {
                let raw = self.buffer[..split].to_owned();
                self.buffer.drain(..split + 2);
                let mut event = None;
                let mut data = String::new();
                for line in raw.lines() {
                    if let Some(name) = line.strip_prefix("event:") {
                        event = Some(name.trim().to_owned());
                    } else if let Some(payload) = line.strip_prefix("data:") {
                        data.push_str(payload.trim());
                    }
                }
                if let Some(event) = event {
                    return Frame {
                        event,
                        data: serde_json::from_str(&data)
                            .unwrap_or_else(|e| panic!("frame data is JSON: {e}: {data}")),
                    };
                }
            }
            let chunk = tokio::time::timeout(BUDGET, self.response.chunk())
                .await
                .expect("a frame arrives inside the budget")
                .expect("the body reads")
                .expect("the stream stays open");
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// Read until a frame the predicate accepts, and return it.
    async fn until(&mut self, what: &str, accept: impl Fn(&Frame) -> bool) -> Frame {
        for _ in 0..200 {
            let frame = self.next().await;
            if accept(&frame) {
                return frame;
            }
        }
        panic!("never saw {what}");
    }

    /// The same, keeping everything read along the way — for an assertion about a
    /// *sequence* of frames rather than a single one, where the frames in between are
    /// the thing under test and [`until`](Self::until) would discard them.
    async fn collect_until(&mut self, what: &str, accept: impl Fn(&Frame) -> bool) -> Vec<Frame> {
        let mut seen = Vec::new();
        for _ in 0..200 {
            let frame = self.next().await;
            let done = accept(&frame);
            seen.push(frame);
            if done {
                return seen;
            }
        }
        panic!("never saw {what}");
    }
}

/// The whole live channel in one pass: the snapshot a client renders from, the
/// events a mutation through the ordinary routes produces, and the frame format
/// itself.
///
/// `slow_` because it clones a fixture root and drives a real reconcile.
#[tokio::test]
async fn slow_the_stream_opens_with_a_snapshot_and_follows_the_mutations() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root_and_worktree(&tmp);
    let daemon = Harness::start(&home, false).await;
    let mut frames = daemon.stream().await;

    // ── the snapshot ────────────────────────────────────────────────────────────
    let opening = frames.next().await;
    assert_eq!(opening.event, "snapshot", "the first frame renders the UI");
    assert_eq!(
        opening.data["event"], "snapshot",
        "the tag travels in the payload too, for an `onmessage` client"
    );
    let snapshot: Snapshot = serde_json::from_value(opening.data).unwrap();
    let root = snapshot
        .roots
        .iter()
        .find(|r| r.slug == testfix::SLUG)
        .expect("the declared root is in the snapshot");
    assert_eq!(
        root.url,
        grove_ops::roots::list(&home).unwrap()[0].url,
        "the declared url travels, so a UI can name the remote"
    );
    assert_eq!(
        root.trunk,
        grove_ops::roots::trunk_dir(&home, testfix::SLUG)
            .display()
            .to_string()
    );
    assert_eq!(
        root.trunk_branch, "main",
        "the branch travels beside the path: the checkout is named by it, and folding `/` to `-` is not reversible, so a UI cannot read it back off the path"
    );
    let worktree = root
        .worktrees
        .iter()
        .find(|w| w.name == "feat")
        .expect("the declared worktree travels with its root");
    assert!(worktree.declared && worktree.present);
    assert_eq!(worktree.branch, "feature/x");
    assert_eq!(
        worktree.path,
        testfix::root_dir(&home, testfix::SLUG)
            .join("feat")
            .display()
            .to_string(),
        "a UI opens a terminal at this path"
    );
    assert_eq!(
        worktree.status.as_ref().unwrap().branch.as_deref(),
        Some("feature/x"),
        "the checked-out branch travels beside the declared one"
    );

    // ── a nudge, and the convergence it announces ───────────────────────────────
    daemon.post("/api/roots/reconcile", &json!({})).await;
    frames
        .until("the declared set announced", |f| {
            f.event == "roots_changed" && f.data["roots"] == json!([testfix::SLUG])
        })
        .await;
    let started = frames
        .until("a reconcile to start", |f| f.event == "task_started")
        .await;
    assert_eq!(started.data["kind"], "reconcile");
    assert_eq!(started.data["slug"], testfix::SLUG);
    let finished = frames
        .until("that reconcile to finish", |f| f.event == "task_finished")
        .await;
    assert_eq!(finished.data["outcome"], "ok");

    // ── a remove, through the ordinary route, seen on the stream ────────────────
    // The route nudges the watcher after itself, so an attached UI learns the root
    // is gone without polling — the evented interplay, end to end.
    daemon
        .post("/api/roots/remove", &json!({"slug": testfix::SLUG}))
        .await;
    frames
        .until("the empty set announced", |f| {
            f.event == "roots_changed" && f.data["roots"] == json!([])
        })
        .await;
    assert!(!grove_ops::roots::root_dir(&daemon.home, testfix::SLUG).exists());

    daemon.state.shutdown.fire();
}

/// **The accept-only contract's other half.** `POST /api/roots/sync` answers
/// `{"sync":"accepted"}` and nothing else, so everything the operator actually wants to
/// know arrives here — which is what makes this the test that the route is honest.
///
/// A client attached *before* the request sees, in order: `root_sync_changed` on
/// accept (broadcast to every viewer, not only the caller that asked), the sync taking
/// the root's background slot, its completion, and a second `root_sync_changed` — after
/// which the snapshot the same connection opened with reports the sync settled and the
/// trunk sits on the commit that was fetched.
///
/// `slow_` because it drives a real fetch over a real fixture clone.
#[tokio::test]
async fn slow_a_sync_is_accepted_and_announced_on_the_stream() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root(&tmp);
    commit_ahead(&tmp);
    let daemon = Harness::start(&home, false).await;
    let mut frames = daemon.stream().await;
    assert_eq!(frames.next().await.event, "snapshot");

    let ack = daemon
        .post("/api/roots/sync", &json!({"slug": testfix::SLUG}))
        .await;
    assert_eq!(
        ack["data"],
        json!({"sync": "accepted"}),
        "the route's whole answer is the ack"
    );

    let seen = frames
        .collect_until("the sync to finish", |f| {
            f.event == "task_finished" && f.data["kind"] == "sync"
        })
        .await;
    let named = |event: &str| -> Vec<&Frame> {
        seen.iter()
            .filter(|f| f.event == event && f.data["slug"] == testfix::SLUG)
            .collect()
    };

    assert!(
        named("root_sync_changed").len() >= 2,
        "one announcement on accept and one on completion: {seen:?}"
    );
    assert!(
        named("task_started")
            .iter()
            .any(|f| f.data["kind"] == "sync"),
        "the sync takes the root's background slot: {seen:?}"
    );
    assert_eq!(
        seen.last().unwrap().data["outcome"],
        "ok",
        "{:?}",
        seen.last()
    );

    // …and the state those level-triggered frames told the client to re-read.
    let snapshot: Snapshot = daemon.get("/api/roots").await;
    let root = snapshot
        .roots
        .iter()
        .find(|r| r.slug == testfix::SLUG)
        .expect("the synced root is in the snapshot");
    assert!(!root.syncing, "the sync has settled");
    assert!(
        root.sync_note.is_none(),
        "a clean fast-forward leaves no note: {:?}",
        root.sync_note
    );
    assert!(
        grove_ops::roots::trunk_dir(&home, testfix::SLUG)
            .join("AHEAD.md")
            .exists(),
        "the trunk moved onto the fetched commit"
    );

    daemon.state.shutdown.fire();
}

/// Move the fixture's source repo one commit ahead of the clone taken from it, so a
/// sync has something to fast-forward onto.
fn commit_ahead(tmp: &TempDir) {
    let src = tmp.path().join("src");
    std::fs::write(src.join("AHEAD.md"), "ahead").unwrap();
    let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
    testfix::git(&src, &[&id[..], &["add", "."]].concat());
    testfix::git(&src, &[&id[..], &["commit", "-q", "-m", "ahead"]].concat());
}

/// `GET /api/roots` answers the same shape the stream opens with — one decoder for a
/// client that streams and a client that polls.
#[tokio::test]
async fn slow_the_roots_route_answers_the_snapshot_shape() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root_and_worktree(&tmp);
    let daemon = Harness::start(&home, false).await;

    let snapshot: Snapshot = daemon.get("/api/roots").await;

    assert_eq!(snapshot.roots.len(), 1);
    let root = &snapshot.roots[0];
    assert_eq!(root.slug, testfix::SLUG);
    assert!(!root.syncing && root.sync_note.is_none());
    assert_eq!(root.pool.target, 0, "nothing declared a warm pool");
    assert!(
        root.trunk_status.is_some(),
        "a realized root reports its trunk's own drift"
    );
    // The engine set may not have caught up with this fresh daemon yet; either way the
    // row is present, which is what a UI renders.
    assert!(
        matches!(
            root.status,
            RootStatus::Ready | RootStatus::Unknown | RootStatus::Cloning
        ),
        "{:?}",
        root.status
    );

    daemon.state.shutdown.fire();
}

/// The log tail: the ring travels in the snapshot, and a line recorded afterwards
/// arrives as its own frame on an already-open connection.
#[tokio::test]
async fn the_stream_carries_the_log_ring_and_the_lines_after_it() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::start(tmp.path(), false).await;
    daemon.state.logs.record(line("before the connection"));

    let mut frames = daemon.stream().await;
    let opening = frames.next().await;
    let snapshot: Snapshot = serde_json::from_value(opening.data).unwrap();
    assert!(
        snapshot
            .logs
            .iter()
            .any(|l| l.message == "before the connection"),
        "the ring is queryable through the snapshot"
    );

    daemon.state.logs.record(line("after the connection"));
    let frame = frames.until("the new line", |f| f.event == "log").await;
    assert_eq!(frame.data["message"], "after the connection");
    assert_eq!(frame.data["level"], "info");

    daemon.state.shutdown.fire();
}

/// An attached stream must not hold a drain open: `axum::serve` waits for in-flight
/// responses, and an SSE response never ends on its own.
#[tokio::test]
async fn an_open_stream_does_not_hold_the_drain_open() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::start(tmp.path(), true).await;
    let mut frames = daemon.stream().await;
    assert_eq!(frames.next().await.event, "snapshot");

    daemon.post("/api/daemon/shutdown", &json!({})).await;

    let closed = tokio::time::timeout(BUDGET, async {
        while tokio::net::TcpStream::connect(daemon.addr).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(closed.is_ok(), "the port never stopped accepting");
}

/// Doctor's second half, over HTTP: the plumbing checks v1 specified and never
/// shipped, with the share report still beside them.
#[tokio::test]
async fn slow_doctor_reports_the_plumbing_checks_beside_the_share_report() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root_and_worktree(&tmp);
    // The exact invisibility v1 shipped with: the bare stands, the checkout is gone.
    std::fs::remove_dir_all(grove_ops::roots::trunk_dir(&home, testfix::SLUG)).unwrap();
    let daemon = Harness::start(&home, false).await;

    let data: DoctorData = serde_json::from_value(
        daemon.post("/api/doctor", &json!({"dry_run": true})).await["data"].clone(),
    )
    .unwrap();

    let find = |kind: CheckKind| {
        data.checks
            .iter()
            .find(|c| c.check == kind)
            .unwrap_or_else(|| panic!("no {kind:?} check in {:?}", data.checks))
    };
    assert_eq!(find(CheckKind::Manifest).status, CheckStatus::Ok);
    assert_eq!(find(CheckKind::Bare).status, CheckStatus::Ok);
    assert_eq!(find(CheckKind::Trunk).status, CheckStatus::Missing);
    assert_eq!(
        find(CheckKind::Trunk).slug.as_deref(),
        Some(testfix::SLUG),
        "every per-root finding names its root"
    );
    assert!(
        data.checks.iter().all(|c| c.check != CheckKind::Root),
        "a root that answered contributes no `root` row: {:?}",
        data.checks
    );

    daemon.state.shutdown.fire();
}

/// A whole-home doctor over a root whose lane will not answer: the root is *named*
/// as unavailable, its engine status still travels, and the run still succeeds —
/// v1's "a slow root contributes nothing", made visible.
///
/// The budget is 30 s on the injected clock, so this costs milliseconds: the lane is
/// wedged, the fake clock is walked past the budget, and the route gives up on that
/// root exactly as it would after half a minute of a real one.
#[tokio::test]
async fn slow_a_wedged_root_is_reported_rather_than_failing_the_whole_run() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root(&tmp);
    let clock = Arc::new(TestClock::new());
    let daemon = Harness::start_with(&home, false, Arc::clone(&clock) as Arc<dyn Clock>).await;

    // The engine has to exist before the wedge, or the root would have no status to
    // report and the assertion below would pass for the wrong reason.
    for _ in 0..500 {
        match daemon.state.engines.get() {
            Some(engines) if !engines.slugs().await.is_empty() => break,
            _ => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }

    // Hold the root's lane with a job that never returns. Foreground, like doctor's
    // own — a queue in front of it is what a genuinely wedged root looks like.
    let lanes = Arc::clone(&daemon.state.lanes);
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let holder = tokio::spawn(async move {
        lanes
            .run(
                testfix::SLUG,
                grove_daemon::Priority::Foreground,
                move || {
                    let _ = held.blocking_recv();
                },
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let doctor = tokio::spawn({
        let client = daemon.client.clone();
        let url = daemon.url("/api/doctor");
        async move {
            client
                .post(url)
                .json(&json!({"dry_run": true}))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    clock.advance(Duration::from_secs(31));

    let body: Value = serde_json::from_str(&doctor.await.unwrap()).unwrap();
    assert_eq!(
        body["ok"], true,
        "a wedged root is not a failed run: {body}"
    );
    let data: DoctorData = serde_json::from_value(body["data"].clone()).unwrap();

    let wedged = data
        .checks
        .iter()
        .find(|c| c.check == CheckKind::Root)
        .unwrap_or_else(|| panic!("the wedged root is named: {:?}", data.checks));
    assert_eq!(wedged.status, CheckStatus::Unavailable);
    assert_eq!(wedged.slug.as_deref(), Some(testfix::SLUG));
    assert!(
        wedged.detail.as_ref().unwrap().contains("budget"),
        "{:?}",
        wedged.detail
    );
    assert!(
        data.checks
            .iter()
            .any(|c| c.check == CheckKind::Manifest && c.status == CheckStatus::Ok),
        "the manifest check answers off any lane, so a wedged root cannot hide it"
    );
    assert!(
        data.statuses.iter().any(|s| s.slug == testfix::SLUG),
        "v1 semantics: the skipped root's engine status still travels"
    );
    assert!(
        data.report.is_empty() && data.pools.is_empty(),
        "…and it contributes nothing else"
    );

    release.send(()).unwrap();
    holder.await.unwrap().unwrap();
    daemon.state.shutdown.fire();
}

fn line(message: &str) -> grove_api::LogLine {
    grove_api::LogLine {
        at_ms: 1_577_836_800_000,
        level: grove_api::LogLevel::Info,
        target: "grove_daemon::tests".into(),
        message: message.to_owned(),
        fields: Vec::new(),
    }
}
