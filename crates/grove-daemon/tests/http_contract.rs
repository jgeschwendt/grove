//! The phase gate: the v1 controller and plug assertions, replayed over real HTTP
//! against a real daemon.
//!
//! Every body is decoded through the `grove-api` types rather than as loose JSON —
//! that *is* the contract test. A field renamed on either side stops being a
//! successful decode, and the shapes these tests accept are the shapes the CLI will
//! accept in phase 5.
//!
//! Each test binds `127.0.0.1:0` and reads the port back, and each takes its own
//! `TempDir` home: nothing here shares a port or a home with anything, so the suite
//! is safe under nextest's per-test processes and safe beside a live grove.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use grove_api::routes::{
    DoctorData, HealthData, HealthStatus, ReconcileAck, ReconcileData, RemoveRootData,
    RemoveWorktreeData, ShutdownData, SyncData, VersionData,
};
use grove_api::{ApiError, BootStatus, Envelope, ErrorCode};
use grove_daemon::lane::{FOREGROUND_DEPTH, Priority};
use grove_daemon::{AppState, Config, Daemon};
use grove_ops::clock::SystemClock;
use grove_ops::doctor::{CheckKind, CheckStatus};
use grove_ops::testfix;
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// A daemon serving on an ephemeral loopback port, plus the handles a test needs to
/// drive it: its shared state (to mark degraded/stopping) and the reconcile mailbox
/// (to prove the nudge route reaches the phase-4 seam).
struct Harness {
    addr: SocketAddr,
    state: AppState,
    client: reqwest::Client,
    /// `None` for an [`engined`](Harness::engined) daemon: leaving the mailbox
    /// unclaimed is exactly what tells `serve` to start the watcher and the engines,
    /// so a test cannot have both the raw seam and a running engine room.
    nudges: Option<mpsc::Receiver<()>>,
}

impl Harness {
    /// A **ready** daemon over `home`, with the shutdown route disarmed so a test
    /// that exercises other routes cannot lose its server.
    ///
    /// Claims the reconcile mailbox, so it runs **no engine room** — the routes are
    /// exercised against a daemon with no moving parts behind them.
    async fn ready(home: &Path) -> Self {
        let harness = Self::start(config(home).with_shutdown_enabled(false)).await;
        harness.state.boot.mark_ready();
        harness
    }

    /// A ready daemon over `home` **with its engine room running** — the reconcile
    /// mailbox is left unclaimed, which is what starts the watcher and the engines.
    ///
    /// Only the routes that reach an engine need this. `POST /api/roots/sync` is the
    /// one whose success arm is unreachable without one, by design: with no engine
    /// driving the root there is nothing to record the request, and the route says so
    /// rather than acknowledging into the void.
    async fn engined(home: &Path) -> Self {
        let harness = Self::spawn(config(home).with_shutdown_enabled(false), false).await;
        harness.state.boot.mark_ready();
        harness
    }

    /// A daemon left in whatever state the caller puts it in — used for the boot
    /// states the readiness gate exists for.
    async fn start(config: Config) -> Self {
        Self::spawn(config, true).await
    }

    async fn spawn(config: Config, claim_nudges: bool) -> Self {
        let mut daemon = Daemon::bind(config, Arc::new(SystemClock))
            .await
            .expect("the daemon binds an ephemeral loopback port");
        let addr = daemon.local_addr().unwrap();
        let state = daemon.state().clone();
        let nudges = claim_nudges.then(|| daemon.take_nudges().expect("the mailbox is unclaimed"));
        tokio::spawn(async move { daemon.serve().await });
        Self {
            addr,
            state,
            // No proxy: a `HTTP_PROXY` in the environment must not sit between a
            // test and its own loopback server.
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            nudges,
        }
    }

