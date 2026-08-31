//! Per-root lanes — the single-git-writer-per-root law (carried law 6), made
//! mechanical.
//!
//! `grove-ops` was written against this invariant and does not enforce it: its
//! global `flock` serializes the *manifest*, deliberately not git. Two concurrent
//! `git worktree add`s on one root race an index lock; a remove racing a reconcile
//! deletes a tree the reconcile is mid-clone into. So every git-writing op for a
//! root — reconcile, sync, fill, promote, remove, worktree ops, doctor's converge —
//! goes through that root's lane, and only one of them runs at a time.
//!
//! v1 got this from the BEAM: one `grove-ops` OS process per active root, its
//! mailbox the queue. Here a lane is a tokio task owning its three queues, and the work
//! itself runs on `spawn_blocking` — grove-ops is synchronous (it spawns git and
//! takes an `flock`), and running one of its calls on an async worker would park a
//! runtime thread for the length of a clone.
//!
//! ## Three priorities, no preemption
//!
//! - **Foreground** — a *mutation* a user is waiting on: a remove, a promote, a
//!   doctor converge. Bounded at [`FOREGROUND_DEPTH`]; an overflow is
//!   [`LaneError::Busy`] rather than unbounded memory, because a queue that deep
//!   means the lane is wedged and shedding is the honest answer.
//! - **Read** — the git reads a snapshot needs. Its own, much shallower queue
//!   ([`READ_DEPTH`]) for one reason: reads arrive from *polling*, mutations from a
//!   human. Sharing the foreground queue let a dashboard refreshing every 100 ms fill
//!   a cloning root's 256 slots in half a minute, after which `POST /api/roots/remove`
//!   on that root answered 503 `unavailable` — "this root is wedged" — because of
//!   traffic that wedged nothing. Shedding a read costs one `unavailable` row in one
//!   snapshot, and the next poll asks again.
//! - **Background** — the engine's level-triggered convergence: reconcile, sync,
//!   fill. Unbounded, because it is *coalesced desired state* — at most one
//!   outstanding per root, per kind — so it cannot grow, and a bound here could shed
//!   a foreground slot's worth of memory pressure onto work nobody is waiting for.
//!
//! They drain strictly in that order, and an in-flight op is **never preempted**: a
//! foreground op on a cloning root waits out that clone (a foreground op on *another*
//! root runs in parallel — that is the whole point of per-root lanes).
//!
//! A queued **read** whose caller has already given up is dropped at the head of the
//! lane rather than run — its answer would reach nobody. Reads only; a mutation runs
//! to completion however bored its caller got, because a git write half-applied
//! because someone closed a tab is exactly the damage grove refuses to do.
//!
//! ## Lazy spawn, race-free idle reap
//!
//! A lane appears on first use and reaps itself after [`Lanes::idle`] of nothing in
//! flight and every queue empty. The reap is the delicate part — v1's
//! undeclare→redeclare race — and it is closed by doing both halves under the
//! registry lock: [`Lanes::submit`] holds it while it looks up *and* enqueues, and
//! the reap holds it while it re-checks emptiness *and* unregisters. So a submit
//! either lands before the re-check (the lane sees a non-empty queue and lives) or
//! after the unregister (it finds no entry and spawns a fresh lane). There is no
//! interleaving in which a job is handed to a lane that has decided to die.
//!
//! ## Quiescence
//!
//! [`Lanes`] counts the jobs it has accepted and not yet finished, so a drain can ask
//! "is any git write still in flight?" ([`Lanes::quiesced`]). Without it a shutdown
//! returns while a `worktree add` is mid-write and the *process* then blocks
//! invisibly on runtime drop — a hang with no log line and no bound.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use grove_ops::clock::Clock;
use tokio::sync::{mpsc, oneshot, watch};

use crate::wait;

/// The foreground backlog bound, carried from v1's `@max_queue`.
pub const FOREGROUND_DEPTH: usize = 256;

