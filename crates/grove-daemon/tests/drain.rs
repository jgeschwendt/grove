//! The drain, and what a *reader* may cost a daemon that is trying to stop.
//!
//! Two properties, both of which the assembled daemon got wrong and neither of which
//! any unit-level test could see:
//!
//! - **A graceful shutdown is bounded.** `axum::serve` waits for every in-flight
//!   connection, and an SSE response finishes only when its body is written — so a
//!   peer that stops reading holds `grove serve` open forever, and `grove off`
//!   recovers only by escalating to SIGTERM, killing whatever git work was in flight.
//! - **Read traffic does not shed mutations.** A snapshot takes a lane job per root,
//!   and `GET /api/roots` builds one on every call; sharing the mutation queue let a
//!   polling UI fill a cloning root's 256 slots and turn every subsequent
//!   `POST /api/worktrees/remove` on that root into a 503 `unavailable`.
//!
//! Hermetic throughout: an ephemeral loopback port, a per-test `TempDir` home, no
//! network. The stalled-reader test speaks raw TCP because that is the only way to
//! *be* a client that does not read.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use grove_api::events::{LogField, LogLevel, LogLine};
use grove_daemon::lane::{FOREGROUND_DEPTH, Priority};
use grove_daemon::{AppState, Config, Daemon};
use grove_ops::clock::SystemClock;
use grove_ops::testfix;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// The drain budget under test. Short so a wedge is a fast failure rather than a
/// hung suite, and long enough that an ordinary drain finishes well inside it.
const DRAIN: Duration = Duration::from_millis(400);

/// How long a test waits for `serve` to return before calling it wedged. Several
/// times [`DRAIN`], so a pass is about the bound and not about the machine's mood.
const PATIENCE: Duration = Duration::from_secs(10);

struct Harness {
    addr: SocketAddr,
    state: AppState,
    serving: tokio::task::JoinHandle<Result<(), grove_daemon::Error>>,
}

impl Harness {
    /// A ready daemon over `home` with the engine room running and a short drain
    /// budget. The shutdown route stays armed — these tests fire the trigger
    /// directly, which is the same signal it schedules.
    async fn start(home: &Path, config: impl FnOnce(Config) -> Config) -> Self {
        let base = Config::new(home, SocketAddr::from(([127, 0, 0, 1], 0)))
            .unwrap()
            .with_drain_budget(DRAIN);
        let daemon = Daemon::bind(config(base), Arc::new(SystemClock))
            .await
            .unwrap();
        let addr = daemon.local_addr().unwrap();
        let state = daemon.state().clone();
        state.boot.mark_ready();
        let serving = tokio::spawn(async move { daemon.serve().await });
        Self {
            addr,
            state,
            serving,
        }
    }