    /// Whether a reconcile nudge reached the seam. Only meaningful on a harness that
    /// claimed the mailbox.
    fn nudged(&mut self) -> bool {
        self.nudges
            .as_mut()
            .expect("this harness claimed the reconcile mailbox")
            .try_recv()
            .is_ok()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).send().await.unwrap()
    }

    async fn post(&self, path: &str, body: &Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .unwrap()
    }

    /// A POST with no body at all — how the CLI calls the nudge and shutdown routes.
    async fn post_bare(&self, path: &str) -> reqwest::Response {
        self.client.post(self.url(path)).send().await.unwrap()
    }

    /// A POST carrying a raw body and no `Content-Type` — what a hand-rolled client
    /// (curl without `-H`, a shell script) sends.
    async fn post_unlabelled(&self, path: &str, body: &'static str) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .body(body)
            .send()
            .await
            .unwrap()
    }

    async fn post_with_origin(&self, path: &str, origin: &str) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .header("origin", origin)
            .send()
            .await
            .unwrap()
    }
}

fn config(home: &Path) -> Config {
    Config::new(home, SocketAddr::from(([127, 0, 0, 1], 0)))
        .expect("a port-0 loopback bind passes the gate")
}

/// Decode a response's body as a success envelope, asserting its status on the way.
async fn ok_body<T: DeserializeOwned>(response: reqwest::Response, status: StatusCode) -> T {
    assert_eq!(response.status(), status);
    match response.json::<Envelope<T>>().await.expect("an envelope") {
        Envelope::Ok(data) => data,
        Envelope::Err(e) => panic!("expected a success envelope, got {e}"),
    }
}

/// Decode a response's body as an error envelope, asserting its status.
async fn err_body(response: reqwest::Response, status: StatusCode) -> ApiError {
    assert_eq!(response.status(), status);
    match response
        .json::<Envelope<Value>>()
        .await
        .expect("an envelope")
    {
        Envelope::Err(error) => error,
        Envelope::Ok(data) => panic!("expected an error envelope, got {data}"),
    }
}

// ─── health ─────────────────────────────────────────────────────────────────────

/// v1 `health_controller_test.exs:5`.
#[tokio::test]
async fn health_reports_ready_with_a_version() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let data: HealthData = ok_body(daemon.get("/api/health").await, StatusCode::OK).await;

    assert_eq!(data.status, HealthStatus::Ready);
    assert!(!data.version.is_empty());
}

/// v1 `health_controller_test.exs:14` — the non-ready branch, with the state itself
/// in `error.data`.
#[tokio::test]
async fn health_503s_while_draining_with_the_stopping_status() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;
    daemon.state.boot.mark_stopping();

    let error = err_body(
        daemon.get("/api/health").await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;

    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_eq!(error.message, "server stopping");
    assert_eq!(error.data_as::<BootStatus>(), Some(BootStatus::Stopping));
}

/// A daemon that has not signalled readiness answers the same way — the gate is the
/// boot state, not a flag the route sets for itself.
#[tokio::test]
async fn health_503s_while_still_booting() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::start(config(tmp.path())).await;

    let error = err_body(
        daemon.get("/api/health").await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;

    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_eq!(error.data_as::<BootStatus>(), Some(BootStatus::Booting));
}

/// v1 `readiness_plug_test.exs:41-43` — health stays observable while degraded, and
/// the reason travels. This payload is what the self-update health gate rolls a bad
/// bundle back on, so a regression that dropped the degraded branch would be
/// invisible until an update stranded a box.
#[tokio::test]
async fn health_503s_while_degraded_with_the_reason() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;
    daemon.state.boot.mark_degraded("ops_incompatible");

    let error = err_body(
        daemon.get("/api/health").await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;

    assert_eq!(error.code, ErrorCode::Degraded);
    assert_eq!(
        error.data,
        Some(json!({"status": "degraded", "reason": "ops_incompatible"}))
    );
}

// ─── daemon ─────────────────────────────────────────────────────────────────────

/// v1 `daemon_controller_test.exs:5`.
#[tokio::test]
async fn version_reports_the_version_and_an_uptime() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let data: VersionData = ok_body(daemon.get("/api/daemon/version").await, StatusCode::OK).await;

    assert_eq!(data.version, env!("CARGO_PKG_VERSION"));
    // Only that it is a real reading: the clock seam's arithmetic is pinned by the
    // unit tests, which can move time without waiting for it.
    assert!(data.uptime_ms < 60_000);
}

