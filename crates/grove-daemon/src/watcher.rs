//! The manifest watcher: discover and announce.
//!
//! One task, three inputs, one job. The inputs are the HTTP nudge
//! (`POST /api/roots/reconcile`, and the poke a root removal sends after itself), an
//! optional filesystem watch on `$GROVE_HOME/manifest.toml`, and one announcement at
//! boot. The job is `roots.adopt` — declare any undeclared on-disk bare, **never
//! clone** — then read the declared set and publish [`Event::RootsChanged`].
//!
//! **The nudge is the reliable path; fs-watch is opportunistic.** The CLI declares a
//! manifest change and then nudges, so convergence never depends on the watcher
//! *noticing* a write (a platform watcher arms asynchronously and can miss one made
//! just after boot). The filesystem watch adds coverage for what the CLI cannot tell
//! us about: a hand-edit, a git-synced manifest from another machine. It is
//! therefore off by default in [`Config::new`] — tests drive the reliable path — and
//! on in [`Config::from_env`].
//!
//! **Do not merge this with the engines.** Discover/announce and realize/own-status
//! are different duties on purpose: the watcher never clones, and an engine never
//! reads the manifest for anyone but itself.
//!
//! [`Config::new`]: crate::Config::new
//! [`Config::from_env`]: crate::Config::from_env

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use grove_api::events::Event;
use grove_ops::clock::{Clock, Deadline};
use tokio::sync::mpsc;

use crate::events::EventBus;
use crate::wait;

/// How long a burst of triggers is collected before one adopt runs.
///
/// The manifest is published by an atomic temp+rename, which fires more than one
/// filesystem event per save, and a CLI that declares several roots nudges once per
/// root. Both collapse into a single pass here.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// The basename the filesystem watch cares about. Everything else under the home —
/// the lock file, `code/`, `channel` — is noise.
const MANIFEST: &str = "manifest.toml";

