//! The read surface: one snapshot shape, served two ways.
//!
//! - [`snapshot`] assembles [`Snapshot`] — every declared root with its engine
//!   status, pool level, sync state, trunk and worktrees, plus the log ring. It is
//!   the body of `GET /api/roots` and the first frame of `GET /api/events`, and it is
//!   deliberately one type: a UI that renders from the stream and a UI that polls
//!   must be looking at the same thing.
//! - [`events`] serves the stream: snapshot, then every state event, every log line,
//!   and a heartbeat comment while nothing else is happening.
//!
//! ## What the stream promises
//!
//! **A drain ends every connection, on a bound.** An SSE response finishes only when
//! its body stops being written, so nothing about a stream ends on its own. Two things
//! together end one: [`pump`] returns on the shutdown signal — on *every* await,
//! sends included — which closes the body; and `Daemon::serve` bounds the whole drain,
//! because a peer that has stopped reading cannot be written to at all and the bytes
//! already handed to hyper would otherwise wedge the connection regardless. Neither
//! half is sufficient alone.
//!
//! **A connection never misses silently.** The bus is bounded and drops for a slow
//! subscriber (see [`crate::events`]); when it does, the subscriber is told it
//! lagged and this route answers with a fresh [`Event::Resync`] rather than
//! continuing from a gap. The log channel's overflow is reported the only way it can
//! be — as a line saying how many were lost.
//!
//! **The heartbeat rides the clock seam.** axum's own `KeepAlive` sleeps on tokio's
//! clock; every other budget in this daemon is a `Deadline` read off the injected
//! [`Clock`], and a test that wants to see a heartbeat should advance a fake clock
//! rather than wait a real quarter minute.
//!
//! ## Order of operations on connect
//!
//! Subscribe first, *then* build the snapshot. The reverse leaves a window in which
//! an event lands after the read and before the subscription, and is lost. This way
//! the same event is merely delivered twice — once folded into the snapshot, once as
//! its own frame — which costs a redundant re-read and nothing else, because every
//! state event is level-triggered.

use std::convert::Infallible;
use std::path::Path;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event as SseEvent, Sse};
use grove_api::events::{LogField, LogLevel, LogLine};
use grove_api::routes::{PoolView, RootView, Snapshot, WorktreeView};
use grove_api::{Event, RootStatus, SyncNote};
use grove_ops::git;
use grove_ops::worktrees::WorktreeStatus;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt as _};

use crate::app::AppState;
use crate::engine::Engine;
use crate::lane::Priority;
use crate::reply::Reply;
use crate::wait;

/// How long a connection may sit silent before a comment frame proves it is alive.
/// Long enough not to be chatter, short enough to beat the idle timeout of any
/// reverse proxy a UI might sit behind.
///
/// It is also how a *gone* client is noticed: a producer learns its reader has
/// dropped only by failing to send, so an idle connection that vanished costs one
/// task until the next beat and no longer.
pub const HEARTBEAT: Duration = Duration::from_secs(15);

/// How long one root's git reads have to answer while a snapshot is assembled.
/// v1's dashboard fan-out budget. A root that misses it — or whose read the lane shed
/// outright — is reported `unavailable` rather than delaying every other root's row,
/// and the job it abandoned is dropped at the head of the lane rather than run.
pub const SNAPSHOT_BUDGET: Duration = Duration::from_secs(3);

/// Frames buffered between the producer task and the socket. A client that stops
/// reading fills this, and then the producer parks on `send` — backpressure that
/// stops at this connection instead of reaching the bus, which is exactly why the
/// producer is a task with a channel rather than a stream that borrows the bus.
const STREAM_DEPTH: usize = 64;

/// `GET /api/events` — the push channel: a snapshot, then everything as it happens.
pub async fn events(State(state): State<AppState>) -> Sse<impl Stream<Item = SseResult>> {
    // Subscribed before the snapshot is read — see the module doc.
    let bus = state.events.subscribe();
    let logs = state.logs.subscribe();
    let (tx, rx) = mpsc::channel(STREAM_DEPTH);
    tokio::spawn(pump(state, bus, logs, tx));
    Sse::new(ReceiverStream::new(rx).map(Frame::into_sse))
}

/// `GET /api/roots` — the same snapshot, once, for a caller that is not streaming.
pub async fn roots(State(state): State<AppState>) -> Reply<Snapshot> {
    Reply::ok(snapshot(&state).await)
}

