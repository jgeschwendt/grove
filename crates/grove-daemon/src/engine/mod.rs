//! The per-root engine: one driver task per declared root, owning that root's
//! status cache, its single background slot, and its pool hint.
//!
//! Files stay authoritative — `manifest.toml` is desired state, git on disk is
//! actual state. **Status is a cache and nothing else**: it is never persisted, and a
//! restarted daemon re-derives every root's status from disk (carried law 11,
//! reactivation = cold boot). What a *running* engine knows that disk cannot say is
//! the transient: `cloning` and `degraded` are facts about work in flight or a
//! failure just recorded, and only the process driving the root has them. That, and
//! nothing else, is why the engine is resident.
//!
//! ## The single background slot
//!
//! At most one of reconcile / sync / fill is in flight per root, chosen in that
//! priority order, and every condition is **level-triggered state** re-evaluated on
//! each completion:
//!
//! - a bg op in flight ⇒ the driver does nothing; the next op is picked when this
//!   one lands. That is what collapses a burst of manifest events into one run and
//!   makes a double-clone unrepresentable — not a lock, not a de-dupe cache.
//! - `reconcile_pending` (set at start and on every roots-changed) ⇒ dispatch
//!   `root.reconcile` **plus** the declared `pool.size`, folded into one lane job as
//!   v1 folded them, so the manifest read never queues separately behind the clone.
//! - `sync_pending` **and** `ready` ⇒ dispatch `root.sync`. Syncing an unrealized
//!   root is meaningless, so the flag simply waits for a later drive to find it
//!   ready.
//! - `ready` and the pool off its declared target ⇒ dispatch `pool.fill`, which
//!   adds a slot below target and reclaims one above it.
//!
//! Reconcile is re-dispatched on *every* manifest change even for a realized root:
//! the op is idempotent and worktrees may have changed out of band.
//!
//! ## No timers, anywhere
//!
//! Convergence is push-based (carried law 8). A failure degrades one root, logs, and
//! waits — the next event is the retry. There is no retry timer, no poll, no
//! periodic sweep in this file. The only deadlines in the daemon are the lane's idle
//! reap and the watcher's debounce, and both are budgets on an event, not a cadence.
//!
//! ## Nothing blocking runs in the driver
//!
//! Every grove-ops call leaves the driver as a task, runs on the root's lane, and
//! comes back as a message. The driver's own loop only ever awaits its mailbox, so
//! `status` keeps answering while a clone drags for minutes — v1's invariant "no
//! foreground ops call inside an engine callback", carried.

mod set;
mod status;

use std::path::PathBuf;
use std::sync::Arc;

use grove_api::events::{Event, TaskKind, TaskOutcome};
use grove_api::policy::{
    ErrorDisposition, RootDisposition, classify_error, classify_reconcile, is_fast_forward_note,
};
use grove_api::status::{RootStatus, SyncNote};
use grove_ops::clock::Clock;
use grove_ops::roots::{Applied, SyncReport};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::events::EventBus;
use crate::lane::{LaneError, Lanes, Priority};

pub use set::RootSet;
pub use status::{DiskStatus, Transition, disk_status, next_status};

/// How many reconciles may hold a clone at once, across every root.
///
/// v1 designed this and YAGNI'd it: a daemon booting a home with N declared roots
/// fanned out N concurrent clones, each a full network fetch.
///
/// A permit is **tried** at the head of the root's own lane and never waited for
/// there — see [`Driver::dispatch`] for both halves of that. Either way the root's
/// status is honest while it waits: `cloning` is set when the reconcile is
/// dispatched, not when it reaches a permit.
pub const DEFAULT_CLONE_LIMIT: usize = 4;

/// What the dashboard's sync surface reads: is one pending or running, and what did
/// the last one leave behind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncInfo {
    pub syncing: bool,
    pub note: Option<SyncNote>,
}