/// v1 `daemon_controller_test.exs:15` — the acknowledgement, and the state flip that
/// precedes it. Shutdown is disarmed here, exactly as v1's test config disarmed it.
#[tokio::test]
async fn shutdown_acks_stopping_and_flips_the_state() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let data: ShutdownData = ok_body(
        daemon.post_bare("/api/daemon/shutdown").await,
        StatusCode::OK,
    )
    .await;

    assert_eq!(data, ShutdownData::STOPPING);
    assert_eq!(daemon.state.boot.status(), BootStatus::Stopping);
}

/// The armed route: the acknowledgement is written *and* the server then stops
/// accepting. No `process::exit` anywhere — the library drains its own accept loop,
/// which is what lets `grove serve` run a daemon in the foreground.
#[tokio::test]
async fn shutdown_drains_the_server_after_answering() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::start(config(tmp.path())).await;
    daemon.state.boot.mark_ready();

    let data: ShutdownData = ok_body(
        daemon.post_bare("/api/daemon/shutdown").await,
        StatusCode::OK,
    )
    .await;
    assert!(data.stopping);

    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while tokio::net::TcpStream::connect(daemon.addr).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(closed.is_ok(), "the port never stopped accepting");
}

// ─── the readiness gate ─────────────────────────────────────────────────────────

/// v1 `readiness_plug_test.exs:21` — a non-whitelisted route is 503'd while
/// draining, and shutdown stays reachable so a late or repeated stop is not itself
/// refused.
#[tokio::test]
async fn the_readiness_gate_503s_a_non_whitelisted_route_while_draining() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;
    daemon.state.boot.mark_stopping();

    let error = err_body(
        daemon.get("/api/daemon/version").await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_eq!(error.message, "server is draining");

    let acked: ShutdownData = ok_body(
        daemon.post_bare("/api/daemon/shutdown").await,
        StatusCode::OK,
    )
    .await;
    assert!(acked.stopping, "shutdown is whitelisted past the gate");
}

/// The degraded half of the same gate, with its own message.
#[tokio::test]
async fn the_readiness_gate_503s_a_non_whitelisted_route_while_degraded() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;
    daemon.state.boot.mark_degraded("ops_incompatible");

    for path in ["/api/daemon/version", "/api/roots/remove", "/api/doctor"] {
        let response = daemon.post(path, &json!({"slug": "o/r"})).await;
        let error = err_body(response, StatusCode::SERVICE_UNAVAILABLE).await;
        assert_eq!(error.message, "server is degraded", "{path}");
    }
    // Health is whitelisted, so the degrade stays observable behind the gate.
    assert_eq!(
        err_body(
            daemon.get("/api/health").await,
            StatusCode::SERVICE_UNAVAILABLE
        )
        .await
        .code,
        ErrorCode::Degraded
    );
}

/// The one boot state the gate does **not** refuse. v1's plug tested `:stopping` and
/// `{:degraded,_}` only, and its endpoint listened before the post-boot ready signal,
/// so a request in that window was served. Carried as written — and pinned here,
/// because it is the kind of asymmetry a rewrite silently "tidies up".
#[tokio::test]
async fn the_readiness_gate_does_not_refuse_a_still_booting_daemon() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::start(config(tmp.path())).await;
    assert_eq!(daemon.state.boot.status(), BootStatus::Booting);

    let data: VersionData = ok_body(daemon.get("/api/daemon/version").await, StatusCode::OK).await;

    assert!(!data.version.is_empty());
}

// ─── the mutation guard ─────────────────────────────────────────────────────────