/// One thing to write down the socket.
///
/// The producer speaks in these rather than in `axum`'s opaque SSE type so its own
/// tests can read what it produced — an `sse::Event` can be built and sent and never
/// inspected again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Frame {
    /// A typed event: `event:` is its tag, `data:` its JSON.
    ///
    /// The tag is redundant with the payload's own `event` field, deliberately — a
    /// client using `addEventListener("task_finished", …)` reads the former and one
    /// using `onmessage` reads the latter, and neither should have to know about the
    /// other.
    Event(Event),
    /// A comment line. Keeps the socket and every proxy on it alive without a client
    /// having to filter a synthetic event out of its own handler.
    Beat,
}

impl Frame {
    /// Render for the wire. Infallible: these types always serialize, and a
    /// connection is not the place to discover otherwise, so the impossible case
    /// becomes a comment rather than a torn-down stream.
    fn into_sse(self) -> SseResult {
        Ok(match self {
            Self::Beat => SseEvent::default().comment("beat"),
            Self::Event(event) => match serde_json::to_string(&event) {
                Ok(data) => SseEvent::default().event(event.name()).data(data),
                Err(e) => {
                    tracing::error!(error = %e, "an event would not serialize");
                    SseEvent::default().comment("unserializable")
                }
            },
        })
    }
}

/// A frame, or the error type axum's `Sse` insists on.
type SseResult = Result<SseEvent, Infallible>;

/// The per-connection producer.
///
/// Every path out is the connection ending: a failed `send` (the client went away),
/// a closed bus, or the daemon draining. **The drain arm is load-bearing** —
/// `axum::serve`'s graceful shutdown waits for in-flight responses, and an SSE
/// response never finishes on its own, so without it one attached dashboard would
/// hold `grove off` open forever.
///
/// It is load-bearing on *every* await, which is why the sends go through
/// [`send_frame`] rather than a bare `tx.send(…).await`. A client that stops reading
/// fills [`STREAM_DEPTH`] and parks the producer on `send`; a drain arm that only
/// races `recv` never runs, and the connection outlives the shutdown that was
/// supposed to end it. (This is only half the guarantee: bytes already handed to
/// hyper still have to reach a socket nobody is draining, so `Daemon::serve` bounds
/// the whole drain — see its `drain_budget`.)
async fn pump(
    state: AppState,
    mut bus: broadcast::Receiver<Event>,
    mut logs: broadcast::Receiver<LogLine>,
    tx: mpsc::Sender<Frame>,
) {
    let opening = Frame::Event(Event::Snapshot(snapshot(&state).await));
    if !send_frame(&state, &tx, opening).await {
        return;
    }

    loop {
        // Rebuilt each pass, so the heartbeat marks *silence* rather than ticking
        // through a busy stream.
        let beat = state.clock.deadline(HEARTBEAT);
        let frame = tokio::select! {
            () = state.shutdown.wait() => return,
            event = bus.recv() => match event {
                Ok(event) => Frame::Event(event),
                // The events this subscriber missed are gone, and every one of them
                // was level-triggered — so the recovery is not to replay them but to
                // re-read the world, which is what a resync carries.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "an events subscriber lagged; resyncing it");
                    Frame::Event(Event::Resync(snapshot(&state).await))
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            line = logs.recv() => match line {
                Ok(line) => Frame::Event(Event::Log(line)),
                // A log line is not level-triggered: a dropped one is gone for good,
                // so the honest frame is a count of what this connection will never
                // see rather than silence.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    Frame::Event(Event::Log(dropped(&state, missed)))
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            () = wait::until(beat, &*state.clock) => Frame::Beat,
        };
        if !send_frame(&state, &tx, frame).await {
            return;
        }
    }
}

/// Hand one frame to the socket, or give up — `false` when the connection is over,
/// because the client went away *or* because the daemon is draining.
///
/// The drain arm is the load-bearing half: without it a producer parked on a full
/// channel (a client that stopped reading) never observes the shutdown it is
/// supposed to end on.
async fn send_frame(state: &AppState, tx: &mpsc::Sender<Frame>, frame: Frame) -> bool {
    tokio::select! {
        biased;
        () = state.shutdown.wait() => false,
        sent = tx.send(frame) => sent.is_ok(),
    }
}

/// The line that stands in for the ones a lagging connection lost.
fn dropped(state: &AppState, missed: u64) -> LogLine {
    LogLine {
        at_ms: state.logs.now_ms(),
        level: LogLevel::Warn,
        target: "grove_daemon::stream".into(),
        message: "this connection fell behind the log stream".into(),
        fields: vec![LogField {
            name: "missed".into(),
            value: missed.to_string(),
        }],
    }
}

/// Assemble the whole observable world.
///
/// Per root: two in-memory engine reads (status, sync state) and **one lane job**
/// carrying every git read the row needs — the worktree list, the pool count, the
/// declared target and the trunk's own status. One job because they are one round
/// trip through the root's lane, and on the lane because they read the same
/// repository the engine may be mid-clone in (carried law 6).
///
/// On the lane's **read** tier, not the foreground one. A snapshot is built by
/// `GET /api/roots`, by every `GET /api/events` connect, and by every bus-lag resync
/// inside [`pump`], so a polling UI produces an unbounded stream of these; sharing the
/// mutation queue let that traffic shed a `POST /api/roots/remove` with 503
/// `unavailable`. Here a flood costs the flooder its own rows and nothing else.
///
/// Roots are read concurrently — each on its own lane, so they do not queue behind
/// each other — and the rows come back in declared order regardless.
pub async fn snapshot(state: &AppState) -> Snapshot {
    let home = state.config.home.clone();
    let declared = tokio::task::spawn_blocking({
        let home = home.clone();
        move || grove_ops::roots::list(&home)
    })
    .await;
    let declared = match declared {
        Ok(Ok(roots)) => roots,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "reading declared roots failed");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, "reading declared roots panicked");
            Vec::new()
        }
    };

    let mut pending = Vec::with_capacity(declared.len());
    for root in declared {
        let state = state.clone();
        pending.push(tokio::spawn(async move { view(&state, root).await }));
    }
    let mut roots = Vec::with_capacity(pending.len());
    for handle in pending {
        match handle.await {
            Ok(view) => roots.push(view),
            // A panicked read is one root's row, not the whole snapshot's.
            Err(e) => tracing::warn!(error = %e, "a snapshot read panicked"),
        }
    }

    Snapshot {
        roots,
        logs: state.logs.recent(),
    }
}