/// The read backlog bound. Deliberately shallow — see the module doc: reads come
/// from polling, and a read backlog deeper than the work in front of it is a
/// dashboard queueing answers it will never look at.
pub const READ_DEPTH: usize = 16;

/// How long a lane sits idle before reaping itself. v1's `@idle_timeout`.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(60);

/// Which queue an op joins.
///
/// Declared in drain order: everything in [`Priority::Foreground`] runs before
/// anything in [`Priority::Read`], which runs before anything in
/// [`Priority::Background`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Priority {
    /// A mutation someone is waiting on.
    Foreground,
    /// A git read someone is waiting on — a snapshot row. Sheds early, and is
    /// abandoned at the head of the lane if its caller has already gone.
    Read,
    /// Convergence work the engine drives; nobody is blocked on it.
    Background,
}

/// Why a lane could not deliver a result. Distinct from the op's *own* failure,
/// which travels inside the `T` the caller asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneError {
    /// The queue this op asked for is at its bound — [`FOREGROUND_DEPTH`] for a
    /// mutation, [`READ_DEPTH`] for a read. v1's `:ops_busy`.
    Busy,
    /// The op never produced a result — it panicked, or its lane went away with the
    /// runtime. The engine reads this as v1 read a crashed background task.
    Lost,
}

impl std::fmt::Display for LaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "ops busy: this root's work queue is full",
            Self::Lost => "the operation did not complete",
        })
    }
}

impl std::error::Error for LaneError {}

/// The closure a caller handed in, type-erased so one lane can carry ops of every
/// return type without a channel per type.
type Work = Box<dyn FnOnce() + Send + 'static>;

/// A unit of lane work, plus its share of the in-flight count.
///
/// The count is decremented by `Drop` rather than by the lane, so a job that is shed
/// at submission, dropped with a retiring lane, or panics mid-op is accounted for
/// exactly like one that ran — a drain that could be wedged by an unlucky exit path
/// would be worse than no drain at all.
struct Job {
    work: Option<Work>,
    inflight: Arc<watch::Sender<usize>>,
}

impl Job {
    fn new(inflight: &Arc<watch::Sender<usize>>, work: Work) -> Self {
        inflight.send_modify(|n| *n += 1);
        Self {
            work: Some(work),
            inflight: Arc::clone(inflight),
        }
    }

    /// Run it, and release its slot — consuming `self`, so the two cannot separate.
    fn call(mut self) {
        if let Some(work) = self.work.take() {
            work();
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.inflight.send_modify(|n| *n = n.saturating_sub(1));
    }
}

struct LaneEntry {
    foreground: mpsc::Sender<Job>,
    read: mpsc::Sender<Job>,
    background: mpsc::UnboundedSender<Job>,
}

/// The live lanes, keyed by slug.
///
/// Held in its own `Arc` rather than inside [`Lanes`] so a running lane can
/// unregister itself without holding the registry's owner alive.
type Registry = Arc<Mutex<HashMap<String, LaneEntry>>>;

/// The lane registry: one per daemon, shared by the routes and every engine.
// stele:landmark per-root-lane
pub struct Lanes {
    registry: Registry,
    clock: Arc<dyn Clock>,
    idle: Duration,
    /// Jobs accepted and not yet finished, across every lane. A `watch` rather than
    /// a counter so [`Lanes::quiesced`] can wait on it without polling.
    inflight: Arc<watch::Sender<usize>>,
}

impl std::fmt::Debug for Lanes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lanes")
            .field("active", &self.active())
            .field("inflight", &self.inflight())
            .field("idle", &self.idle)
            .finish_non_exhaustive()
    }
}