/// Why an engine could not answer.
#[derive(Debug)]
pub enum EngineError {
    /// The driver is gone — the root was undeclared, or the daemon is shutting down.
    NoEngine,
    /// The root's lane refused or lost the op.
    Lane(LaneError),
    /// The op ran and failed.
    Ops(grove_ops::Error),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEngine => f.write_str("no engine is running for this root"),
            Self::Lane(e) => write!(f, "{e}"),
            Self::Ops(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// Everything an engine needs from the daemon around it. One value, cloned per
/// engine — the fields are all handles.
#[derive(Clone)]
pub struct Deps {
    pub home: PathBuf,
    pub lanes: Arc<Lanes>,
    pub events: EventBus,
    pub clock: Arc<dyn Clock>,
    /// The dispatch-side clone bound, shared across every root.
    pub clones: Arc<Semaphore>,
}

impl std::fmt::Debug for Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps")
            .field("home", &self.home)
            .field("clones", &self.clones.available_permits())
            .finish_non_exhaustive()
    }
}

/// A handle on one root's engine. Cloning it addresses the same driver.
#[derive(Clone, Debug)]
pub struct Engine {
    slug: Arc<str>,
    tx: mpsc::UnboundedSender<Msg>,
}

/// The driver's mailbox vocabulary.
enum Msg {
    /// The manifest changed (or was re-announced): re-derive, mark a reconcile.
    RootsChanged,
    Status(oneshot::Sender<RootStatus>),
    SyncInfo(oneshot::Sender<SyncInfo>),
    /// Accept-only: set the flag, kick the driver, answer at once.
    Sync(oneshot::Sender<()>),
    /// A clone permit came free somewhere; re-drive a reconcile that deferred.
    PermitFreed,
    BgDone(TaskKind, BgOutcome, Option<OwnedSemaphorePermit>),
    Stop,
}

/// What a background task brings back. `Crashed` is v1's `:DOWN` on a bg task: the
/// op produced no result at all, which is a different fact from an op that failed.
enum BgOutcome {
    Reconcile(Result<Applied, grove_ops::Error>, Option<PoolRead>),
    /// The reconcile never ran: no clone permit was free, and the lane was released
    /// rather than held while one came available. Re-armed, not failed.
    Deferred,
    Sync(Result<SyncReport, grove_ops::Error>),
    Fill(Result<usize, grove_ops::Error>),
    Crashed,
}

/// What a reconcile learned about the root's warm pool while it held the lane: the
/// declared target, and what is actually on disk.
///
/// Both, not just the target: the observed count is the driver's convergence input
/// and a reconcile is where a slot gets *claimed* (a declared-but-missing worktree
/// promotes one), so reading only the target leaves the hint stale-high and starves
/// the refill that should follow.
#[derive(Clone, Copy, Debug)]
struct PoolRead {
    target: u32,
    observed: usize,
}

impl Engine {
    /// Start a driver for `slug` and return its handle.
    ///
    /// The engine begins `unknown` with a reconcile pending and derives from disk in
    /// its first loop iteration — never inside `start`, so a caller starting a dozen
    /// engines is not serialized behind a dozen filesystem reads.
    #[must_use]
    pub fn start(deps: Deps, slug: impl Into<String>) -> Self {
        let slug: Arc<str> = Arc::from(slug.into());
        let (tx, rx) = mpsc::unbounded_channel();
        let driver = Driver {
            slug: Arc::clone(&slug),
            deps,
            status: RootStatus::Unknown,
            target: 0,
            pool: 0,
            bg: None,
            fill_blocked: false,
            reconcile_pending: true,
            sync_pending: false,
            sync_note: None,
            tx: tx.clone(),
        };
        tokio::spawn(driver.run(rx));
        Self { slug, tx }
    }

    /// The root this engine drives.
    #[must_use]
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// Tell the engine the declared set changed. Fire-and-forget: the engine
    /// re-derives and re-dispatches a reconcile on its own schedule.
    pub fn roots_changed(&self) {
        let _ = self.tx.send(Msg::RootsChanged);
    }