/// v1 `mutation_guard_test.exs` — "a POST with no Origin (the CLI) passes". Dropping
/// this allowance breaks every mutation grove itself performs.
#[tokio::test]
async fn the_mutation_guard_allows_a_request_with_no_origin() {
    let tmp = TempDir::new().unwrap();
    let mut daemon = Harness::ready(tmp.path()).await;

    let data: ReconcileData = ok_body(
        daemon.post_bare("/api/roots/reconcile").await,
        StatusCode::OK,
    )
    .await;

    assert_eq!(data.reconcile, ReconcileAck::Scheduled);
    assert!(
        daemon.nudged(),
        "the nudge reaches the seam, not just the response"
    );
}

/// The local dashboard's origin passes; a foreign one — including one whose name has
/// been rebound to 127.0.0.1, which still sends its own host — does not.
#[tokio::test]
async fn the_mutation_guard_allows_loopback_and_refuses_a_foreign_origin() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    for origin in [
        "https://localhost:7777",
        "http://127.0.0.1:7777",
        "http://[::1]:7777",
    ] {
        let response = daemon
            .post_with_origin("/api/roots/reconcile", origin)
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{origin}");
    }

    for origin in ["https://evil.example", "http://evil.example:7777"] {
        let error = err_body(
            daemon
                .post_with_origin("/api/roots/reconcile", origin)
                .await,
            StatusCode::FORBIDDEN,
        )
        .await;
        assert_eq!(error.code, ErrorCode::Forbidden, "{origin}");
        assert_eq!(error.message, "cross-origin request refused");
    }
}

/// Safe methods are never gated by origin.
#[tokio::test]
async fn the_mutation_guard_never_gates_a_safe_method() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let response = daemon
        .client
        .get(daemon.url("/api/health"))
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

// ─── roots ──────────────────────────────────────────────────────────────────────

/// v1 `roots_controller_test.exs:47` — grove-ops' remove is idempotent and would
/// acknowledge a no-op, so an undeclared slug is a 404 with the slug in `error.data`.
#[tokio::test]
async fn roots_remove_404s_an_undeclared_slug() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let error = err_body(
        daemon
            .post("/api/roots/remove", &json!({"slug": "no/such-root"}))
            .await,
        StatusCode::NOT_FOUND,
    )
    .await;

    assert_eq!(error.code, ErrorCode::NotFound);
    assert_eq!(error.data, Some(json!({"slug": "no/such-root"})));
}

/// v1 `roots_controller_test.exs:34` — the declared root goes, on disk and in the
/// manifest, and the reconcile nudge that tears its engine down follows.
#[tokio::test]
async fn roots_remove_deletes_a_declared_root_from_disk() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root(&tmp);
    let mut daemon = Harness::ready(&home).await;
    let root = testfix::root_dir(&home, testfix::SLUG);
    assert!(root.is_dir(), "the fixture root is on disk to begin with");

    let data: RemoveRootData = ok_body(
        daemon
            .post("/api/roots/remove", &json!({"slug": testfix::SLUG}))
            .await,
        StatusCode::OK,
    )
    .await;

    assert_eq!(data.removed, testfix::SLUG);
    assert!(!root.exists(), "delete-on-disk before undeclare");
    assert!(grove_ops::roots::list(&home).unwrap().is_empty());
    assert!(
        daemon.nudged(),
        "a removed root's teardown is driven by the reconcile nudge"
    );
}