/// A running watcher. Dropping the handle does not stop it; [`Watcher::shutdown`]
/// does, and so does dropping every [`crate::ReconcileNudge`] that feeds it.
#[derive(Debug)]
pub struct Watcher {
    stop: mpsc::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Watcher {
    /// Start watching `home`, consuming `nudges` as one of its triggers.
    ///
    /// Creates `home` if it is not there — v1's `mkdir_p!` at init. A daemon pointed
    /// at a home nobody has used yet must still be able to watch for the manifest
    /// that is about to appear.
    #[must_use]
    pub fn start(
        home: PathBuf,
        clock: Arc<dyn Clock>,
        events: EventBus,
        nudges: mpsc::Receiver<()>,
        fs_watch: bool,
    ) -> Self {
        if let Err(e) = std::fs::create_dir_all(&home) {
            tracing::warn!(home = %home.display(), error = %e, "could not create the grove home");
        }
        let (stop, stop_rx) = mpsc::channel(1);
        let handle = tokio::spawn(run(home, clock, events, nudges, fs_watch, stop_rx));
        Self { stop, handle }
    }

    /// Ask the watcher to stop, and wait for it.
    pub async fn shutdown(self) {
        let _ = self.stop.send(()).await;
        let _ = self.handle.await;
    }
}

/// The nudge mailbox, or nothing.
///
/// A closed `mpsc::Receiver` is **permanently ready**: `recv()` on it answers `None`
/// immediately and forever. Left as a live `select!` arm that is a permanently ready
/// branch with no yield point, which spins a core — and, on a current-thread runtime,
/// starves every other task including the one carrying the stop signal. Retiring the
/// receiver retires the branch: `None` here parks, which is what "this trigger is
/// gone" actually means.
async fn next_nudge(nudges: &mut Option<mpsc::Receiver<()>>) -> Option<()> {
    match nudges {
        Some(nudges) => nudges.recv().await,
        None => std::future::pending().await,
    }
}

async fn run(
    home: PathBuf,
    clock: Arc<dyn Clock>,
    events: EventBus,
    nudges: mpsc::Receiver<()>,
    fs_watch: bool,
    mut stop: mpsc::Receiver<()>,
) {
    let mut nudges = Some(nudges);
    // `fs` is held for the task's lifetime: dropping a `notify` watcher unregisters
    // it. It is also read once, as the "is there any trigger left?" test below.
    let (fs, mut fs_events) = if fs_watch {
        match watch_manifest(&home) {
            Ok((watcher, rx)) => (Some(watcher), rx),
            Err(e) => {
                // Not fatal, by design: the nudge path is the reliable one, and a
                // daemon that refused to start because a platform watcher was
                // unavailable would be trading a real capability for a redundant one.
                tracing::warn!(error = %e, "manifest watcher unavailable");
                (None, mpsc::unbounded_channel().1)
            }
        }
    } else {
        (None, mpsc::unbounded_channel().1)
    };

    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<()>();
    let mut armed: Option<Deadline> = None;
    let mut running = false;
    let mut pending = false;

    // One announcement at boot, so the engine set and any attached view learn the
    // declared world without waiting for the first change to it.
    let mut fire_now = true;

    loop {
        if fire_now {
            fire_now = false;
            armed = None;
            if running {
                pending = true;
            } else {
                running = true;
                spawn_adopt(home.clone(), events.clone(), done_tx.clone());
            }
        }

        // Set when the nudge channel closes while a filesystem watch survives it;
        // applied after the `select!`, where the borrow the arm held is over.
        let mut retire_nudges = false;

        tokio::select! {
            biased;
            _ = stop.recv() => break,
            nudge = next_nudge(&mut nudges) => match nudge {
                // Every trigger arms the same debounce; one already armed absorbs it.
                Some(()) => { armed.get_or_insert_with(|| clock.deadline(DEBOUNCE)); }
                // Nobody can nudge us any more. With no filesystem watch either
                // there is no trigger left, so the task is finished rather than
                // parked forever on a closed channel.
                None if fs.is_none() => break,
                // With one, the fs watch is now the only trigger — so drop the dead
                // receiver rather than re-reading `None` from it forever.
                None => retire_nudges = true,
            },
            Some(()) = fs_events.recv() => { armed.get_or_insert_with(|| clock.deadline(DEBOUNCE)); },
            Some(()) = done_rx.recv() => {
                running = false;
                // A trigger that arrived mid-run is honored exactly once, however
                // many arrived: the flag is level, not a counter.
                if pending {
                    pending = false;
                    fire_now = true;
                }
            },
            () = wait::until(armed.unwrap_or_else(|| clock.deadline(DEBOUNCE)), &*clock),
                if armed.is_some() => { fire_now = true; },
        }

        if retire_nudges {
            tracing::debug!(
                "the reconcile mailbox closed; the filesystem watch is the last trigger"
            );
            nudges = None;
        }
    }
}

/// Run one adopt+list+announce off the watcher's own task.
///
/// Off-task for the reason v1 put it off its mailbox: this touches the filesystem
/// and the manifest lock, and the nudge that triggers it must stay answerable in
/// microseconds. Failure-tolerant in both directions — an error logs and skips the
/// broadcast (announcing a set we could not read would be worse than silence), and a
/// panic still reports completion so a pending trigger is honored.
fn spawn_adopt(home: PathBuf, events: EventBus, done: mpsc::UnboundedSender<()>) {
    tokio::spawn(async move {
        let work = tokio::task::spawn_blocking(move || {
            grove_ops::roots::adopt(&home)?;
            grove_ops::roots::list(&home)
        })
        .await;
        match work {
            Ok(Ok(roots)) => {
                let roots: Vec<String> = roots.into_iter().map(|root| root.slug).collect();
                tracing::info!(count = roots.len(), "roots adopted");
                events.publish(Event::RootsChanged { roots });
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "roots adopt failed"),
            Err(e) => tracing::warn!(error = %e, "roots adopt crashed"),
        }
        let _ = done.send(());
    });
}

/// Watch `home` for writes to its `manifest.toml`.
///
/// Non-recursive: the manifest is a direct child, and recursing would put every git
/// object write under `code/` through the filter. Matched on **basename**, as v1
/// matched it — a platform watcher reports the temp file and the rename under
/// several path spellings, and the one thing they agree on is the final name.
fn watch_manifest(
    home: &Path,
) -> notify::Result<(notify::RecommendedWatcher, mpsc::UnboundedReceiver<()>)> {
    use notify::{RecursiveMode, Watcher as _};

    let (tx, rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if event
            .paths
            .iter()
            .any(|path| path.file_name().is_some_and(|name| name == MANIFEST))
        {
            // Unbounded and non-blocking: this runs on the platform watcher's own
            // thread, which must never be parked by our debounce.
            let _ = tx.send(());
        }
    })?;
    watcher.watch(home, RecursiveMode::NonRecursive)?;
    Ok((watcher, rx))
}

