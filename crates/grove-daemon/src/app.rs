//! The app: shared state, the router, and the bound-but-not-yet-serving [`Daemon`].

use std::future::IntoFuture as _;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use grove_ops::clock::Clock;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc, watch};

use crate::boot::BootState;
use crate::config::Config;
use crate::engine::{Deps, RootSet};
use crate::events::EventBus;
use crate::lane::Lanes;
use crate::logs::LogRing;
use crate::watcher::Watcher;
use crate::{Error, guard, routes, stream};

/// How long the shutdown route waits before draining, so the acknowledgement
/// reaches the client that asked for it. Carried from v1's 100 ms sleep.
pub const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

/// How long the whole drain has, measured from the moment the shutdown trigger
/// fires: first for open responses to finish, then for lane work to land.
///
/// **A bound, not a target.** `axum::serve`'s graceful shutdown waits for every
/// in-flight connection, and an SSE response finishes only when its body is written —
/// which never happens if the peer stopped reading (a suspended tab, a slept laptop,
/// a half-open TCP peer). Unbounded, one such connection holds `grove serve` open
/// forever, and `grove off` only ever recovers by escalating to SIGTERM, which kills
/// whatever git work was in flight. Bounded, the drain gives up on the connection and
/// then spends what is left of the budget letting the git writes land.
///
/// Sized under the CLI's own `STOP_GRACE` (10 s), so a graceful stop finishes inside
/// the window before `grove off` starts signalling.
pub const DEFAULT_DRAIN: Duration = Duration::from_secs(5);

/// Depth of the reconcile mailbox. One: a nudge is level-triggered — "converge,
/// soon" — so a second one arriving while the first is unread says nothing new. A
/// full mailbox is therefore success, not backpressure.
const NUDGE_DEPTH: usize = 1;

/// Everything a handler reads. Cheap to clone (a handful of `Arc`s and channel
/// handles); axum clones it per request.
#[derive(Clone)]
pub struct AppState {
    pub boot: Arc<BootState>,
    pub clock: Arc<dyn Clock>,
    pub config: Arc<Config>,
    /// Per-root serialization. Every git-writing route runs through it.
    pub lanes: Arc<Lanes>,
    /// The push channel: engines and the watcher publish, `GET /api/events` and the
    /// engine set subscribe.
    pub events: EventBus,
    /// The log tail `GET /api/events` streams and every snapshot carries. The daemon
    /// owns the ring; the *process* decides whether to feed it, by installing
    /// [`LogRing::layer`] on whatever subscriber it sets up (`grove serve` does).
    pub logs: Arc<LogRing>,
    /// The running engines — `None` until the engine room starts, which is why it is
    /// a `OnceLock` rather than a field: a test that claims the reconcile mailbox
    /// runs a daemon with no engines at all, and `POST /api/doctor` must answer
    /// there too (with no per-root statuses, since nothing is driving a root).
    pub engines: Arc<OnceLock<RootSet>>,
    /// The manifest watcher's trigger.
    pub reconcile: ReconcileNudge,
    pub shutdown: ShutdownTrigger,
}

/// A handle onto the reconcile mailbox — what `POST /api/roots/reconcile` pokes and
/// the manifest watcher consumes.
///
/// The route's contract is deliberately narrow: it acknowledges that convergence was
/// *scheduled*, never that it finished, so what sits on the other end of this
/// channel can change without the wire changing. A caller that claims the receiver
/// with [`Daemon::take_nudges`] gets the raw seam and no engine room.
#[derive(Clone, Debug)]
pub struct ReconcileNudge {
    tx: mpsc::Sender<()>,
}

impl ReconcileNudge {
    /// Ask for a reconcile pass. Never blocks and never fails the request: a full
    /// mailbox means one is already pending, which is the same answer.
    pub fn poke(&self) {
        match self.tx.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
            Err(mpsc::error::TrySendError::Closed(())) => {
                tracing::warn!("reconcile mailbox closed; the nudge reaches nobody");
            }
        }
    }
}

/// The graceful-shutdown signal, shared between the shutdown route and the accept
/// loop. A `watch` rather than a `Notify`: it is level-triggered, so a fire that
/// lands before the accept loop starts waiting is still observed.
#[derive(Clone, Debug)]
pub struct ShutdownTrigger {
    tx: watch::Sender<bool>,
}