/// v1 `roots_controller_test.exs:49` — a body with no slug is a 422, not a 500.
#[tokio::test]
async fn roots_remove_422s_a_body_without_a_slug() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let error = err_body(
        daemon.post("/api/roots/remove", &json!({})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;

    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(error.message, "missing slug");
}

// ─── roots/sync ─────────────────────────────────────────────────────────────────

/// The accept, end to end: the ack is the contract's constant body, and the request
/// really did reach `roots::sync` — the trunk lands on the commit the source picked up
/// after the clone, which nothing but a completed fetch + fast-forward can produce.
///
/// Completion is *observed*, never returned (that is what accept-only means): the
/// assertion polls disk rather than reading anything out of the response.
/// `crates/grove-daemon/tests/observe.rs` pins the other observation channel — the
/// `root_sync_changed` frame on `GET /api/events`.
///
/// `slow_` because it drives a real fetch through a real engine room.
#[tokio::test]
async fn slow_roots_sync_accepts_and_the_engine_fast_forwards_the_trunk() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root(&tmp);
    commit_ahead(&tmp);
    let daemon = Harness::engined(&home).await;
    let trunk = grove_ops::roots::trunk_dir(&home, testfix::SLUG);
    assert!(!trunk.join("AHEAD.md").exists(), "behind to begin with");

    let data: SyncData = ok_body(
        daemon
            .post("/api/roots/sync", &json!({"slug": testfix::SLUG}))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(data, SyncData::ACCEPTED);

    let synced = tokio::time::timeout(Duration::from_secs(30), async {
        while !trunk.join("AHEAD.md").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(synced.is_ok(), "the accepted sync never reached the trunk");

    daemon.state.shutdown.fire();
}

/// The remove routes' rule, applied to sync: grove-ops would happily fetch nothing for
/// a slug nobody declared, so the route decides declaredness from the manifest first
/// and 404s with the slug in `error.data`.
#[tokio::test]
async fn roots_sync_404s_an_undeclared_slug() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let error = err_body(
        daemon
            .post("/api/roots/sync", &json!({"slug": "no/such-root"}))
            .await,
        StatusCode::NOT_FOUND,
    )
    .await;

    assert_eq!(error.code, ErrorCode::NotFound);
    assert_eq!(error.data, Some(json!({"slug": "no/such-root"})));
}

/// A **declared** root with nothing driving it is 503, not 200: this harness claims
/// the reconcile mailbox, so the daemon runs no engine room, and there is nothing to
/// record the request. Acknowledging here would be the one lie an accept-only contract
/// can tell — a client that watched `root_sync_changed` forever would never see one.
#[tokio::test]
async fn roots_sync_503s_when_no_engine_drives_the_root() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root(&tmp);
    let daemon = Harness::ready(&home).await;

    let error = err_body(
        daemon
            .post("/api/roots/sync", &json!({"slug": testfix::SLUG}))
            .await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;

    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("no engine"), "{}", error.message);
    assert_eq!(error.data, Some(json!({"slug": testfix::SLUG})));
}

/// A body with no slug is a 422, not a 500 and not a whole-home fetch.
#[tokio::test]
async fn roots_sync_422s_a_body_without_a_slug() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let error = err_body(
        daemon.post("/api/roots/sync", &json!({})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;

    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(error.message, "missing slug");
}

/// Sync is behind the readiness gate like every other mutation — it is not on the
/// two-path whitelist, so a draining daemon refuses it before the handler is reached.
#[tokio::test]
async fn roots_sync_503s_while_draining() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root(&tmp);
    let daemon = Harness::ready(&home).await;
    daemon.state.boot.mark_stopping();

    let error = err_body(
        daemon
            .post("/api/roots/sync", &json!({"slug": testfix::SLUG}))
            .await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;

    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_eq!(error.message, "server is draining");
}

/// …and behind the mutation guard: a state-changing POST from a foreign origin is 403
/// before anything is fetched.
#[tokio::test]
async fn roots_sync_403s_a_foreign_origin() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root(&tmp);
    let daemon = Harness::ready(&home).await;

    let response = daemon
        .client
        .post(daemon.url("/api/roots/sync"))
        .header("origin", "https://evil.example")
        .json(&json!({"slug": testfix::SLUG}))
        .send()
        .await
        .unwrap();

    let error = err_body(response, StatusCode::FORBIDDEN).await;
    assert_eq!(error.code, ErrorCode::Forbidden);
}

/// Move the fixture's source repo one commit ahead of the clone taken from it, so a
/// sync has something to fast-forward onto. The source lives in the caller's own
/// scratch, so this touches nothing outside the test.
fn commit_ahead(tmp: &TempDir) {
    let src = tmp.path().join("src");
    std::fs::write(src.join("AHEAD.md"), "ahead").unwrap();
    let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
    testfix::git(&src, &[&id[..], &["add", "."]].concat());
    testfix::git(&src, &[&id[..], &["commit", "-q", "-m", "ahead"]].concat());
}

// ─── worktrees ──────────────────────────────────────────────────────────────────

/// v1 `worktrees_controller_test.exs:35`.
#[tokio::test]
async fn worktrees_remove_404s_an_undeclared_name() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root(&tmp);
    let daemon = Harness::ready(&home).await;

    let error = err_body(
        daemon
            .post(
                "/api/worktrees/remove",
                &json!({"slug": testfix::SLUG, "name": "no-such-wt"}),
            )
            .await,
        StatusCode::NOT_FOUND,
    )
    .await;

    assert_eq!(error.code, ErrorCode::NotFound);
    assert_eq!(
        error.data,
        Some(json!({"slug": testfix::SLUG, "name": "no-such-wt"}))
    );
}