    /// This root's cached status.
    pub async fn status(&self) -> Result<RootStatus, EngineError> {
        self.ask(Msg::Status).await
    }

    /// Whether a sync is pending or running, and the last one's note.
    pub async fn sync_info(&self) -> Result<SyncInfo, EngineError> {
        self.ask(Msg::SyncInfo).await
    }

    /// Request a sync. **Accept-only** (invariant `push-only`): this returns as soon
    /// as the request is recorded, never when the fetch finishes. Completion arrives
    /// as [`Event::RootSyncChanged`] on the bus, and a re-request while one is
    /// pending or in flight coalesces into it.
    ///
    /// Recorded is not scheduled: the driver dispatches a pending sync only from
    /// `ready`, so the completion broadcast follows the root reaching it. A
    /// `degraded` root — which the engine leaves only on a dispatched reconcile —
    /// holds the request, and `syncing`, until one arrives.
    pub async fn sync(&self) -> Result<(), EngineError> {
        self.ask(Msg::Sync).await
    }

    /// Stop the driver.
    ///
    /// Explicit rather than "drop the last handle": a background task holds a sender
    /// so it can report its outcome, so an undeclared root would otherwise stay alive
    /// for the length of its in-flight clone. In-flight work still runs to completion
    /// on the lane — it simply reports to a mailbox nobody reads.
    pub fn stop(&self) {
        let _ = self.tx.send(Msg::Stop);
    }

    async fn ask<T>(&self, msg: impl FnOnce(oneshot::Sender<T>) -> Msg) -> Result<T, EngineError> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(msg(tx)).map_err(|_| EngineError::NoEngine)?;
        rx.await.map_err(|_| EngineError::NoEngine)
    }
}

/// The driver's state. Owned by exactly one task, so none of it is behind a lock:
/// every read and write happens in the message loop below.
struct Driver {
    slug: Arc<str>,
    deps: Deps,
    status: RootStatus,
    /// The declared warm-slot target, re-read by every reconcile task.
    target: u32,
    /// The observed warm-slot count — **a hint, not truth**. The one cache in a
    /// files-authoritative system, and audited as such: `pool.fill` re-observes disk
    /// before mutating, so a stale hint costs at most one no-op fill, never an
    /// over-fill.
    pool: usize,
    bg: Option<TaskKind>,
    /// A pool converge failed; wait for the next real event rather than
    /// re-dispatching into the same failure on a still-true level condition.
    fill_blocked: bool,
    reconcile_pending: bool,
    sync_pending: bool,
    sync_note: Option<SyncNote>,
    tx: mpsc::UnboundedSender<Msg>,
}