#[cfg(test)]
mod tests {
    use super::{DEBOUNCE, Watcher};
    use crate::events::EventBus;
    use grove_api::events::Event;
    use grove_ops::clock::TestClock;
    use grove_ops::testfix;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    /// The watcher's inputs and its one output.
    struct Harness {
        watcher: Option<Watcher>,
        clock: Arc<TestClock>,
        nudge: mpsc::Sender<()>,
        events: tokio::sync::broadcast::Receiver<Event>,
    }

    impl Harness {
        fn start(home: std::path::PathBuf) -> Self {
            let clock = Arc::new(TestClock::new());
            let bus = EventBus::new();
            let events = bus.subscribe();
            let (nudge, nudges) = mpsc::channel(1);
            let watcher = Watcher::start(home, clock.clone(), bus, nudges, false);
            Self {
                watcher: Some(watcher),
                clock,
                nudge,
                events,
            }
        }

        /// Wait for the next announcement, stepping the fake clock as we go.
        ///
        /// Stepping in a loop rather than once: the watcher arms its debounce when
        /// it *processes* a trigger, which may be after a single `advance` has
        /// already happened — leaving a deadline in a future the frozen clock never
        /// reaches. Real time has no such race. The iteration bound is what keeps a
        /// wedged watcher a failing test rather than a hung suite.
        async fn announced(&mut self) -> Vec<String> {
            for _ in 0..200 {
                self.clock.advance(DEBOUNCE);
                if let Ok(event) = tokio::time::timeout(crate::wait::TICK, self.events.recv()).await
                {
                    return match event.expect("the bus is live") {
                        Event::RootsChanged { roots } => roots,
                        other => panic!("expected roots_changed, got {other:?}"),
                    };
                }
            }
            panic!("no announcement arrived");
        }

        async fn stop(mut self) {
            self.watcher.take().unwrap().shutdown().await;
        }
    }

    /// The boot announcement: a watcher over a home with a declared root publishes
    /// that set without anyone asking.
    #[tokio::test]
    async fn a_watcher_announces_the_declared_set_at_boot() {
        let tmp = TempDir::new().unwrap();
        let mut harness = Harness::start(testfix::home_with_root(&tmp));
        assert_eq!(harness.announced().await, vec![testfix::SLUG.to_owned()]);
        harness.stop().await;
    }