/// v1 `worktrees_controller_test.exs:24` — the declared worktree is git-removed and
/// undeclared, and the *name* is what comes back (not the slug).
#[tokio::test]
async fn worktrees_remove_removes_a_declared_worktree() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root_and_worktree(&tmp);
    let daemon = Harness::ready(&home).await;
    let worktree = testfix::root_dir(&home, testfix::SLUG).join("feat");
    assert!(worktree.is_dir());

    let data: RemoveWorktreeData = ok_body(
        daemon
            .post(
                "/api/worktrees/remove",
                &json!({"slug": testfix::SLUG, "name": "feat"}),
            )
            .await,
        StatusCode::OK,
    )
    .await;

    assert_eq!(data.removed, "feat");
    assert!(!worktree.exists());
    assert!(
        !grove_ops::worktrees::list(&home, testfix::SLUG)
            .unwrap()
            .iter()
            .any(|w| w.name == "feat" && w.declared)
    );
}

/// The lane's backpressure, seen from the wire: a root whose foreground queue is at
/// its bound answers 503 `unavailable` rather than growing without limit. v1's
/// `ops_busy`, in the readiness vocabulary — "come back", not "your request was
/// wrong".
///
/// The lane is held by one blocking job and then filled to its depth, so the request
/// under test is the one that finds no room.
#[tokio::test]
async fn worktrees_remove_503s_when_the_root_lane_is_saturated() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root(&tmp);
    let daemon = Harness::ready(&home).await;
    let lanes = std::sync::Arc::clone(&daemon.state.lanes);

    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let holder = {
        let lanes = std::sync::Arc::clone(&lanes);
        tokio::spawn(async move {
            lanes
                .run(testfix::SLUG, Priority::Foreground, move || {
                    let _ = held.blocking_recv();
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut queued = Vec::new();
    for _ in 0..FOREGROUND_DEPTH {
        let lanes = std::sync::Arc::clone(&lanes);
        queued.push(tokio::spawn(async move {
            lanes.run(testfix::SLUG, Priority::Foreground, || ()).await
        }));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    let error = err_body(
        daemon
            .post(
                "/api/worktrees/remove",
                &json!({"slug": testfix::SLUG, "name": "feat"}),
            )
            .await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;

    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("busy"), "{}", error.message);
    assert_eq!(error.data, Some(json!({"slug": testfix::SLUG})));

    release.send(()).unwrap();
    holder.await.unwrap().unwrap();
    for handle in queued {
        handle.await.unwrap().unwrap();
    }
}

/// A body missing `name` is a 422, with the message naming both fields.
#[tokio::test]
async fn worktrees_remove_422s_an_incomplete_body() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let error = err_body(
        daemon
            .post("/api/worktrees/remove", &json!({"slug": "o/r"}))
            .await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;

    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(error.message, "missing slug or name");
}

// ─── doctor ─────────────────────────────────────────────────────────────────────

/// v1 `doctor_controller_test.exs:9,16` — the rich payload: the share report, the
/// pool levels beside it, and the `statuses` array (empty until phase 4's engines
/// exist, and present from day one so the shape never changes under a client).
#[tokio::test]
async fn doctor_returns_the_report_pools_and_statuses() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root_and_worktree(&tmp);
    let manifest = home.join("manifest.toml");
    grove_ops::manifest::add_share(&manifest, testfix::SLUG, "symlink", &[".env"]).unwrap();
    grove_ops::manifest::set_pool_size(&manifest, testfix::SLUG, 1).unwrap();
    let daemon = Harness::ready(&home).await;

    let data: DoctorData = ok_body(
        daemon.post("/api/doctor", &json!({"dry_run": true})).await,
        StatusCode::OK,
    )
    .await;

    assert!(
        data.report.iter().any(|o| o.path == ".env"),
        "the declared share is reported"
    );
    assert_eq!(data.pools.len(), 1);
    assert_eq!(data.pools[0].target, 1);
    assert!(
        data.statuses.is_empty(),
        "this harness claims the reconcile mailbox, so it runs no engine room — and \
         a daemon with no engines honestly reports no engine statuses"
    );
    // The plumbing half, in the shape the CLI renders: the manifest and this root's
    // bare/trunk all check out, so every row is an `ok` rather than a finding.
    assert!(
        data.checks
            .iter()
            .any(|c| c.check == CheckKind::Manifest && c.status == CheckStatus::Ok),
        "{:?}",
        data.checks
    );
    for kind in [CheckKind::Bare, CheckKind::Trunk] {
        let check = data.checks.iter().find(|c| c.check == kind).unwrap();
        assert_eq!(check.status, CheckStatus::Ok, "{kind:?}");
        assert_eq!(check.slug.as_deref(), Some(testfix::SLUG));
    }
    assert!(
        data.checks.iter().all(|c| !c.is_finding()),
        "a healthy home reports no findings: {:?}",
        data.checks
    );
}

/// A body-less doctor POST is a whole-home run — v1 read its params with a strict
/// `== true`, so an absent field was false.
#[tokio::test]
async fn doctor_accepts_a_body_less_post() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let data: DoctorData = ok_body(daemon.post_bare("/api/doctor").await, StatusCode::OK).await;

    assert!(data.report.is_empty(), "an empty home reports nothing");
}

/// A body that is present but unlabelled is refused, not dropped. Dropping it is how
/// `{"slug":"o/r","dry_run":true}` becomes a whole-home *materializing* converge: the
/// two flags this route branches on both default to false. The route already 422s a
/// malformed body, so refusing an undeclared one keeps one rule instead of two.
#[tokio::test]
async fn doctor_422s_a_body_sent_without_a_json_content_type() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let error = err_body(
        daemon
            .post_unlabelled("/api/doctor", r#"{"slug":"o/r","dry_run":true}"#)
            .await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;

    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(
        error.message,
        "doctor request body must be sent as application/json"
    );
}

/// The other half of the same rule: a declared body that does not parse is the client
/// bug it looks like, never a default run.
#[tokio::test]
async fn doctor_422s_a_malformed_body() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let error = err_body(
        daemon.post("/api/doctor", &json!({"dry_run": "yes"})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;

    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(error.message, "malformed doctor request");
}

// ─── fallbacks ──────────────────────────────────────────────────────────────────

/// v1's framework fallback rendered the envelope from a *second* hand-written
/// literal. Here even an unrouted path comes out of the one serializer.
#[tokio::test]
async fn an_unknown_route_renders_the_error_envelope() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let error = err_body(daemon.get("/api/nope").await, StatusCode::NOT_FOUND).await;

    assert_eq!(error.code, ErrorCode::Error);
    assert_eq!(error.message, "Not Found");
}

#[tokio::test]
async fn a_wrong_method_renders_the_error_envelope() {
    let tmp = TempDir::new().unwrap();
    let daemon = Harness::ready(tmp.path()).await;

    let error = err_body(
        daemon.get("/api/doctor").await,
        StatusCode::METHOD_NOT_ALLOWED,
    )
    .await;

    assert_eq!(error.code, ErrorCode::Error);
    assert_eq!(error.message, "Method Not Allowed");
}