impl Driver {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        // Derive before the first drive, exactly as v1 derived in `handle_continue`
        // rather than `init`: the engine is addressable the moment `start` returns,
        // and the disk read happens on this task.
        self.derive();
        self.drive();

        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::RootsChanged => {
                    self.derive();
                    self.fill_blocked = false;
                    self.reconcile_pending = true;
                    self.drive();
                }
                Msg::Status(reply) => {
                    let _ = reply.send(self.status);
                }
                Msg::SyncInfo(reply) => {
                    let _ = reply.send(SyncInfo {
                        syncing: self.sync_pending || self.bg == Some(TaskKind::Sync),
                        note: self.sync_note,
                    });
                }
                Msg::Sync(reply) => {
                    self.sync_pending = true;
                    self.drive();
                    // Broadcast on *accept*, so every attached view shows the
                    // in-flight state immediately — not only the client that asked.
                    self.publish_sync_changed();
                    let _ = reply.send(());
                }
                Msg::PermitFreed => self.drive(),
                Msg::BgDone(kind, outcome, permit) => self.bg_done(kind, outcome, permit),
                Msg::Stop => break,
            }
        }
        tracing::debug!(slug = %self.slug, status = %self.status, "engine stopped");
    }

    // ── the driver ──────────────────────────────────────────────────────────────

    /// Pick the next background op, or leave the slot alone.
    // stele:landmark push-only
    fn drive(&mut self) {
        if self.bg.is_some() {
            return;
        }
        if self.reconcile_pending {
            self.reconcile_pending = false;
            self.status = next_status(self.status, Transition::ReconcileDispatched);
            self.dispatch(TaskKind::Reconcile);
            return;
        }
        if self.sync_pending && self.status == RootStatus::Ready {
            self.sync_pending = false;
            self.dispatch(TaskKind::Sync);
            return;
        }
        // Both directions: below target, add a slot; above it — a lowered or deleted
        // `pool.size` — give one back. One-way convergence left a root that dropped
        // from 2 to 0 holding both checkouts forever, with doctor reporting `2/0` and
        // no command that could fix it.
        //
        // `fill_blocked` is the failure latch law 8 requires. Without it a fill that
        // keeps failing re-dispatches the instant it lands — the level condition is
        // still true — and the root spins on git at full speed, forever. The latch
        // clears on the next real event (a manifest change, or a reconcile/sync that
        // moved the root), which is exactly "the next event is the retry".
        if self.status == RootStatus::Ready
            && self.pool != self.target as usize
            && !self.fill_blocked
        {
            self.dispatch(TaskKind::Fill);
        }
    }

    /// Take the background slot and run `kind` on this root's lane.
    ///
    /// The slot is taken *here* — synchronously, before the task exists — which is
    /// what makes the coalescing airtight: a second trigger arriving one line later
    /// already sees `bg` occupied. The `task_started` event, by contrast, is
    /// published from inside the lane job once the work actually begins, so neither a
    /// reconcile queued behind its own root's lane nor one waiting on the clone
    /// semaphore is reported as running. The signal that a queued reconcile exists is
    /// the root's own `cloning` status, set below.
    ///
    /// ## Where a reconcile's clone permit is taken
    ///
    /// **Tried at the head of the root's own lane, and never waited for there.** Two
    /// failure modes have to be avoided at once, and only this ordering avoids both.
    ///
    /// Acquiring dispatch-side and *then* awaiting lane admission inverts the bound:
    /// a reconcile doing nothing at all — merely waiting out a foreground op on its
    /// own root — holds one of [`DEFAULT_CLONE_LIMIT`] permits against roots whose
    /// lanes are completely free. A whole-home doctor takes every root's foreground
    /// lane at once, so four such roots stalled home-wide convergence for the length
    /// of the pass.
    ///
    /// But *blocking* for a permit inside the lane job is worse, because the job
    /// holds the lane while it waits. Once the permits are all held by clones that
    /// have stalled — a peer that accepts the connection and then goes silent; a
    /// dropped VPN; a slept laptop's half-open socket — every other root's reconcile
    /// parks on the semaphore holding its own lane, and each of those roots' snapshot
    /// reads are shed as `unavailable`. The whole home then reports unavailable and
    /// converges nothing, while `/api/health` still says `ready`. Damage that was
    /// meant to be local to a busy root became the daemon's.
    ///
    /// So the job takes the permit with `try_acquire` and, failing, returns
    /// [`BgOutcome::Deferred`] immediately: the lane is released, reads answer again,
    /// and the reconcile is re-armed. A waiter task then awaits a permit off the lane
    /// purely as a wake-up — it drops the permit it got and pokes the driver, which
    /// re-dispatches into the same try. No timer, and no permit is held by anything
    /// that is not cloning.
    ///
    /// The permit travels back to the driver with the result, so it outlives
    /// `task_finished` (see [`Driver::bg_done`]). Releasing it when the lane job ends
    /// instead would let the *next* reconcile announce itself before the previous
    /// one's completion was published — the bound would still hold, but nothing
    /// watching the stream could prove it.
    fn dispatch(&mut self, kind: TaskKind) {
        self.bg = Some(kind);

        let (home, slug) = (self.deps.home.clone(), self.slug.to_string());
        let lanes = Arc::clone(&self.deps.lanes);
        let clones = Arc::clone(&self.deps.clones);
        let events = self.deps.events.clone();
        let target = self.target;
        let tx = self.tx.clone();

        tokio::spawn(async move {
            let outcome = match kind {
                TaskKind::Reconcile => {
                    let job = {
                        let (home, slug, events) = (home.clone(), slug.clone(), events.clone());
                        let clones = Arc::clone(&clones);
                        move || {
                            let Ok(permit) = clones.try_acquire_owned() else {
                                return None;
                            };
                            started(&events, &slug, kind);
                            Some((
                                grove_ops::roots::reconcile_one(&home, &slug),
                                // Folded into the same lane job as v1 folded it: the
                                // declared target is a manifest read that would
                                // otherwise queue behind the very clone it follows.
                                pool_read(&home, &slug),
                                permit,
                            ))
                        }
                    };
                    match lanes.run(&slug, Priority::Background, job).await {
                        Ok(Some((applied, pool, permit))) => {
                            return finish(
                                &tx,
                                kind,
                                BgOutcome::Reconcile(applied, pool),
                                Some(permit),
                            );
                        }
                        // No permit was free. The lane is already released; wait for
                        // one off-lane, then poke the driver to try again.
                        Ok(None) => {
                            spawn_permit_waiter(clones, tx.clone());
                            BgOutcome::Deferred
                        }
                        Err(_) => BgOutcome::Crashed,
                    }
                }
                TaskKind::Sync => {
                    let job = {
                        let (home, slug, events) = (home.clone(), slug.clone(), events.clone());
                        move || {
                            started(&events, &slug, kind);
                            grove_ops::roots::sync(&home, &slug)
                        }
                    };
                    match lanes.run(&slug, Priority::Background, job).await {
                        Ok(report) => BgOutcome::Sync(report),
                        Err(_) => BgOutcome::Crashed,
                    }
                }
                TaskKind::Fill => {
                    let job = {
                        let (home, slug, events) = (home.clone(), slug.clone(), events.clone());
                        // Re-observe disk before mutating: the count the driver holds
                        // is a hint, and a fill that is already at target must be a
                        // no-op rather than an over-fill (or an over-reclaim).
                        move || {
                            started(&events, &slug, kind);
                            let observed = grove_ops::worktrees::pool_count(&home, &slug)?;
                            match observed.cmp(&(target as usize)) {
                                std::cmp::Ordering::Less => grove_ops::pool::fill(&home, &slug),
                                std::cmp::Ordering::Greater => {
                                    grove_ops::pool::reclaim(&home, &slug)
                                }
                                std::cmp::Ordering::Equal => Ok(observed),
                            }
                        }
                    };
                    match lanes.run(&slug, Priority::Background, job).await {
                        Ok(count) => BgOutcome::Fill(count),
                        Err(_) => BgOutcome::Crashed,
                    }
                }
            };
            finish(&tx, kind, outcome, None);
        });
    }

    /// A background op landed: free the slot, apply the outcome, re-drive.
    ///
    /// `permit` is a reconcile's clone permit, held for exactly as long as this
    /// function runs. Dropping it here — after `task_finished` is published — is what
    /// makes the clone bound observable from the event stream: a started/finished
    /// pair brackets a held permit exactly.
    fn bg_done(
        &mut self,
        kind: TaskKind,
        outcome: BgOutcome,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        self.bg = None;
        // A deferred reconcile never ran: nothing to report, nothing to publish. It
        // is re-armed and the driver waits for the permit waiter's poke.
        if matches!(outcome, BgOutcome::Deferred) {
            self.reconcile_pending = true;
            self.status = next_status(
                self.status,
                Transition::Derive(disk_status(&self.deps.home, &self.slug)),
            );
            return;
        }
        let result = match outcome {
            BgOutcome::Reconcile(applied, pool) => {
                // Applied first and separately, so a pool read that succeeded still
                // lands when the reconcile beside it errored.
                if let Some(pool) = pool {
                    self.target = pool.target;
                    // Truth, not a decrement: a reconcile is where a declared
                    // worktree CLAIMS a slot, so the hint would otherwise sit
                    // stale-high and starve the refill that should follow.
                    self.pool = pool.observed;
                }
                self.fill_blocked = false;
                self.apply_reconcile(applied)
            }
            BgOutcome::Sync(report) => {
                self.fill_blocked = false;
                self.sync_done(report)
            }
            BgOutcome::Fill(count) => self.fill_done(count),
            BgOutcome::Crashed => self.bg_crashed(kind),
            BgOutcome::Deferred => unreachable!("handled above"),
        };
        self.publish(Event::TaskFinished {
            slug: self.slug.to_string(),
            kind,
            outcome: result,
        });
        drop(permit);
        self.drive();
    }

    // ── reconcile ───────────────────────────────────────────────────────────────

    fn apply_reconcile(&mut self, applied: Result<Applied, grove_ops::Error>) -> TaskOutcome {
        match applied {
            Ok(applied) => {
                let (status, outcome) = match classify_reconcile(applied.status) {
                    RootDisposition::Ready => (RootStatus::Ready, TaskOutcome::Ok),
                    RootDisposition::Degraded => (RootStatus::Degraded, TaskOutcome::Failed),
                };
                if status != self.status {
                    tracing::info!(
                        slug = %self.slug, status = %status, outcome = ?applied.status,
                        "engine reconciled"
                    );
                }
                self.status = status;
                outcome
            }
            // A *terminal* fault will not heal on the next manifest event — a
            // would-clobber conflict or a malformed declaration is stuck until a
            // human edits the manifest, and that edit is itself the event that
            // re-drives us. So stop at `degraded`, where doctor can see it, instead
            // of re-deriving back to `cloning` and retrying forever.
            Err(e) if classify_error(&e) == ErrorDisposition::Terminal => {
                tracing::warn!(
                    slug = %self.slug, code = e.code(), error = %e,
                    "reconcile hit a terminal ops fault; degrading"
                );
                self.status = RootStatus::Degraded;
                TaskOutcome::Failed
            }
            // Transient (`network`/`git`/`io`, or a root undeclared out from under
            // us): not a clone *failure*. The attempt is over, so trust disk plainly
            // rather than stranding the now-stale `cloning`, and wait for the next
            // event. A genuine clone failure arrives as `Applied{status: failed}`
            // above, not here.
            Err(e) => {
                tracing::warn!(
                    slug = %self.slug, error = %e,
                    "reconcile errored; re-deriving from disk"
                );
                self.status = next_status(
                    self.status,
                    Transition::ReconcileError(disk_status(&self.deps.home, &self.slug)),
                );
                TaskOutcome::Failed
            }
        }
    }

    // ── sync ────────────────────────────────────────────────────────────────────

    fn sync_done(&mut self, report: Result<SyncReport, grove_ops::Error>) -> TaskOutcome {
        let outcome = match report {
            Ok(report) => {
                // Pruned slots leave the hint stale-high, which would starve the
                // refill guard; decrement by what the op reported.
                self.pool = self.pool.saturating_sub(report.stale_slots_pruned);
                self.sync_note = SyncNote::of(report.trunk);
                debug_assert_eq!(
                    self.sync_note.is_some(),
                    is_fast_forward_note(report.trunk),
                    "the note and the shared curation must agree"
                );
                tracing::info!(
                    slug = %self.slug, pruned = report.stale_slots_pruned,
                    "root synced"
                );
                TaskOutcome::Ok
            }
            Err(e) => {
                tracing::warn!(slug = %self.slug, error = %e, "root sync failed");
                self.sync_note = Some(SyncNote::Failed);
                TaskOutcome::Failed
            }
        };
        self.publish_sync_changed();
        outcome
    }

    // ── pool fill ───────────────────────────────────────────────────────────────

    fn fill_done(&mut self, count: Result<usize, grove_ops::Error>) -> TaskOutcome {
        match count {
            Ok(pool) => {
                self.pool = pool;
                tracing::info!(slug = %self.slug, pool, target = self.target, "pool converged");
                TaskOutcome::Ok
            }
            Err(e) => {
                // Latch, or the still-true level condition re-dispatches this the
                // instant it lands and the root spins on git forever.
                self.fill_blocked = true;
                tracing::warn!(
                    slug = %self.slug, error = %e,
                    "pool converge failed; waiting for the next event"
                );
                TaskOutcome::Failed
            }
        }
    }

    /// A background task returned nothing at all. v1's `:DOWN` handling, verbatim in
    /// consequence: a reconcile crash degrades **this root only**, a sync crash
    /// leaves a `failed` note, a fill crash is logged and forgotten.
    fn bg_crashed(&mut self, kind: TaskKind) -> TaskOutcome {
        match kind {
            TaskKind::Reconcile => {
                tracing::warn!(slug = %self.slug, "reconcile task crashed; degrading root");
                self.status = RootStatus::Degraded;
            }
            TaskKind::Sync => {
                tracing::warn!(slug = %self.slug, "root sync task crashed");
                self.sync_note = Some(SyncNote::Failed);
                self.publish_sync_changed();
            }
            TaskKind::Fill => {
                self.fill_blocked = true;
                tracing::warn!(slug = %self.slug, "pool converge task crashed");
            }
        }
        TaskOutcome::Failed
    }

    // ── status ──────────────────────────────────────────────────────────────────

    /// Re-derive from disk. Never downgrades a transient the driver owns — see
    /// [`next_status`].
    fn derive(&mut self) {
        let status = next_status(
            self.status,
            Transition::Derive(disk_status(&self.deps.home, &self.slug)),
        );
        if status != self.status {
            tracing::info!(slug = %self.slug, from = %self.status, to = %status, "engine status");
        }
        self.status = status;
    }

    fn publish(&self, event: Event) {
        self.deps.events.publish(event);
    }

    fn publish_sync_changed(&self) {
        self.publish(Event::RootSyncChanged {
            slug: self.slug.to_string(),
        });
    }
}