impl Lanes {
    /// Lanes reaping after `idle`, timing off `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, idle: Duration) -> Self {
        Self {
            registry: Arc::new(Mutex::new(HashMap::new())),
            clock,
            idle,
            inflight: Arc::new(watch::channel(0).0),
        }
    }

    /// Run `op` on `slug`'s lane and wait for its result.
    ///
    /// `op` is synchronous by design — it is a `grove-ops` call — and runs on a
    /// blocking thread, one at a time per root. The future may be dropped (a client
    /// disconnects, a `timeout` fires) without cancelling the op: a git mutation
    /// half-applied because its caller lost interest is exactly the class of damage
    /// grove refuses to do, so the work always runs to completion and only the
    /// result is discarded.
    ///
    /// [`Priority::Read`] is the one exception, and only because it earns it: a read
    /// writes nothing, so skipping one whose caller has gone is indistinguishable from
    /// running it and discarding the answer — except in the lane slot it hands back to
    /// work someone is still waiting on.
    pub async fn run<T, F>(&self, slug: &str, priority: Priority, op: F) -> Result<T, LaneError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        // A read is the one op it is safe to skip once its caller has gone: it writes
        // nothing, so not running it is indistinguishable from running it and throwing
        // the answer away — except in the lane slot it hands back. Never for a
        // mutation (see the module doc).
        let abandonable = priority == Priority::Read;
        self.submit(
            slug,
            priority,
            Box::new(move || {
                if abandonable && tx.is_closed() {
                    return;
                }
                let _ = tx.send(op());
            }),
        )?;
        rx.await.map_err(|_| LaneError::Lost)
    }

    /// Enqueue one job, spawning the lane if this is its first use.
    ///
    /// The whole body runs under the registry lock — see the module doc: that is
    /// what makes the idle reap race-free. Nothing here blocks or awaits, so the
    /// lock is held for a map lookup and a channel push.
    fn submit(&self, slug: &str, priority: Priority, work: Work) -> Result<(), LaneError> {
        let job = Job::new(&self.inflight, work);
        let mut lanes = self.lock();
        let entry = lanes.entry(slug.to_owned()).or_insert_with(|| {
            spawn(
                slug.to_owned(),
                Arc::clone(&self.registry),
                Arc::clone(&self.clock),
                self.idle,
            )
        });
        // A shed or refused job is dropped here, which is what releases the slot
        // `Job::new` just took.
        match priority {
            Priority::Foreground => entry.foreground.try_send(job).map_err(shed),
            Priority::Read => entry.read.try_send(job).map_err(shed),
            Priority::Background => entry.background.send(job).map_err(|_| LaneError::Lost),
        }
    }

    /// How many lanes are alive — lazily spawned and idle-reaped, so this is the
    /// count of *recently active* roots, not declared ones.
    #[must_use]
    pub fn active(&self) -> usize {
        self.lock().len()
    }

    /// The slugs with a live lane, sorted. What a drain names in its log line when it
    /// gives up waiting.
    #[must_use]
    pub fn active_slugs(&self) -> Vec<String> {
        let mut slugs: Vec<String> = self.lock().keys().cloned().collect();
        slugs.sort();
        slugs
    }

    /// Whether `slug` currently has a lane.
    #[must_use]
    pub fn is_active(&self, slug: &str) -> bool {
        self.lock().contains_key(slug)
    }

    /// Jobs accepted and not yet finished, across every lane.
    #[must_use]
    pub fn inflight(&self) -> usize {
        *self.inflight.borrow()
    }

    /// Resolve once no job is queued or running anywhere — immediately, if none is.
    ///
    /// The drain's half of carried law 11: a daemon may be killed at any time, but a
    /// *graceful* stop should let the `git worktree add` it started finish rather
    /// than return and leave the process blocking on runtime drop with nothing said.
    /// Callers bound it — see [`crate::Daemon::serve`].
    pub async fn quiesced(&self) {
        let mut rx = self.inflight.subscribe();
        while *rx.borrow_and_update() > 0 {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// A poisoned registry is not a reason to stop serving: the guarded value is a
    /// map of channel handles, so a panicking holder cannot have left it half-built,
    /// and refusing every subsequent op would turn one panic into a dead daemon.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, LaneEntry>> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How a bounded queue's rejection reads to the caller.
///
/// Consuming, not borrowing: the rejected [`Job`] travels back inside the error, and
/// dropping it here is what returns the in-flight slot [`Job::new`] took on the way
/// in. A shed that leaked its slot would leave every later drain convinced work was
/// still outstanding.
fn shed(e: mpsc::error::TrySendError<Job>) -> LaneError {
    match e {
        mpsc::error::TrySendError::Full(job) => {
            drop(job);
            LaneError::Busy
        }
        mpsc::error::TrySendError::Closed(job) => {
            drop(job);
            LaneError::Lost
        }
    }
}

/// Start a lane task and return the handles that address it.
fn spawn(slug: String, registry: Registry, clock: Arc<dyn Clock>, idle: Duration) -> LaneEntry {
    let (fg_tx, fg_rx) = mpsc::channel(FOREGROUND_DEPTH);
    let (read_tx, read_rx) = mpsc::channel(READ_DEPTH);
    let (bg_tx, bg_rx) = mpsc::unbounded_channel();
    tokio::spawn(run_lane(Lane {
        slug,
        registry,
        clock,
        idle,
        foreground: fg_rx,
        read: read_rx,
        background: bg_rx,
    }));
    LaneEntry {
        foreground: fg_tx,
        read: read_tx,
        background: bg_tx,
    }
}

/// One lane's own state: its identity, its three queues, and what it reaps against.
struct Lane {
    slug: String,
    registry: Registry,
    clock: Arc<dyn Clock>,
    idle: Duration,
    foreground: mpsc::Receiver<Job>,
    read: mpsc::Receiver<Job>,
    background: mpsc::UnboundedReceiver<Job>,
}

/// The lane itself: drain foreground, then read, then background, one op at a time;
/// when all three are empty, wait for work or for the idle deadline.
async fn run_lane(mut lane: Lane) {
    loop {
        // Anything already queued runs before we consider waiting, in priority order.
        // `try_recv` rather than a `select!` because `select!` over several ready
        // branches is only *biased* toward the first — this is the strict order.
        let queued = lane
            .foreground
            .try_recv()
            .ok()
            .or_else(|| lane.read.try_recv().ok())
            .or_else(|| lane.background.try_recv().ok());

        let job = if let Some(job) = queued {
            job
        } else {
            let deadline = lane.clock.deadline(lane.idle);
            tokio::select! {
                biased;
                job = lane.foreground.recv() => match job {
                    Some(job) => job,
                    None => break,
                },
                job = lane.read.recv() => match job {
                    Some(job) => job,
                    None => break,
                },
                job = lane.background.recv() => match job {
                    Some(job) => job,
                    None => break,
                },
                () = wait::until(deadline, &*lane.clock) => {
                    if reap(&lane) {
                        break;
                    }
                    continue;
                }
            }
        };

        // One op at a time, on a blocking thread. A panic inside grove-ops drops the
        // caller's oneshot — which reads as `LaneError::Lost` — and the lane keeps
        // serving: one bad op must not take the root's serialization with it.
        if let Err(e) = tokio::task::spawn_blocking(move || job.call()).await {
            tracing::warn!(slug = %lane.slug, error = %e, "a lane op did not complete");
        }
    }
    tracing::debug!(slug = %lane.slug, "lane reaped");
}

/// Retire this lane if it is still idle, under the registry lock.
///
/// Returns whether the lane unregistered and may exit. The emptiness re-check and
/// the unregister are one critical section with [`Lanes::submit`]'s lookup-and-
/// enqueue, which is what makes a re-ensure unable to address a dying lane.
fn reap(lane: &Lane) -> bool {
    let mut lanes = lane.registry.lock().unwrap_or_else(PoisonError::into_inner);
    if !lane.foreground.is_empty() || !lane.read.is_empty() || !lane.background.is_empty() {
        return false;
    }
    lanes.remove(&lane.slug);
    true
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_IDLE, FOREGROUND_DEPTH, LaneError, Lanes, Priority, READ_DEPTH};
    use grove_ops::clock::{SystemClock, TestClock};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn lanes() -> Lanes {
        Lanes::new(Arc::new(SystemClock), DEFAULT_IDLE)
    }

    /// A lane appears on first use and answers.
    #[tokio::test]
    async fn a_lane_spawns_lazily_and_runs_the_op() {
        let lanes = lanes();
        assert_eq!(lanes.active(), 0);

        let answer = lanes
            .run("o/r", Priority::Foreground, || 41 + 1)
            .await
            .unwrap();

        assert_eq!(answer, 42);
        assert!(lanes.is_active("o/r"));
        assert_eq!(lanes.active(), 1, "one root, one lane");
    }

    /// The invariant itself: two ops on one root never overlap, and they run in the
    /// order they were submitted. The first op parks until the test releases it, so
    /// an implementation that ran them concurrently would record an overlap.
    #[tokio::test]
    async fn two_ops_on_one_root_run_strictly_in_order() {
        let lanes = Arc::new(lanes());
        let live = Arc::new(AtomicUsize::new(0));
        let overlaps = Arc::new(AtomicUsize::new(0));
        let (release, held) = oneshot::channel::<()>();

        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for (n, gate) in [(0, Some(held)), (1, None)] {
            let (lanes, live, overlaps, order) = (
                Arc::clone(&lanes),
                Arc::clone(&live),
                Arc::clone(&overlaps),
                Arc::clone(&order),
            );
            handles.push(tokio::spawn(async move {
                lanes
                    .run("o/r", Priority::Foreground, move || {
                        if live.fetch_add(1, Ordering::SeqCst) != 0 {
                            overlaps.fetch_add(1, Ordering::SeqCst);
                        }
                        // The first op blocks its lane until released — on a
                        // blocking thread, which is where lane ops run.
                        if let Some(gate) = gate {
                            let _ = gate.blocking_recv();
                        }
                        order.lock().unwrap().push(n);
                        live.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                    .unwrap();
            }));
            // Submission order is the queue order, so the two spawns are sequenced.
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        release.send(()).unwrap();
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(overlaps.load(Ordering::SeqCst), 0, "the ops overlapped");
        assert_eq!(*order.lock().unwrap(), vec![0, 1], "out of order");
    }

    /// …and the other half of the law: two roots are not serialized against each
    /// other. The second op only completes because the first is still parked.
    #[tokio::test]
    async fn ops_on_two_roots_interleave() {
        let lanes = Arc::new(lanes());
        let (release, held) = oneshot::channel::<()>();
        let first = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move {
                lanes
                    .run("o/one", Priority::Foreground, move || {
                        let _ = held.blocking_recv();
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        let other = tokio::time::timeout(
            Duration::from_secs(5),
            lanes.run("o/two", Priority::Foreground, || "done"),
        )
        .await
        .expect("a second root is not blocked by the first")
        .unwrap();

        assert_eq!(other, "done");
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
    }

    /// Foreground drains ahead of background — but never by preempting the op
    /// already running. The lane is held by a background op; the foreground and
    /// background jobs queued behind it come out foreground-first.
    #[tokio::test]
    async fn foreground_drains_first_without_preempting() {
        let lanes = Arc::new(lanes());
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release, held) = oneshot::channel::<()>();

        let holder = {
            let (lanes, order) = (Arc::clone(&lanes), Arc::clone(&order));
            tokio::spawn(async move {
                lanes
                    .run("o/r", Priority::Background, move || {
                        let _ = held.blocking_recv();
                        order.lock().unwrap().push("in-flight");
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Queue background *first*, then foreground: the order they come out in is
        // the priority's doing, not arrival order.
        let queued: Vec<_> = [Priority::Background, Priority::Foreground]
            .into_iter()
            .map(|priority| {
                let (lanes, order) = (Arc::clone(&lanes), Arc::clone(&order));
                let label = if priority == Priority::Foreground {
                    "fg"
                } else {
                    "bg"
                };
                tokio::spawn(async move {
                    lanes
                        .run("o/r", priority, move || order.lock().unwrap().push(label))
                        .await
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(20)).await;

        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        for handle in queued {
            handle.await.unwrap().unwrap();
        }
        assert_eq!(
            *order.lock().unwrap(),
            vec!["in-flight", "fg", "bg"],
            "the in-flight op finished first, then foreground before background"
        );
    }

    /// The foreground bound sheds rather than growing: one op holds the lane, the
    /// queue fills to its depth, and the next submission is `Busy`.
    #[tokio::test]
    async fn a_full_foreground_queue_answers_busy() {
        let lanes = Arc::new(lanes());
        let (release, held) = oneshot::channel::<()>();
        let holder = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move {
                lanes
                    .run("o/r", Priority::Foreground, move || {
                        let _ = held.blocking_recv();
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut queued = Vec::new();
        for _ in 0..FOREGROUND_DEPTH {
            let lanes = Arc::clone(&lanes);
            queued.push(tokio::spawn(async move {
                lanes.run("o/r", Priority::Foreground, || ()).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            lanes.run("o/r", Priority::Foreground, || ()).await,
            Err(LaneError::Busy),
            "the {FOREGROUND_DEPTH}-deep foreground queue must shed, not grow"
        );
        // Background is unbounded — coalesced desired state cannot pile up, and a
        // shed there would drop convergence nobody is waiting to retry.
        let background = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move { lanes.run("o/r", Priority::Background, || ()).await })
        };

        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        for handle in queued {
            handle.await.unwrap().unwrap();
        }
        background.await.unwrap().unwrap();
    }

    /// A panicking op costs its caller a result — never the lane. The next op on the
    /// same root still runs.
    #[tokio::test]
    async fn a_panicking_op_loses_its_result_and_leaves_the_lane_serving() {
        let lanes = lanes();
        assert_eq!(
            lanes
                .run("o/r", Priority::Foreground, || panic!("grove-ops exploded"))
                .await,
            Err(LaneError::Lost)
        );
        assert_eq!(
            lanes.run("o/r", Priority::Foreground, || "alive").await,
            Ok("alive")
        );
    }

    /// The idle reap, on the fake clock: a lane that has done its work goes away
    /// once the idle budget passes, and the *next* op brings a fresh one back.
    #[tokio::test]
    async fn an_idle_lane_reaps_itself_and_respawns_on_demand() {
        let clock = Arc::new(TestClock::new());
        let lanes = Lanes::new(clock.clone(), Duration::from_secs(1));
        lanes.run("o/r", Priority::Foreground, || ()).await.unwrap();
        assert!(lanes.is_active("o/r"));

        advance_until(&clock, Duration::from_secs(1), || !lanes.is_active("o/r")).await;

        assert_eq!(lanes.active(), 0, "the idle lane retired");
        lanes.run("o/r", Priority::Foreground, || ()).await.unwrap();
        assert!(lanes.is_active("o/r"), "and a later op brings one back");
    }

    /// The race the reap exists to survive (v1's undeclare→redeclare): a submission
    /// arriving as the idle deadline fires is served, never handed to a lane that
    /// has decided to exit. Both halves take the registry lock, so the two orders
    /// are the only ones — and this hammers the window from both sides.
    ///
    /// **A real clock and a zero idle budget**, not a hand-wound one. `wait::until`
    /// only re-reads an injected clock every [`crate::wait::TICK`] (50 ms real), so a
    /// `TestClock` version of this test loses the race by ~50 ms *every* iteration:
    /// the submit always wins, the reap never fires, and the test passes against an
    /// implementation with no registry locking at all. With `idle` at zero the lane
    /// decides to die the instant its queues drain, which is the window itself —
    /// measured at 0/50 iterations entered under the fake clock, and hit thousands of
    /// times here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_submission_racing_the_reap_is_still_served() {
        const TASKS: usize = 16;
        const OPS: usize = 400;
        let slugs = ["o/a", "o/b", "o/c", "o/d"];

        let lanes = Arc::new(Lanes::new(Arc::new(SystemClock), Duration::ZERO));
        let mut hammers = Vec::new();
        for task in 0..TASKS {
            let lanes = Arc::clone(&lanes);
            hammers.push(tokio::spawn(async move {
                for op in 0..OPS {
                    let slug = slugs[(task + op) % slugs.len()];
                    // Both bounded tiers and the unbounded one, so the reap's
                    // emptiness re-check is raced on every queue it consults.
                    let priority = match op % 3 {
                        0 => Priority::Foreground,
                        1 => Priority::Read,
                        _ => Priority::Background,
                    };
                    assert_eq!(
                        lanes.run(slug, priority, move || op).await,
                        Ok(op),
                        "{slug} lost an op to the reap"
                    );
                }
            }));
        }
        for hammer in hammers {
            hammer.await.unwrap();
        }

        assert_eq!(lanes.inflight(), 0, "every op was accounted for");
        // Nothing is left submitting, so every lane retires: with a zero idle budget
        // the last one goes as soon as it observes its own empty queues.
        for _ in 0..200 {
            if lanes.active() == 0 {
                return;
            }
            tokio::time::sleep(crate::wait::TICK).await;
        }
        panic!("a lane outlived its work: {:?}", lanes.active_slugs());
    }

    /// Reads shed at their own shallow depth, and **a mutation still gets in**. This
    /// is the whole point of the third tier: a polling dashboard filling a cloning
    /// root's queue must not make `POST /api/roots/remove` answer "this root is
    /// wedged".
    #[tokio::test]
    async fn a_read_flood_sheds_without_taking_the_mutation_queue() {
        let lanes = Arc::new(lanes());
        let (release, held) = oneshot::channel::<()>();
        let holder = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move {
                lanes
                    .run("o/r", Priority::Foreground, move || {
                        let _ = held.blocking_recv();
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Far more reads than the read tier can hold — the polling UI.
        let mut reads = Vec::new();
        for _ in 0..READ_DEPTH * 8 {
            let lanes = Arc::clone(&lanes);
            reads.push(tokio::spawn(async move {
                lanes.run("o/r", Priority::Read, || ()).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            lanes.run("o/r", Priority::Read, || ()).await,
            Err(LaneError::Busy),
            "the read tier sheds at its own depth"
        );
        // …and the mutation is admitted, not shed, however many reads arrived.
        let mutation = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move { lanes.run("o/r", Priority::Foreground, || "removed").await })
        };

        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        assert_eq!(mutation.await.unwrap(), Ok("removed"));
        for read in reads {
            // A shed read is `Busy`, a served one is `Ok` — neither may be `Lost`.
            assert_ne!(read.await.unwrap(), Err(LaneError::Lost));
        }
    }

    /// A read whose caller has given up is dropped at the head of the lane. The lane
    /// is held while the read's future is dropped, so the job is definitely queued
    /// when its waiter goes away.
    #[tokio::test]
    async fn an_abandoned_read_is_not_run() {
        let lanes = Arc::new(lanes());
        let ran = Arc::new(AtomicUsize::new(0));
        let (release, held) = oneshot::channel::<()>();
        let holder = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move {
                lanes
                    .run("o/r", Priority::Foreground, move || {
                        let _ = held.blocking_recv();
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        {
            let counted = Arc::clone(&ran);
            let read = lanes.run("o/r", Priority::Read, move || {
                counted.fetch_add(1, Ordering::SeqCst);
            });
            // Give up on it, exactly as `wait::within` does at the snapshot budget.
            assert!(
                tokio::time::timeout(Duration::from_millis(50), read)
                    .await
                    .is_err()
            );
        }

        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        // A later op proves the lane drained past the abandoned read.
        lanes.run("o/r", Priority::Read, || ()).await.unwrap();
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "a read nobody is waiting for must not spend a lane slot"
        );
    }

    /// A *mutation* whose caller gave up still runs: a git write half-applied because
    /// a client disconnected is the damage grove refuses to do.
    #[tokio::test]
    async fn an_abandoned_mutation_still_runs() {
        let lanes = Arc::new(lanes());
        let ran = Arc::new(AtomicUsize::new(0));
        let (release, held) = oneshot::channel::<()>();
        let holder = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move {
                lanes
                    .run("o/r", Priority::Foreground, move || {
                        let _ = held.blocking_recv();
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        {
            let counted = Arc::clone(&ran);
            let mutation = lanes.run("o/r", Priority::Foreground, move || {
                counted.fetch_add(1, Ordering::SeqCst);
            });
            assert!(
                tokio::time::timeout(Duration::from_millis(50), mutation)
                    .await
                    .is_err()
            );
        }

        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        lanes.run("o/r", Priority::Foreground, || ()).await.unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the mutation still applied");
    }

    /// Quiescence is what a bounded drain waits on: it is immediate when nothing is
    /// in flight, and it does not resolve while a job is still running.
    #[tokio::test]
    async fn quiescence_reports_work_still_in_flight() {
        let lanes = Arc::new(lanes());
        tokio::time::timeout(Duration::from_secs(5), lanes.quiesced())
            .await
            .expect("an idle registry is already quiescent");

        let (release, held) = oneshot::channel::<()>();
        let job = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move {
                lanes
                    .run("o/r", Priority::Background, move || {
                        let _ = held.blocking_recv();
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(lanes.inflight(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), lanes.quiesced())
                .await
                .is_err(),
            "a drain must not call a running git write finished"
        );

        release.send(()).unwrap();
        job.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), lanes.quiesced())
            .await
            .expect("the job landed");
        assert_eq!(lanes.inflight(), 0);
    }

    /// A shed job releases the slot it took on the way in — otherwise a busy minute
    /// would leave the drain permanently convinced work was outstanding.
    #[tokio::test]
    async fn a_shed_job_is_not_counted_as_in_flight() {
        let lanes = Arc::new(lanes());
        let (release, held) = oneshot::channel::<()>();
        let holder = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move {
                lanes
                    .run("o/r", Priority::Read, move || {
                        let _ = held.blocking_recv();
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut queued = Vec::new();
        for _ in 0..READ_DEPTH {
            let lanes = Arc::clone(&lanes);
            queued.push(tokio::spawn(async move {
                lanes.run("o/r", Priority::Read, || ()).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let before = lanes.inflight();
        assert_eq!(
            lanes.run("o/r", Priority::Read, || ()).await,
            Err(LaneError::Busy)
        );
        assert_eq!(
            lanes.inflight(),
            before,
            "the shed job took no lasting slot"
        );

        release.send(()).unwrap();
        holder.await.unwrap().unwrap();
        for handle in queued {
            handle.await.unwrap().unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), lanes.quiesced())
            .await
            .expect("everything accepted eventually landed");
    }

    /// Step the fake clock until `condition` holds.
    ///
    /// Advancing in a loop, rather than once, because the lane arms its idle
    /// deadline when it *observes* an empty queue — which may be after a single
    /// `advance` has already happened, leaving a deadline in a future the clock
    /// never reaches. Real time has no such race; a hand-wound clock does, and the
    /// loop is the price of not waiting a real minute. The reap itself is observed
    /// rather than awaited: nothing in the public surface signals "a lane exited",
    /// and adding a channel for a test to watch would be a production seam that
    /// exists only for the test.
    async fn advance_until(clock: &TestClock, step: Duration, condition: impl Fn() -> bool) {
        for _ in 0..200 {
            if condition() {
                return;
            }
            clock.advance(step);
            tokio::time::sleep(crate::wait::TICK).await;
        }
        panic!("condition never held");
    }
}