/// What one root's git reads produce, in one lane job.
struct Reads {
    worktrees: Vec<WorktreeStatus>,
    pool: PoolView,
    trunk_status: Option<git::Status>,
}

async fn view(state: &AppState, root: grove_ops::manifest::Root) -> RootView {
    let slug = root.slug;
    let engine = match state.engines.get() {
        Some(engines) => engines.engine(&slug).await,
        None => None,
    };
    let state_of = engine_state(engine.as_ref()).await;

    let deadline = state.clock.deadline(SNAPSHOT_BUDGET);
    let reads = {
        let (home, owned) = (state.config.home.clone(), slug.clone());
        let job = state.lanes.run(&slug, Priority::Read, move || Reads {
            worktrees: grove_ops::worktrees::list(&home, &owned).unwrap_or_default(),
            pool: PoolView {
                observed: grove_ops::worktrees::pool_count(&home, &owned).unwrap_or(0),
                target: grove_ops::pool::size(&home, &owned).unwrap_or(0),
            },
            trunk_status: grove_ops::roots::trunk_status(&home, &owned).ok().flatten(),
        });
        wait::within(deadline, &*state.clock, job).await
    };

    let root_dir = grove_ops::roots::root_dir(&state.config.home, &slug);
    let (trunk, trunk_branch) = trunk_view(&state.config.home, &slug);
    match reads {
        Some(Ok(reads)) => RootView {
            slug,
            url: root.url,
            status: state_of.status,
            error: state_of.error,
            pool: reads.pool,
            syncing: state_of.syncing,
            sync_note: state_of.sync_note,
            trunk,
            trunk_branch,
            trunk_status: reads.trunk_status,
            worktrees: reads
                .worktrees
                .into_iter()
                .map(|wt| worktree_view(&root_dir, wt))
                .collect(),
        },
        // The row stands — a UI must still see the root — but it reports
        // `unavailable` rather than the engine's status: "ready" beside an empty
        // worktree list is a lie a client cannot detect, and this root's reads are
        // exactly what did not happen.
        outcome => {
            tracing::warn!(
                slug = %slug,
                timed_out = outcome.is_none(),
                "a root's snapshot reads did not complete"
            );
            RootView {
                slug,
                url: root.url,
                status: RootStatus::Unavailable,
                // Not the engine's degrade text: the row no longer reports the
                // status that text explains, and a reason printed beside
                // `unavailable` would name a failure this row is not describing.
                error: None,
                pool: PoolView::default(),
                syncing: state_of.syncing,
                sync_note: state_of.sync_note,
                trunk,
                trunk_branch,
                trunk_status: None,
                worktrees: Vec::new(),
            }
        }
    }
}