    /// A home that does not exist yet is created, and announces an empty set rather
    /// than failing — the daemon may be pointed at a home whose manifest is about to
    /// be written.
    #[tokio::test]
    async fn a_missing_home_is_created_and_announces_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("brand-new");
        let mut harness = Harness::start(home.clone());
        assert!(harness.announced().await.is_empty());
        assert!(home.is_dir(), "the home is created at init");
        harness.stop().await;
    }

    /// The debounce, on the fake clock: three nudges inside one window produce one
    /// announcement, and nothing fires until the clock actually passes it.
    #[tokio::test]
    async fn a_burst_of_nudges_debounces_into_one_announcement() {
        let tmp = TempDir::new().unwrap();
        let mut harness = Harness::start(testfix::home_with_root(&tmp));
        harness.announced().await; // the boot announcement

        for _ in 0..3 {
            harness.nudge.send(()).await.unwrap();
        }
        // Time has not moved, so the debounce has not elapsed.
        tokio::time::sleep(crate::wait::TICK * 2).await;
        assert!(
            harness.events.try_recv().is_err(),
            "the debounce must hold until its budget elapses"
        );

        assert_eq!(harness.announced().await, vec![testfix::SLUG.to_owned()]);

        // …and exactly one: the burst coalesced rather than queueing three passes.
        tokio::time::sleep(crate::wait::TICK * 2).await;
        assert!(harness.events.try_recv().is_err(), "the burst coalesced");
        harness.stop().await;
    }

    /// A root declared out of band — the offline `grove clone add` case — is
    /// announced on the next nudge, which is how the engine set learns to start an
    /// engine for it.
    #[tokio::test]
    async fn a_nudge_re_reads_the_declared_set() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let mut harness = Harness::start(home.clone());
        assert!(harness.announced().await.is_empty());

        grove_ops::manifest::add_root(
            &grove_ops::roots::manifest_path(&home),
            "o/later",
            "https://example.invalid/o/later.git",
        )
        .unwrap();
        harness.nudge.send(()).await.unwrap();

        assert_eq!(harness.announced().await, vec!["o/later".to_owned()]);
        harness.stop().await;
    }

    /// An adopt whose home is unreadable logs and **skips the broadcast**: announcing
    /// a declared set we could not read would push a wrong set to every consumer.
    #[tokio::test]
    async fn a_failed_adopt_announces_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        // A directory where the manifest belongs: every read of it fails.
        std::fs::create_dir_all(grove_ops::roots::manifest_path(&home)).unwrap();

        let mut harness = Harness::start(home);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            harness.events.try_recv().is_err(),
            "a failed adopt is silent, not a wrong announcement"
        );

        // And the watcher is still alive: a later trigger is still accepted.
        harness.nudge.send(()).await.unwrap();
        harness.clock.advance(DEBOUNCE);
        tokio::time::sleep(Duration::from_millis(100)).await;
        harness.stop().await;
    }

    /// A retired mailbox **parks**; it does not keep answering.
    ///
    /// The distinction is the whole fix. A closed `mpsc::Receiver` reports `None`
    /// immediately and forever, so a `select!` arm still holding one is a permanently
    /// ready branch — the loop re-polls it as fast as the scheduler allows, for as
    /// long as the watcher lives.
    #[tokio::test]
    async fn a_retired_nudge_mailbox_parks_instead_of_answering() {
        let (tx, rx) = mpsc::channel::<()>(1);
        drop(tx);
        let mut nudges = Some(rx);
        assert_eq!(
            super::next_nudge(&mut nudges).await,
            None,
            "a closed mailbox reports itself once"
        );

        nudges = None;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), super::next_nudge(&mut nudges))
                .await
                .is_err(),
            "…and then parks: an arm that keeps answering is an arm the loop spins on"
        );
    }

    /// The spin itself, made observable — and in the shape production actually hits
    /// it. `axum::serve` consumes the router and every `ReconcileNudge` in it, so the
    /// nudge sender is dropped *before* `Watcher::shutdown` is called, with the
    /// filesystem watch armed (`Config::from_env` turns it on for every real daemon).
    ///
    /// Under a **paused** clock tokio auto-advances time only while the runtime is
    /// idle. So a watcher that parks lets an hour-long sleep resolve in microseconds,
    /// and one that spins never lets the runtime reach idle at all — the sleep then
    /// never returns, which is the busy-wait stated as an assertion rather than as a
    /// number of iterations. Judged from *outside* the runtime, since a starved
    /// runtime cannot fire the timeout that would judge it from within.
    ///
    /// (The spin is a burnt core, not a wedge: tokio's cooperative budget yields the
    /// task every 128 ready polls, so a `shutdown` racing it still lands. Measured at
    /// exactly 128 iterations per slice.)
    #[test]
    fn a_closed_nudge_channel_does_not_spin_the_watcher() {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            runtime.block_on(async {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                std::fs::create_dir_all(&home).unwrap();
                let (nudge, nudges) = mpsc::channel(1);
                let watcher = Watcher::start(
                    home,
                    Arc::new(TestClock::new()),
                    EventBus::new(),
                    nudges,
                    true,
                );
                // What `axum::serve` returning does to the last nudge sender.
                drop(nudge);
                tokio::time::sleep(Duration::from_secs(3600)).await;
                watcher.shutdown().await;
            });
            let _ = done.send(());
        });

        assert!(
            finished.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the watcher never let its runtime go idle: a closed nudge mailbox left in \
             the `select!` is a permanently ready branch, and the loop burns a core on \
             it for as long as the daemon lives"
        );
    }

    /// The filesystem half, against a real watcher on a real file. `slow_` because a
    /// platform watcher arms asynchronously and the event budget is real seconds —
    /// exactly the reason the nudge, not this, is the reliable path.
    #[tokio::test]
    async fn slow_an_fs_write_to_the_manifest_announces() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        let clock = Arc::new(TestClock::new());
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let (_nudge, nudges) = mpsc::channel(1);
        let watcher = Watcher::start(home.clone(), clock.clone(), bus, nudges, true);

        // Drain the boot announcement.
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        // Let the platform watcher arm before the write it is supposed to see.
        tokio::time::sleep(Duration::from_millis(500)).await;

        grove_ops::manifest::add_root(
            &grove_ops::roots::manifest_path(&home),
            "o/by-hand",
            "https://example.invalid/o/by-hand.git",
        )
        .unwrap();

        // The fs event arms the debounce; the fake clock releases it. Advance in a
        // loop, since we cannot know when the platform delivered the event.
        let announced = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                clock.advance(DEBOUNCE);
                if let Ok(Ok(Event::RootsChanged { roots })) =
                    tokio::time::timeout(Duration::from_millis(100), events.recv()).await
                    && !roots.is_empty()
                {
                    return roots;
                }
            }
        })
        .await
        .expect("a hand-edit reaches the watcher");

        assert_eq!(announced, vec!["o/by-hand".to_owned()]);
        watcher.shutdown().await;
    }
}