/// Announce that a background op is now running, from inside its own lane job — so
/// `task_started` means "this root's lane is running it", not "it is queued
/// somewhere".
fn started(events: &EventBus, slug: &str, kind: TaskKind) {
    events.publish(Event::TaskStarted {
        slug: slug.to_owned(),
        kind,
    });
}

/// Hand the outcome — and a reconcile's clone permit — back to the driver.
fn finish(
    tx: &mpsc::UnboundedSender<Msg>,
    kind: TaskKind,
    outcome: BgOutcome,
    permit: Option<OwnedSemaphorePermit>,
) {
    let _ = tx.send(Msg::BgDone(kind, outcome, permit));
}

/// Wait — **off any lane** — until a clone permit is free, then poke the driver.
///
/// The permit itself is dropped immediately: this is a wake-up, not a claim. Holding
/// it across the driver's re-dispatch would put us back where we started, waiting for
/// a lane while occupying one of the bound's slots. The re-dispatched job simply
/// tries again, and defers again if another root won the race — each round waits on a
/// real permit release, so there is no spin and no timer.
fn spawn_permit_waiter(clones: Arc<Semaphore>, tx: mpsc::UnboundedSender<Msg>) {
    tokio::spawn(async move {
        // A closed semaphore means the daemon is going away; there is nothing to
        // wake for.
        if clones.acquire().await.is_ok() {
            let _ = tx.send(Msg::PermitFreed);
        }
    });
}

/// The root's warm-pool facts, read inside the reconcile's lane job: what the
/// manifest declares and what is on disk. `None` if either read failed — the driver
/// then keeps the values it had rather than converging on a guess.
fn pool_read(home: &std::path::Path, slug: &str) -> Option<PoolRead> {
    Some(PoolRead {
        target: grove_ops::pool::size(home, slug).ok()?,
        observed: grove_ops::worktrees::pool_count(home, slug).ok()?,
    })
}