/// The trunk a row draws: its absolute path and the branch it checks out, resolved
/// together so the two halves cannot disagree.
///
/// Off the lane, like the [`grove_ops::roots::root_dir`] join beside it: the manifest
/// read is the same one `roots::list` already did to produce this row, and the bare's
/// `HEAD` is a ref file rather than a working tree the engine could be mid-clone in.
///
/// A root whose bare has no answer yet — declared and unrealized, or mid-clone — still
/// has to draw a row. It falls back to [`grove_ops::roots::trunk_dir`]'s own guess and
/// names the branch after the directory that guess picked, so the path and the branch
/// stay one story instead of pairing a guessed path with a blank branch. That name is
/// the folded one, so a slash-bearing trunk reads back here as `feature-x` rather than
/// `feature/x` — a placeholder for a root that has no branch to report yet, not a ref
/// anything may resolve. It is replaced by the real branch the moment the bare answers.
fn trunk_view(home: &Path, slug: &str) -> (String, String) {
    grove_ops::roots::trunk(home, slug).map_or_else(
        |_| {
            let dir = grove_ops::roots::trunk_dir(home, slug);
            let branch = dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            (dir.display().to_string(), branch)
        },
        |trunk| (trunk.dir.display().to_string(), trunk.branch),
    )
}

/// Everything a row reads out of the running engine, as one value: what the two
/// in-memory reads below answer.
struct EngineState {
    status: RootStatus,
    /// Why the root is `degraded`; see [`grove_api::routes::RootView::error`].
    error: Option<String>,
    syncing: bool,
    sync_note: Option<SyncNote>,
}

/// The two in-memory engine reads. A root with no engine — the daemon is running
/// without an engine room, or the set has not caught up with a fresh declaration —
/// reports `unknown`, which is what "no driver has an opinion yet" means.
async fn engine_state(engine: Option<&Engine>) -> EngineState {
    let Some(engine) = engine else {
        return EngineState {
            status: RootStatus::Unknown,
            error: None,
            syncing: false,
            sync_note: None,
        };
    };
    let info = engine.status_info().await.ok();
    let sync = engine.sync_info().await.ok();
    EngineState {
        status: info.as_ref().map_or(RootStatus::Unavailable, |i| i.status),
        error: info.and_then(|i| i.error),
        syncing: sync.is_some_and(|sync| sync.syncing),
        sync_note: sync.and_then(|sync| sync.note),
    }
}

fn worktree_view(root_dir: &std::path::Path, wt: WorktreeStatus) -> WorktreeView {
    WorktreeView {
        path: root_dir.join(&wt.name).display().to_string(),
        name: wt.name,
        branch: wt.branch,
        base: wt.base,
        declared: wt.declared,
        present: wt.present,
        status: wt.status,
    }
}

#[cfg(test)]
mod tests {
    use super::{Frame, HEARTBEAT, STREAM_DEPTH, pump};
    use crate::app::{AppState, Daemon};
    use crate::config::Config;
    use crate::events::CAPACITY;
    use grove_api::events::{Event, LogLevel, LogLine, TaskKind};
    use grove_ops::clock::{Clock, TestClock};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    /// The pump on its own: a bound daemon's state, no engine room, no listener
    /// serving. Everything the producer does is observable through the channel it
    /// writes, which is the whole reason it speaks [`Frame`] rather than axum's
    /// opaque SSE type.
    async fn attach(home: &TempDir) -> (AppState, Arc<TestClock>, mpsc::Receiver<Frame>) {
        let clock = Arc::new(TestClock::new());
        let config = Config::new(home.path(), SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let daemon = Daemon::bind(config, Arc::clone(&clock) as Arc<dyn Clock>)
            .await
            .unwrap();
        let state = daemon.state().clone();
        let (tx, rx) = mpsc::channel(STREAM_DEPTH);
        tokio::spawn(pump(
            state.clone(),
            state.events.subscribe(),
            state.logs.subscribe(),
            tx,
        ));
        (state, clock, rx)
    }

    async fn next(rx: &mut mpsc::Receiver<Frame>) -> Frame {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a frame arrives")
            .expect("the stream is still open")
    }

    /// A UI must render without a second request, so the first thing down the wire is
    /// the whole world.
    #[tokio::test]
    async fn the_first_frame_is_a_snapshot() {
        let home = TempDir::new().unwrap();
        let (_state, _clock, mut rx) = attach(&home).await;
        assert!(matches!(
            next(&mut rx).await,
            Frame::Event(Event::Snapshot(_))
        ));
    }

    /// The lag path: a connection that falls behind the bounded bus is **resynced**,
    /// not silently short a few events. Nothing reads `rx` until the publisher has
    /// overrun both the bus and the channel in front of it, which is the only way to
    /// produce a real `Lagged` rather than simulate one.
    #[tokio::test]
    async fn a_lagging_connection_is_resynced_rather_than_left_short() {
        let home = TempDir::new().unwrap();
        let (state, _clock, mut rx) = attach(&home).await;

        for n in 0..(CAPACITY + STREAM_DEPTH) * 4 {
            state.events.publish(Event::TaskStarted {
                slug: format!("o/{n}"),
                kind: TaskKind::Reconcile,
            });
        }

        let mut seen = 0;
        while !matches!(next(&mut rx).await, Frame::Event(Event::Resync(_))) {
            seen += 1;
            assert!(seen < 1000, "the resync never came");
        }
    }

    /// The heartbeat is a *comment* on the clock seam: a fake advance produces it,
    /// which is what keeps a 15-second budget from costing a test 15 seconds.
    #[tokio::test]
    async fn a_silent_stream_beats_on_the_injected_clock() {
        let home = TempDir::new().unwrap();
        let (_state, clock, mut rx) = attach(&home).await;
        assert!(matches!(
            next(&mut rx).await,
            Frame::Event(Event::Snapshot(_))
        ));

        // Nothing has happened, and nothing will: the only frame that can arrive is
        // the beat, and only once the clock passes the budget.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), rx.recv())
                .await
                .is_err(),
            "a beat before its budget would be chatter"
        );
        clock.advance(HEARTBEAT);
        assert_eq!(next(&mut rx).await, Frame::Beat);
    }