    fn client() -> reqwest::Client {
        // No proxy: an `HTTP_PROXY` in the environment must not sit between a test
        // and its own loopback server.
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// Fire the drain and wait for `serve` to return, or fail the test.
    async fn drain(self) {
        self.state.shutdown.fire();
        tokio::time::timeout(PATIENCE, self.serving)
            .await
            .expect(
                "`serve` must return within its drain budget: an unbounded drain is a \
                 `grove serve` that only ever exits by being killed",
            )
            .expect("the serve task did not panic")
            .expect("serving ended cleanly");
    }
}

/// **One client that stops reading must not hold the daemon open.**
///
/// The connection is raw TCP: it asks for the stream and then never reads a byte.
/// Enough large log lines are recorded to fill the per-connection channel *and*
/// several megabytes of socket buffer behind it, which is what makes the wedge real
/// rather than a matter of timing — the response body cannot finish writing to a peer
/// that is not draining it, however cleanly the producer exits.
///
/// Both halves of the fix are load-bearing here. Without the producer's send racing
/// the drain, the pump parks on a full channel and never ends the stream at all;
/// without `Daemon::serve`'s bounded drain, the bytes already handed to hyper wedge
/// the connection anyway.
#[tokio::test]
async fn a_client_that_stops_reading_cannot_hold_the_drain_open() {
    let tmp = TempDir::new().unwrap();
    let harness = Harness::start(tmp.path(), |config| config).await;

    let mut socket = std::net::TcpStream::connect(harness.addr).unwrap();
    socket
        .write_all(
            format!(
                "GET /api/events HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n\r\n",
                harness.addr
            )
            .as_bytes(),
        )
        .unwrap();
    socket.flush().unwrap();
    // …and from here the client reads nothing, ever.

    // Far more bytes than any socket buffer will absorb, so what is queued for this
    // connection genuinely cannot be written.
    for n in 0..256 {
        harness.state.logs.record(LogLine {
            at_ms: 1,
            level: LogLevel::Warn,
            target: "grove_daemon::drain".into(),
            message: "x".repeat(64 * 1024),
            fields: vec![LogField {
                name: "count".into(),
                value: n.to_string(),
            }],
        });
        if n % 32 == 0 {
            tokio::task::yield_now().await;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    harness.drain().await;
    drop(socket);
}

/// The ordinary case the bound must not break: a client that *is* reading gets its
/// stream ended and the daemon drains immediately, nowhere near its budget.
#[tokio::test]
async fn a_reading_client_is_ended_rather_than_abandoned() {
    let tmp = TempDir::new().unwrap();
    let harness = Harness::start(tmp.path(), |config| config).await;

    let mut socket = tokio::net::TcpStream::connect(harness.addr).await.unwrap();
    socket
        .write_all(
            format!(
                "GET /api/events HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n\r\n",
                harness.addr
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    // Read to EOF. The server closing the response is the only thing that ends this.
    let reader = tokio::spawn(async move {
        let mut buffer = [0u8; 8192];
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    });
    // Let the opening snapshot land, so the connection is genuinely established.
    tokio::time::sleep(Duration::from_millis(100)).await;

    harness.drain().await;
    tokio::time::timeout(PATIENCE, reader)
        .await
        .expect("the stream ends when the daemon drains")
        .unwrap();
}

/// **Lane work is drained, not abandoned on the first pass.**
///
/// `serve` returning with a `git worktree add` still running hands the problem to
/// runtime drop, which blocks until every started blocking task finishes — with no
/// bound, no log line and nothing an operator can wait for. So the drain waits for
/// the lane, inside the same budget that bounds the connections.
#[tokio::test]
async fn a_drain_waits_for_git_work_already_on_a_lane() {
    let tmp = TempDir::new().unwrap();
    let harness = Harness::start(tmp.path(), |config| {
        config.with_drain_budget(Duration::from_secs(30))
    })
    .await;

    let landed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let job = {
        let (lanes, landed) = (Arc::clone(&harness.state.lanes), Arc::clone(&landed));
        tokio::spawn(async move {
            lanes
                .run("o/r", Priority::Background, move || {
                    std::thread::sleep(Duration::from_millis(300));
                    landed.store(true, std::sync::atomic::Ordering::SeqCst);
                })
                .await
        })
    };
    // Let it reach the head of its lane before the drain starts.
    tokio::time::sleep(Duration::from_millis(50)).await;

    harness.drain().await;
    assert!(
        landed.load(std::sync::atomic::Ordering::SeqCst),
        "`serve` returned while a git write was still running: the process would then \
         block on runtime drop with nothing said"
    );
    job.await.unwrap().unwrap();
}

/// **Read traffic must not shed a mutation.**
///
/// The root's lane is held by one long foreground op — the stand-in for an in-flight
/// clone, which can hold a lane for an hour. A polling UI then issues far more
/// snapshots than the foreground queue is deep. With reads sharing that queue, the
/// `POST /api/worktrees/remove` behind them came back 503 `unavailable`; on the read
/// tier it is simply admitted, and answers once the lane is free.
#[tokio::test]
async fn read_traffic_never_sheds_a_mutation() {
    const READS: usize = FOREGROUND_DEPTH + 64;

    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root_and_worktree(&tmp);
    let harness = Harness::start(&home, |config| config).await;
    let client = Harness::client();

    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let (entered, running) = tokio::sync::oneshot::channel::<()>();
    let holder = {
        let lanes = Arc::clone(&harness.state.lanes);
        tokio::spawn(async move {
            lanes
                .run(testfix::SLUG, Priority::Foreground, move || {
                    let _ = entered.send(());
                    let _ = held.blocking_recv();
                })
                .await
        })
    };
    running
        .await
        .expect("the holder reached the head of the lane");

    // Concurrently, as a dashboard polling a busy home actually arrives: every one
    // takes a lane job per declared root, and every one gives up on it at the
    // snapshot budget while the job stays queued.
    let mut reads = Vec::with_capacity(READS);
    for _ in 0..READS {
        let (client, url) = (client.clone(), harness.url("/api/roots"));
        reads.push(tokio::spawn(async move { client.get(url).send().await }));
    }
    for read in reads {
        let response = read.await.unwrap().unwrap();
        assert!(
            response.status().is_success(),
            "a read must still answer, shed or not"
        );
    }

    let mutation = {
        let (client, url) = (client.clone(), harness.url("/api/worktrees/remove"));
        tokio::spawn(async move {
            client
                .post(url)
                .json(&serde_json::json!({"slug": testfix::SLUG, "name": "feat"}))
                .send()
                .await
                .unwrap()
        })
    };
    // The lane is busy, so the mutation is queued; releasing the holder lets it run.
    tokio::time::sleep(Duration::from_millis(50)).await;
    release.send(()).unwrap();
    holder.await.unwrap().unwrap();

    let response = tokio::time::timeout(PATIENCE, mutation)
        .await
        .expect("the mutation was admitted rather than shed")
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(
        status.is_success(),
        "read traffic shed a destructive mutation: {status} {body}"
    );

    harness.drain().await;
}