impl ShutdownTrigger {
    fn new() -> Self {
        Self {
            tx: watch::channel(false).0,
        }
    }

    /// Stop accepting now.
    ///
    /// `send_replace`, not `send`: `send` fails — and leaves the value untouched —
    /// when no receiver exists yet, which is exactly the case where the fire has to
    /// stick (the shutdown route can answer before the accept loop subscribes).
    pub fn fire(&self) {
        self.tx.send_replace(true);
    }

    /// Fire after `grace`, off the request's own task so the acknowledgement is
    /// written first.
    pub fn schedule(&self, grace: Duration) {
        let trigger = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            trigger.fire();
        });
    }

    /// Resolve once [`fire`](Self::fire) has been called (immediately, if it
    /// already has).
    pub async fn wait(&self) {
        let mut rx = self.tx.subscribe();
        if *rx.borrow_and_update() {
            return;
        }
        let _ = rx.changed().await;
    }
}

/// The whole HTTP surface: ten routes, the two guards, and the fallbacks that
/// keep even a 404 inside the envelope.
///
/// Layer order is load-bearing. `Router::layer` wraps outermost-last, so readiness
/// is applied second and therefore runs **first** — matching v1's `:api` pipeline
/// (`ReadinessPlug` then `MutationGuard`): a draining server answers 503 rather than
/// spending a cross-origin verdict on a request it will not serve either way.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(routes::health))
        .route("/api/daemon/version", get(routes::version))
        .route("/api/daemon/shutdown", post(routes::shutdown))
        .route("/api/roots/reconcile", post(routes::reconcile))
        .route("/api/roots/sync", post(routes::sync))
        .route("/api/roots/remove", post(routes::remove_root))
        .route("/api/worktrees/remove", post(routes::remove_worktree))
        .route("/api/doctor", post(routes::doctor))
        // The read surface. Both answer the same shape; the stream keeps answering.
        .route("/api/roots", get(stream::roots))
        .route("/api/events", get(stream::events))
        .fallback(routes::not_found)
        .method_not_allowed_fallback(routes::method_not_allowed)
        .layer(axum::middleware::from_fn(guard::mutation))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            guard::readiness,
        ))
        .with_state(state)
}

/// A daemon holding its listener, before it serves.
///
/// Binding and serving are separate so the caller can learn the real address of a
/// port-0 bind, and so the readiness signal is the caller's to give: `grove serve`
/// marks ready once the process is up, and a test can serve a deliberately
/// `booting` or `degraded` daemon to exercise the gate.
pub struct Daemon {
    state: AppState,
    listener: TcpListener,
    nudges: Option<mpsc::Receiver<()>>,
}

impl Daemon {
    /// Bind `config.bind` and assemble the state. The loopback gate has already run
    /// in [`Config::new`]; it runs again here so a hand-built `Config` cannot slip
    /// past it.
    ///
    /// The lanes and the event bus exist from here, but **no engine room does**:
    /// starting the watcher would create the home and start reading the manifest,
    /// which a caller that only wanted a bound listener never asked for.
    /// [`serve`](Self::serve) starts it.
    pub async fn bind(config: Config, clock: Arc<dyn Clock>) -> Result<Self, Error> {
        let logs = Arc::new(LogRing::new(Arc::clone(&clock), config.log_level));
        Self::bind_with_logs(config, clock, logs).await
    }

    /// Bind onto a log ring the caller already holds.
    ///
    /// The process installs its `tracing` subscriber before it has a daemon — a
    /// subscriber set late captures nothing said early, boot included — so the ring
    /// has to exist before the bind that would otherwise create it. `grove serve`
    /// builds one, layers it onto its subscriber, and hands it here.
    pub async fn bind_with_logs(
        config: Config,
        clock: Arc<dyn Clock>,
        logs: Arc<LogRing>,
    ) -> Result<Self, Error> {
        crate::config::guard_loopback(config.bind)?;
        let listener = TcpListener::bind(config.bind).await?;
        let (tx, rx) = mpsc::channel(NUDGE_DEPTH);
        let lanes = Arc::new(Lanes::new(Arc::clone(&clock), config.lane_idle));
        Ok(Self {
            state: AppState {
                boot: Arc::new(BootState::new(&*clock)),
                clock,
                config: Arc::new(config),
                lanes,
                events: EventBus::new(),
                logs,
                engines: Arc::new(OnceLock::new()),
                reconcile: ReconcileNudge { tx },
                shutdown: ShutdownTrigger::new(),
            },
            listener,
            nudges: Some(rx),
        })
    }