    /// A log line reaches an attached connection as its own frame — the parallel
    /// channel, arriving on the same wire.
    #[tokio::test]
    async fn a_recorded_log_line_becomes_a_frame() {
        let home = TempDir::new().unwrap();
        let (state, _clock, mut rx) = attach(&home).await;
        assert!(matches!(
            next(&mut rx).await,
            Frame::Event(Event::Snapshot(_))
        ));

        state.logs.record(LogLine {
            at_ms: 1,
            level: LogLevel::Warn,
            target: "grove_daemon::engine".into(),
            message: "root sync failed".into(),
            fields: Vec::new(),
        });

        let Frame::Event(Event::Log(line)) = next(&mut rx).await else {
            panic!("expected a log frame")
        };
        assert_eq!(line.message, "root sync failed");
    }

    /// **…including a producer parked on a full channel** — the case the drain arm
    /// on `recv` alone never covers.
    ///
    /// A client that stops reading fills [`STREAM_DEPTH`] and the producer parks on
    /// `send`, where a `select!` that races only the *receives* never reaches its
    /// shutdown arm. Nothing here reads the channel: the assertion is that it
    /// **closes**, which happens only when the producer returned and dropped its
    /// sender. (Even so this is half the guarantee — bytes already handed to hyper
    /// still have to reach a socket nobody is draining, which is why `Daemon::serve`
    /// bounds the whole drain.)
    #[tokio::test]
    async fn a_producer_parked_on_a_full_channel_still_ends_on_the_drain() {
        let home = TempDir::new().unwrap();
        let (state, _clock, rx) = attach(&home).await;

        for n in 0..STREAM_DEPTH * 4 {
            state.events.publish(Event::TaskStarted {
                slug: format!("o/{n}"),
                kind: TaskKind::Reconcile,
            });
        }
        // Long enough for the producer to fill the channel and park on `send`.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!rx.is_closed(), "the producer is still attached");

        state.shutdown.fire();

        for _ in 0..100 {
            if rx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("a producer parked on a full channel never observed the drain");
    }

    /// **A stream ends when the daemon drains** — the producer half. `axum::serve`'s
    /// graceful shutdown waits for in-flight responses and an SSE response never
    /// completes on its own, so a producer that ignored the drain would hold one open
    /// forever. This pins the ordinary path (the producer is parked on a `recv`); the
    /// parked-on-`send` path is beside it, and the connection-level bound that covers
    /// a peer which cannot be written to at all lives in `tests/drain.rs`.
    #[tokio::test]
    async fn a_drain_ends_every_open_stream() {
        let home = TempDir::new().unwrap();
        let (state, _clock, mut rx) = attach(&home).await;
        assert!(matches!(
            next(&mut rx).await,
            Frame::Event(Event::Snapshot(_))
        ));

        state.shutdown.fire();

        assert!(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("the producer notices the drain")
                .is_none(),
            "the channel closes rather than going quiet"
        );
    }
}