    /// The address actually bound — the real port when `config.bind` asked for 0.
    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        Ok(self.listener.local_addr()?)
    }

    /// The shared state: mark ready, mark degraded, trigger a shutdown.
    #[must_use]
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Claim the reconcile mailbox, and with it the engine room.
    ///
    /// The watcher is the mailbox's ordinary consumer, so taking it means "I am
    /// driving convergence, not you": [`serve`](Self::serve) then starts no watcher
    /// and no engines, and the caller reads raw nudges. That is how the HTTP
    /// contract suite exercises the routes against a daemon with no moving parts
    /// behind them.
    pub fn take_nudges(&mut self) -> Option<mpsc::Receiver<()>> {
        self.nudges.take()
    }

    /// Start the engine room: the manifest watcher and the engine set.
    ///
    /// Order matters. The set subscribes to the bus before the watcher can publish
    /// its boot announcement, so the announcement is not lost to a subscriber that
    /// was not there yet — belt to the braces of the set's own cold-boot read.
    fn start_engine_room(&mut self, nudges: mpsc::Receiver<()>) -> Watcher {
        let deps = Deps {
            home: self.state.config.home.clone(),
            lanes: Arc::clone(&self.state.lanes),
            events: self.state.events.clone(),
            clock: Arc::clone(&self.state.clock),
            clones: Arc::new(Semaphore::new(self.state.config.clone_limit)),
        };
        let set = RootSet::start(deps);
        // Infallible: `bind` just built this `OnceLock` and nothing else can reach
        // it before `serve`.
        let _ = self.state.engines.set(set);
        Watcher::start(
            self.state.config.home.clone(),
            Arc::clone(&self.state.clock),
            self.state.events.clone(),
            nudges,
            self.state.config.fs_watch,
        )
    }

    /// Serve until the shutdown trigger fires, then drain — **under a deadline**.
    ///
    /// The drain has two phases and one budget ([`Config::drain_budget`], armed off
    /// the clock seam when the trigger fires):
    ///
    /// 1. **Responses.** `axum::serve` waits for every in-flight connection. When the
    ///    budget expires first, the remaining connections are dropped and the fact is
    ///    logged rather than holding the process open (see [`DEFAULT_DRAIN`]).
    /// 2. **Lane work.** The accept loop stopping says nothing about the `git clone` a
    ///    lane is mid-way through. Returning here without waiting hands the problem to
    ///    runtime drop, which blocks until every started blocking task finishes — with
    ///    no bound, no log line, and nothing an operator can see. So whatever is left
    ///    of the budget is spent on [`Lanes::quiesced`], and what does not land is
    ///    reported by slug.
    ///
    /// Neither phase makes the daemon killable-only-when-idle: carried law 11 stands,
    /// and an abandoned clone is re-derived from disk on the next boot.
    ///
    /// [`Lanes::quiesced`]: crate::lane::Lanes::quiesced
    pub async fn serve(mut self) -> Result<(), Error> {
        // Readiness stays the caller's signal (see the crate doc); serving only
        // starts the machinery that convergence needs.
        let watcher = self
            .nudges
            .take()
            .map(|nudges| self.start_engine_room(nudges));

        let shutdown = self.state.shutdown.clone();
        let engines = Arc::clone(&self.state.engines);
        let lanes = Arc::clone(&self.state.lanes);
        let clock = Arc::clone(&self.state.clock);
        let budget = self.state.config.drain_budget;

        // The budget starts at the *trigger*, not at `serve`: it bounds the drain,
        // never the serving.
        let expired = {
            let (shutdown, clock) = (shutdown.clone(), Arc::clone(&clock));
            async move {
                shutdown.wait().await;
                crate::wait::until(clock.deadline(budget), &*clock).await;
            }
        };
        tokio::pin!(expired);

        let result = {
            let serving = axum::serve(self.listener, router(self.state))
                .with_graceful_shutdown(async move { shutdown.wait().await })
                .into_future();
            tokio::pin!(serving);
            tokio::select! {
                biased;
                result = &mut serving => Some(result),
                () = &mut expired => None,
            }
            // `serving` is dropped here, and the connections we gave up on with it.
        };
        if result.is_none() {
            tracing::warn!(
                reason = "drain budget expired",
                "abandoning responses that would not finish writing"
            );
        }

        // Stop driving before returning: an engine that outlives the HTTP surface is
        // a git writer nobody can observe or stop.
        if let Some(engines) = engines.get() {
            engines.shutdown();
        }
        if let Some(watcher) = watcher {
            watcher.shutdown().await;
        }

        // Phase two, on what is left of the same budget. An already-expired budget
        // skips it — the deadline future has resolved and must not be polled again.
        let drained = if result.is_some() {
            tokio::select! {
                biased;
                () = lanes.quiesced() => true,
                () = &mut expired => false,
            }
        } else {
            lanes.inflight() == 0
        };
        if !drained {
            tracing::warn!(
                count = lanes.inflight(),
                slug = ?lanes.active_slugs(),
                reason = "drain budget expired",
                "returning with git work still in flight"
            );
        }

        result.transpose()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Daemon, ShutdownTrigger};
    use crate::config::Config;
    use grove_ops::clock::SystemClock;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;

    /// A config over a home nothing else can be looking at.
    ///
    /// Per-test `TempDir` even though `bind` deliberately never touches the home:
    /// that is a property of `bind` today, not a contract, and a fixed `/tmp` path is
    /// a collision — and, on a shared machine, a plantable symlink — waiting for the
    /// first phase that pre-reads the manifest at bind time.
    fn config(home: &TempDir) -> Config {
        Config::new(home.path(), SocketAddr::from(([127, 0, 0, 1], 0))).unwrap()
    }

    /// A fire that lands before anything waits is still observed — the race a
    /// `Notify` would lose.
    #[tokio::test]
    async fn a_shutdown_fired_before_the_wait_is_still_observed() {
        let trigger = ShutdownTrigger::new();
        trigger.fire();
        tokio::time::timeout(Duration::from_secs(1), trigger.wait())
            .await
            .expect("an already-fired trigger resolves immediately");
    }

    #[tokio::test]
    async fn a_scheduled_shutdown_fires_after_its_grace() {
        let trigger = ShutdownTrigger::new();
        trigger.schedule(Duration::from_millis(10));
        tokio::time::timeout(Duration::from_secs(1), trigger.wait())
            .await
            .expect("the scheduled fire arrives");
    }

    /// Port 0 binds an ephemeral port and reports the real one — the only way tests
    /// stand up a daemon without a fixed port to collide on.
    #[tokio::test]
    async fn binding_port_zero_reports_the_real_address() {
        let home = TempDir::new().unwrap();
        let daemon = Daemon::bind(config(&home), Arc::new(SystemClock))
            .await
            .unwrap();
        let addr = daemon.local_addr().unwrap();
        assert!(addr.ip().is_loopback());
        assert_ne!(addr.port(), 0);
    }

    /// The gate again at the bind seam: a `Config` built by hand cannot walk a
    /// non-loopback address past it.
    #[tokio::test]
    async fn binding_refuses_a_non_loopback_address() {
        let home = TempDir::new().unwrap();
        let mut config = config(&home);
        config.bind = SocketAddr::from(([0, 0, 0, 0], 0));
        let Err(err) = Daemon::bind(config, Arc::new(SystemClock)).await else {
            panic!("a non-loopback bind must be refused")
        };
        assert!(err.to_string().contains("non-loopback"), "{err}");
    }

    /// The mailbox is the seam, so its coalescing is pinned: a nudge lands, a second
    /// one over an unread first is silently the same answer, and both are readable
    /// by whoever claims the receiver.
    #[tokio::test]
    async fn the_reconcile_seam_coalesces_and_delivers() {
        let home = TempDir::new().unwrap();
        let mut daemon = Daemon::bind(config(&home), Arc::new(SystemClock))
            .await
            .unwrap();
        let mut nudges = daemon.take_nudges().expect("the mailbox is unclaimed");

        daemon.state().reconcile.poke();
        daemon.state().reconcile.poke();

        assert!(nudges.try_recv().is_ok(), "the nudge arrives");
        assert!(
            nudges.try_recv().is_err(),
            "two pokes over one unread nudge coalesce"
        );
        assert!(
            daemon.take_nudges().is_none(),
            "the mailbox is claimed once"
        );
    }
}
