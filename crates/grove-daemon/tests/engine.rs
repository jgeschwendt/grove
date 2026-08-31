//! The phase-4 gate: the engine's semantics against real git, real lanes and the
//! real event bus.
//!
//! Everything here is hermetic — a per-test `TempDir` home, a per-test local fixture
//! remote, no network and no shared state — so the suite is safe under nextest's
//! per-test processes and safe beside a live grove.
//!
//! The unit-level halves live beside the code they pin: the transition table in
//! `engine::status`, lane priority and the idle reap in `lane`, debounce and
//! coalescing in `watcher`. What is here is what only the assembled engine can
//! show — that a burst really does collapse into one clone, that a terminal fault
//! really does stop where a transient one re-derives, and that a restarted engine
//! recovers everything it needs from the filesystem.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use grove_api::SyncNote;
use grove_api::events::{Event, TaskKind, TaskOutcome};
use grove_api::status::RootStatus;
use grove_daemon::RootSet;
use grove_daemon::engine;
use grove_daemon::engine::{Deps, Engine};
use grove_daemon::events::EventBus;
use grove_daemon::lane::{DEFAULT_IDLE, Lanes, Priority};
use grove_ops::clock::SystemClock;
use grove_ops::testfix;
use tempfile::TempDir;
use tokio::sync::{Semaphore, broadcast, oneshot};

const SLUG: &str = "o/r";

/// How long a bounded poll waits in total before failing the test. Generous, because
/// the thing being waited on is a real clone: the bound exists so a wedged engine is
/// a failing test rather than a hung suite, not to assert a latency.
const BUDGET: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(10);

/// A home declaring `o/r` against a writable local source repo, with nothing
/// realized: exactly what an offline `grove clone add` leaves behind for the daemon
/// to converge.
fn declared_home(tmp: &TempDir) -> (PathBuf, PathBuf) {
    let src = tmp.path().join("src");
    testfix::fixture_repo(&src);
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    declare(&home, SLUG, &src);
    (home, src)
}

fn declare(home: &Path, slug: &str, url: &Path) {
    grove_ops::manifest::add_root(
        &grove_ops::roots::manifest_path(home),
        slug,
        url.to_str().unwrap(),
    )
    .unwrap();
}

/// A commit on the source repo's `main`, so a sync has something to fast-forward to.
fn commit(repo: &Path, file: &str) {
    std::fs::write(repo.join(file), file).unwrap();
    let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
    testfix::git(repo, &[&id[..], &["add", "."]].concat());
    testfix::git(repo, &[&id[..], &["commit", "-q", "-m", file]].concat());
}

/// The daemon-side context an engine runs in, plus a subscription to what it says.
struct Room {
    deps: Deps,
    events: broadcast::Receiver<Event>,
}

impl Room {
    fn new(home: PathBuf) -> Self {
        Self::with_clone_limit(home, engine::DEFAULT_CLONE_LIMIT)
    }

    fn with_clone_limit(home: PathBuf, clones: usize) -> Self {
        let bus = EventBus::new();
        let events = bus.subscribe();
        Self {
            deps: Deps {
                home,
                lanes: Arc::new(Lanes::new(Arc::new(SystemClock), DEFAULT_IDLE)),
                events: bus,
                clock: Arc::new(SystemClock),
                clones: Arc::new(Semaphore::new(clones)),
            },
            events,
        }
    }

    fn start(&self, slug: &str) -> Engine {
        Engine::start(self.deps.clone(), slug)
    }

    /// Drain whatever the bus holds right now.
    fn drain(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
        out
    }
}

/// Poll `probe` until it answers `Some`, or fail the test.
///
/// A poll rather than a subscription: what these tests assert is a *settled* state
/// (this root is ready, this note is set), and waiting for the event that happens to
/// carry it would couple every assertion to the exact event sequence rather than to
/// the outcome.
async fn until<T>(what: &str, mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    for _ in 0..(BUDGET.as_millis() / POLL.as_millis()) {
        if let Some(value) = probe().await {
            return value;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("timed out waiting for {what}");
}

/// Wait for `engine` to settle on `status`.
async fn settles_on(engine: &Engine, status: RootStatus) {
    until(&format!("status {status}"), async || {
        (engine.status().await.unwrap() == status).then_some(())
    })
    .await;
}

/// Wait for an event the predicate accepts, returning everything seen up to and
/// including it.
async fn collect_until(
    events: &mut broadcast::Receiver<Event>,
    what: &str,
    accept: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(BUDGET, events.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("the bus stayed live");
        let done = accept(&event);
        seen.push(event);
        if done {
            return seen;
        }
    }
}

fn is_finished(event: &Event, want: TaskKind) -> bool {
    matches!(event, Event::TaskFinished { kind, .. } if *kind == want)
}

// ── realize ─────────────────────────────────────────────────────────────────────

/// The whole point of the engine: a declared-but-missing root becomes a realized one
/// with nobody asking, and the transient is visible while it happens.
#[tokio::test]
async fn an_engine_clones_a_declared_root_and_reports_ready() {
    let tmp = TempDir::new().unwrap();
    let (home, _src) = declared_home(&tmp);
    let mut room = Room::new(home.clone());

    let engine = room.start(SLUG);
    settles_on(&engine, RootStatus::Ready).await;

    assert!(grove_ops::roots::bare_dir(&home, SLUG).is_dir());
    assert!(grove_ops::roots::trunk_dir(&home, SLUG).is_dir());

    let seen = collect_until(&mut room.events, "the reconcile to finish", |e| {
        is_finished(e, TaskKind::Reconcile)
    })
    .await;
    assert!(
        seen.contains(&Event::TaskStarted {
            slug: SLUG.into(),
            kind: TaskKind::Reconcile
        }),
        "the reconcile announced itself: {seen:?}"
    );
    assert!(
        seen.contains(&Event::TaskFinished {
            slug: SLUG.into(),
            kind: TaskKind::Reconcile,
            outcome: TaskOutcome::Ok
        }),
        "…and its outcome: {seen:?}"
    );
}

/// A burst of manifest events over a busy root collapses into **one** follow-up
/// reconcile, and no two reconciles are ever in flight at once — the double-clone
/// impossibility, observed rather than argued.
///
/// The lane is held by a foreground job for the whole burst, so the engine's first
/// reconcile is dispatched but not running: that is the window in which a
/// level-triggered flag and an edge-triggered queue behave differently.
#[tokio::test]
async fn a_burst_of_manifest_events_collapses_into_one_reconcile() {
    let tmp = TempDir::new().unwrap();
    let (home, _src) = declared_home(&tmp);
    let mut room = Room::new(home);
    let lanes = Arc::clone(&room.deps.lanes);

    let (release, held) = oneshot::channel::<()>();
    let holder = tokio::spawn(async move {
        lanes
            .run(SLUG, Priority::Foreground, move || {
                let _ = held.blocking_recv();
            })
            .await
    });
    // Let the holding job reach the lane before the engine queues behind it.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let engine = room.start(SLUG);
    // The dispatch happened before the lane could run it, so the root is already
    // `cloning` — the honest status for work that is queued but not yet started.
    settles_on(&engine, RootStatus::Cloning).await;

    for _ in 0..10 {
        engine.roots_changed();
    }
    release.send(()).unwrap();
    holder.await.unwrap().unwrap();

    settles_on(&engine, RootStatus::Ready).await;
    // Let any further dispatch the burst could have caused surface.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut started = 0;
    let mut in_flight = 0;
    for event in room.drain() {
        match event {
            Event::TaskStarted {
                kind: TaskKind::Reconcile,
                ..
            } => {
                started += 1;
                in_flight += 1;
                assert_eq!(in_flight, 1, "two reconciles were in flight at once");
            }
            Event::TaskFinished {
                kind: TaskKind::Reconcile,
                ..
            } => in_flight -= 1,
            _ => {}
        }
    }
    assert_eq!(
        started, 2,
        "ten manifest events over a busy root must collapse to the in-flight \
         reconcile plus exactly one coalesced follow-up"
    );
}

// ── failure dispositions ────────────────────────────────────────────────────────

/// A reconcile that comes back `failed` — an unclonable URL — degrades the root, and
/// the degrade **sticks** with no event: there is no retry timer to walk it back.
#[tokio::test]
async fn a_failed_reconcile_degrades_and_stops() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // A local path that is not a repository: the clone fails with no resolver and no
    // socket, so the assertion never depends on what the network says.
    declare(&home, SLUG, &tmp.path().join("not-a-repo"));

    let room = Room::new(home);
    let engine = room.start(SLUG);
    settles_on(&engine, RootStatus::Degraded).await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        engine.status().await.unwrap(),
        RootStatus::Degraded,
        "nothing but an event may move a degraded root"
    );
}

/// …and the next event *is* the retry: a manifest fixed under a degraded engine
/// converges on the following `roots_changed`, with no timer involved.
#[tokio::test]
async fn a_degraded_root_recovers_on_the_next_event() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    declare(&home, SLUG, &tmp.path().join("not-a-repo"));

    let room = Room::new(home.clone());
    let engine = room.start(SLUG);
    settles_on(&engine, RootStatus::Degraded).await;

    // Repair the declaration the way an operator would, then announce it.
    let src = tmp.path().join("src");
    testfix::fixture_repo(&src);
    let manifest = grove_ops::roots::manifest_path(&home);
    grove_ops::manifest::remove_root(&manifest, SLUG).unwrap();
    declare(&home, SLUG, &src);
    engine.roots_changed();

    settles_on(&engine, RootStatus::Ready).await;
}

/// A **terminal** ops fault degrades; a **transient** one re-derives from disk. Same
/// engine shape, same absence of a retry timer, different resting state — and the
/// difference is exactly `grove_api::policy::classify_error`.
///
/// One test per terminal code, as v1's suite generated them: `invalid_input` here
/// (a slug `validate_slug` rejects) and `conflict` is unreachable from `reconcile_one`
/// without a would-clobber on disk, so it is pinned at the policy layer instead.
#[tokio::test]
async fn a_terminal_fault_degrades_where_a_transient_one_re_derives() {
    let tmp = TempDir::new().unwrap();
    let (home, _src) = declared_home(&tmp);
    let room = Room::new(home);

    // invalid_input: the slug never reaches git — `reconcile_one` refuses it.
    let terminal = room.start("../escape");
    settles_on(&terminal, RootStatus::Degraded).await;

    // not_declared: transient, so the engine trusts disk (nothing there) and waits.
    let transient = room.start("o/never-declared");
    settles_on(&transient, RootStatus::Missing).await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(terminal.status().await.unwrap(), RootStatus::Degraded);
    assert_eq!(
        transient.status().await.unwrap(),
        RootStatus::Missing,
        "a transient failure leaves the honest disk verdict, not a degrade"
    );
}

// ── sync ────────────────────────────────────────────────────────────────────────

/// The note's whole life: absent after a clean sync, set when the trunk diverges,
/// and cleared by the next clean one. Carried law 9 — a diverged trunk is reported,
/// never forced — is what the middle step pins.
#[tokio::test]
async fn slow_the_sync_note_is_set_by_a_diverged_trunk_and_cleared_by_a_clean_sync() {
    let tmp = TempDir::new().unwrap();
    let (home, src) = declared_home(&tmp);
    let mut room = Room::new(home.clone());
    let engine = room.start(SLUG);
    settles_on(&engine, RootStatus::Ready).await;

    // A clean fast-forward leaves no note.
    commit(&src, "REMOTE.md");
    engine.sync().await.unwrap();
    collect_until(&mut room.events, "the sync to finish", |e| {
        is_finished(e, TaskKind::Sync)
    })
    .await;
    assert_eq!(engine.sync_info().await.unwrap().note, None);

    // A local commit in the trunk plus a newer remote one: an ff is impossible.
    let trunk = grove_ops::roots::trunk_dir(&home, SLUG);
    commit(&trunk, "LOCAL.md");
    commit(&src, "REMOTE2.md");
    engine.sync().await.unwrap();
    collect_until(&mut room.events, "the diverged sync to finish", |e| {
        is_finished(e, TaskKind::Sync)
    })
    .await;
    assert_eq!(
        engine.sync_info().await.unwrap().note,
        Some(SyncNote::Diverged),
        "a diverged trunk is reported"
    );
    assert!(
        trunk.join("LOCAL.md").exists(),
        "…and left exactly where it was"
    );

    // Resolving it clears the note on the next clean sync.
    testfix::git(&trunk, &["reset", "-q", "--hard", "origin/main"]);
    engine.sync().await.unwrap();
    collect_until(&mut room.events, "the clearing sync to finish", |e| {
        is_finished(e, TaskKind::Sync)
    })
    .await;
    assert_eq!(engine.sync_info().await.unwrap().note, None);
}

/// `sync` is accept-only (invariant `push-only`): it answers as soon as the request
/// is recorded, and the completion arrives on the bus. The acknowledgement is
/// observable as "syncing" *before* any fetch could have finished.
#[tokio::test]
async fn sync_accepts_immediately_and_announces_its_completion() {
    let tmp = TempDir::new().unwrap();
    let (home, src) = declared_home(&tmp);
    let mut room = Room::new(home);
    let engine = room.start(SLUG);
    settles_on(&engine, RootStatus::Ready).await;
    commit(&src, "REMOTE.md");
    room.drain();

    engine.sync().await.unwrap();

    let seen = collect_until(&mut room.events, "the sync to finish", |e| {
        is_finished(e, TaskKind::Sync)
    })
    .await;
    // The accept broadcasts before the fetch, so every attached view shows the
    // in-flight state — not only the client that asked.
    assert_eq!(
        seen.first(),
        Some(&Event::RootSyncChanged { slug: SLUG.into() }),
        "the accept is announced first: {seen:?}"
    );
    assert!(
        seen.iter()
            .filter(|e| matches!(e, Event::RootSyncChanged { .. }))
            .count()
            >= 2,
        "…and so is the completion: {seen:?}"
    );
    assert!(!engine.sync_info().await.unwrap().syncing);
}

/// The other half of accept-only, and the one a client can misread: the accept
/// **records** the request, it does not schedule it. The driver dispatches a pending
/// sync only from `ready`, so on a degraded root the request latches — `syncing` is
/// true, no fetch runs, and the completion broadcast the route points a client at
/// does not come until the root recovers.
///
/// `degraded` is the corner worth pinning rather than `cloning` or `missing`: those
/// carry a reconcile that will deliver the root to `ready` on its own, while a
/// degraded root moves only on a dispatched reconcile — so this is where `syncing`
/// can rest indefinitely, and why `docs/api.md` § Sync tells a UI to read it as
/// "holds a sync request", never as "is fetching".
#[tokio::test]
async fn a_sync_accepted_on_a_degraded_root_waits_for_the_recovery() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    declare(&home, SLUG, &tmp.path().join("not-a-repo"));

    let mut room = Room::new(home.clone());
    let engine = room.start(SLUG);
    settles_on(&engine, RootStatus::Degraded).await;
    room.drain();

    // Accepted, exactly as the route reports it — the engine records the request for
    // a root it cannot currently sync.
    engine.sync().await.unwrap();
    assert!(
        engine.sync_info().await.unwrap().syncing,
        "the accept latches the request"
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        engine.sync_info().await.unwrap().syncing,
        "and it stays latched: nothing dispatches a sync off `ready`"
    );
    assert_eq!(engine.status().await.unwrap(), RootStatus::Degraded);
    assert_eq!(
        room.drain()
            .iter()
            .filter(|e| matches!(e, Event::RootSyncChanged { .. }))
            .count(),
        1,
        "only the accept was announced — a client waiting on the completion waits"
    );

    // The recovery is the retry, here as everywhere: repair the declaration, announce
    // it, and the latched sync rides the drive that follows the reconcile.
    let src = tmp.path().join("src");
    testfix::fixture_repo(&src);
    let manifest = grove_ops::roots::manifest_path(&home);
    grove_ops::manifest::remove_root(&manifest, SLUG).unwrap();
    declare(&home, SLUG, &src);
    engine.roots_changed();

    until("the latched sync to run", async || {
        (!engine.sync_info().await.unwrap().syncing).then_some(())
    })
    .await;
    assert_eq!(engine.status().await.unwrap(), RootStatus::Ready);
}

// ── pool ────────────────────────────────────────────────────────────────────────

/// The pool converges to its declared target without anyone driving it, and the
/// warm slot is **claimed by the reconcile that realizes a declared worktree** —
/// which is the only path a user can take (`grove tree add` against a running daemon
/// declares and nudges; the engine does the rest). A pool filled by a path nothing
/// redeems is a full checkout of disk per slot, forever.
#[tokio::test]
async fn slow_the_pool_fills_to_target_and_a_declared_worktree_claims_a_warm_slot() {
    let tmp = TempDir::new().unwrap();
    let (home, _src) = declared_home(&tmp);
    let manifest = grove_ops::roots::manifest_path(&home);
    grove_ops::manifest::set_pool_size(&manifest, SLUG, 1).unwrap();
    grove_ops::manifest::add_share(&manifest, SLUG, "symlink", &[".env"]).unwrap();

    let room = Room::new(home.clone());
    let engine = room.start(SLUG);
    settles_on(&engine, RootStatus::Ready).await;

    until("the pool to reach its target", async || {
        (grove_ops::worktrees::pool_count(&home, SLUG).unwrap() >= 1).then_some(())
    })
    .await;
    // A mark inside the slot: the worktree carrying it afterwards is proof the slot
    // was MOVED, not that a cold checkout happened to land at the same path.
    let slot_mark = grove_ops::roots::root_dir(&home, SLUG).join(".pool/slot-0/CLAIMED");
    std::fs::write(&slot_mark, "warm").unwrap();

    // What `grove tree add` writes when a daemon is up: a declaration, then a nudge.
    grove_ops::manifest::add_worktree(&manifest, SLUG, "feat", "feature/x", Some("main")).unwrap();
    engine.roots_changed();

    // The share is the LAST step of a claim (attach → move → declare →
    // materialize), so waiting on it is waiting for the whole thing to land.
    let worktree = grove_ops::roots::root_dir(&home, SLUG).join("feat");
    until(
        "the declared worktree to be realized with its share",
        async || {
            std::fs::symlink_metadata(worktree.join(".env"))
                .is_ok_and(|m| m.is_symlink())
                .then_some(())
        },
    )
    .await;
    assert!(worktree.join("README.md").is_file(), "the tree moved");
    assert!(
        worktree.join("CLAIMED").is_file(),
        "the warm slot was claimed rather than cold-created beside it"
    );
    // And the pool refills toward its target off the same claim — the hint the
    // reconcile brought back is the observed count, not a stale one.
    until("the pool to refill", async || {
        (grove_ops::worktrees::pool_count(&home, SLUG).unwrap() >= 1).then_some(())
    })
    .await;
}

/// With no warm slot, the same declaration is cold-created — warm and cold are
/// interchangeable, which is what lets the claim live on the realizing path.
#[tokio::test]
async fn slow_a_declared_worktree_is_cold_created_when_the_pool_is_empty() {
    let tmp = TempDir::new().unwrap();
    let (home, _src) = declared_home(&tmp);
    let manifest = grove_ops::roots::manifest_path(&home);
    let room = Room::new(home.clone());
    let engine = room.start(SLUG);
    settles_on(&engine, RootStatus::Ready).await;

    grove_ops::manifest::add_worktree(&manifest, SLUG, "feat", "feature/x", Some("main")).unwrap();
    engine.roots_changed();

    let worktree = grove_ops::roots::root_dir(&home, SLUG).join("feat");
    until("the declared worktree to be realized", async || {
        worktree.join("README.md").is_file().then_some(())
    })
    .await;
    assert_eq!(grove_ops::worktrees::pool_count(&home, SLUG).unwrap(), 0);
}

/// Convergence runs both ways. A target lowered to zero used to leave every slot on
/// disk forever, with `doctor` reporting `n/0` and no command that could fix it.
#[tokio::test]
async fn slow_a_lowered_pool_target_reclaims_its_slots() {
    let tmp = TempDir::new().unwrap();
    let (home, _src) = declared_home(&tmp);
    let manifest = grove_ops::roots::manifest_path(&home);
    grove_ops::manifest::set_pool_size(&manifest, SLUG, 2).unwrap();

    let room = Room::new(home.clone());
    let engine = room.start(SLUG);
    settles_on(&engine, RootStatus::Ready).await;
    until("the pool to reach its target", async || {
        (grove_ops::worktrees::pool_count(&home, SLUG).unwrap() == 2).then_some(())
    })
    .await;

    grove_ops::manifest::set_pool_size(&manifest, SLUG, 0).unwrap();
    engine.roots_changed();

    until("the pool to drain to the new target", async || {
        (grove_ops::worktrees::pool_count(&home, SLUG).unwrap() == 0).then_some(())
    })
    .await;
}

// ── the clone bound ─────────────────────────────────────────────────────────────

/// The mitigation v1 designed and shelved: six roots declared at once do not become
/// six concurrent clones. Bounded dispatch-side, so a queued root still reads
/// `cloning` rather than pretending nothing is happening.
#[tokio::test]
async fn slow_the_clone_semaphore_bounds_concurrent_reconciles() {
    const ROOTS: usize = 6;
    const LIMIT: usize = 2;

    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    testfix::fixture_repo(&src);
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let slugs: Vec<String> = (0..ROOTS).map(|n| format!("o/r{n}")).collect();
    for slug in &slugs {
        declare(&home, slug, &src);
    }

    let mut room = Room::with_clone_limit(home.clone(), LIMIT);
    let engines: Vec<Engine> = slugs.iter().map(|slug| room.start(slug)).collect();

    // Watch the whole run rather than sampling: the bound is a property of every
    // instant, so every transition is checked as it arrives.
    let mut in_flight = 0usize;
    let mut peak = 0usize;
    let mut finished = 0usize;
    while finished < ROOTS {
        let event = tokio::time::timeout(BUDGET, room.events.recv())
            .await
            .expect("the roots converge")
            .expect("the bus stayed live");
        match event {
            Event::TaskStarted {
                kind: TaskKind::Reconcile,
                ..
            } => {
                in_flight += 1;
                peak = peak.max(in_flight);
                assert!(
                    in_flight <= LIMIT,
                    "{in_flight} clones ran at once against a limit of {LIMIT}"
                );
            }
            Event::TaskFinished {
                kind: TaskKind::Reconcile,
                ..
            } => {
                in_flight -= 1;
                finished += 1;
            }
            _ => {}
        }
    }

    for engine in &engines {
        settles_on(engine, RootStatus::Ready).await;
    }
    assert!((1..=LIMIT).contains(&peak), "peak concurrency was {peak}");
}

/// A reconcile that cannot get a clone permit **releases its root's lane** instead of
/// holding it, and converges as soon as a permit frees — with no timer.
///
/// This is the difference between one wedged clone and a wedged home. When the permit
/// is *waited for* inside the lane job, every root whose reconcile is queued behind
/// the exhausted semaphore holds its own lane too; their snapshot reads then shed as
/// `unavailable`, so a handful of unreachable-but-connected remotes takes the whole
/// home's readability down while `/api/health` still says `ready`.
#[tokio::test]
async fn a_reconcile_without_a_clone_permit_frees_its_lane_and_resumes_when_one_lands() {
    let tmp = TempDir::new().unwrap();
    let (home, _src) = declared_home(&tmp);
    let room = Room::with_clone_limit(home.clone(), 1);
    let lanes = Arc::clone(&room.deps.lanes);
    // Every permit taken, as a stalled clone would hold it.
    let held = Arc::clone(&room.deps.clones).acquire_owned().await.unwrap();

    let engine = room.start(SLUG);

    // The root's own lane still answers — this is the read a dashboard would shed.
    let lane_answered = tokio::time::timeout(
        Duration::from_secs(5),
        lanes.run(SLUG, Priority::Foreground, || "read"),
    )
    .await
    .expect("the lane was free while the reconcile waited for a permit")
    .unwrap();
    assert_eq!(lane_answered, "read");
    assert!(
        !grove_ops::roots::bare_dir(&home, SLUG).is_dir(),
        "…and nothing was cloned without a permit"
    );

    // The permit landing is the event that re-drives it. No retry clock anywhere.
    drop(held);
    settles_on(&engine, RootStatus::Ready).await;
}

// ── restart ─────────────────────────────────────────────────────────────────────

/// Reactivation is a cold boot (carried law 11): a fresh engine over a realized root
/// answers `ready` from the filesystem alone — it inherits nothing from the engine
/// that died, and it does not need a reconcile to find out.
#[tokio::test]
async fn a_restarted_engine_re_derives_its_status_from_disk() {
    let tmp = TempDir::new().unwrap();
    let (home, _src) = declared_home(&tmp);
    let room = Room::new(home.clone());

    let first = room.start(SLUG);
    settles_on(&first, RootStatus::Ready).await;
    first.stop();

    let restarted = room.start(SLUG);
    settles_on(&restarted, RootStatus::Ready).await;

    // And the other direction: a root whose `.trunk` vanished out of band is
    // rediscovered as missing, not remembered as ready.
    restarted.stop();
    std::fs::remove_dir_all(grove_ops::roots::trunk_dir(&home, SLUG)).unwrap();
    std::fs::remove_dir_all(grove_ops::roots::bare_dir(&home, SLUG)).unwrap();
    let third = Engine::start(
        Deps {
            // A lane registry as fresh as the engine: nothing at all carries over.
            lanes: Arc::new(Lanes::new(Arc::new(SystemClock), DEFAULT_IDLE)),
            ..room.deps.clone()
        },
        SLUG,
    );
    // It re-clones, because that is what a declared-but-missing root means.
    settles_on(&third, RootStatus::Ready).await;
    assert!(grove_ops::roots::trunk_dir(&home, SLUG).is_dir());
}

/// A stopped engine is gone: it answers nothing, and the work it had in flight is
/// not left addressing it.
#[tokio::test]
async fn a_stopped_engine_stops_answering() {
    let tmp = TempDir::new().unwrap();
    let (home, _src) = declared_home(&tmp);
    let room = Room::new(home);
    let engine = room.start(SLUG);
    settles_on(&engine, RootStatus::Ready).await;

    engine.stop();

    until("the engine to stop answering", async || {
        engine.status().await.err().map(|_| ())
    })
    .await;
}

// ── the engine set ──────────────────────────────────────────────────────────────

/// The set mirrors the declared roots: start what is declared and not running, stop
/// what is running and not declared, and cold-boot from the manifest rather than
/// waiting for an announcement that may already have been published.
///
/// The roots here point at paths that are not repositories, so each engine degrades
/// in milliseconds — membership is what is under test, not realization.
#[tokio::test]
async fn the_engine_set_mirrors_the_declared_roots() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let missing = tmp.path().join("not-a-repo");
    declare(&home, "o/a", &missing);
    declare(&home, "o/b", &missing);

    let room = Room::new(home.clone());
    let set = RootSet::start(room.deps.clone());

    // Cold boot: the set reads the manifest itself.
    until("both declared roots to have engines", async || {
        (set.slugs().await == ["o/a", "o/b"]).then_some(())
    })
    .await;

    // An announcement carrying a smaller set stops the engine that left it.
    grove_ops::manifest::remove_root(&grove_ops::roots::manifest_path(&home), "o/b").unwrap();
    room.deps.events.publish(Event::RootsChanged {
        roots: vec!["o/a".into()],
    });
    until("the undeclared root's engine to stop", async || {
        (set.slugs().await == ["o/a"]).then_some(())
    })
    .await;

    // …and a root declared out of band appears on the next resync.
    declare(&home, "o/c", &missing);
    set.resync();
    until("the newly declared root to get an engine", async || {
        (set.slugs().await == ["o/a", "o/c"]).then_some(())
    })
    .await;

    // The statuses the set reports are the only place a degrade is observable — no
    // disk read can tell a degraded root from a merely unrealized one.
    let statuses = until("both engines to settle", async || {
        let statuses = set.statuses(None).await;
        statuses
            .iter()
            .all(|entry| entry.status == RootStatus::Degraded)
            .then_some(statuses)
    })
    .await;
    assert_eq!(statuses.len(), 2);
    assert_eq!(
        set.statuses(Some("o/a")).await,
        statuses
            .iter()
            .filter(|entry| entry.slug == "o/a")
            .cloned()
            .collect::<Vec<_>>(),
        "a slug-scoped doctor reports only that root's engine"
    );
    assert!(
        set.statuses(Some("o/never-declared")).await.is_empty(),
        "a root with no engine contributes no status at all"
    );

    set.shutdown();
}

// ── failure isolation ───────────────────────────────────────────────────────────

/// **A manifest we could not read is not an empty manifest.**
///
/// `Set::reconcile` treats "not in the declared list" as "stop this engine", so a
/// read failure folded into an empty vector executes as "nothing is declared" and
/// tears down every driver in the daemon — taking with it the `cloning`/`degraded`
/// facts that are the only reason an engine is resident. Push-based convergence
/// (carried law 8) means nothing re-drives it afterwards either.
#[tokio::test]
async fn an_unreadable_manifest_leaves_every_engine_running() {
    let tmp = TempDir::new().unwrap();
    let home = testfix::home_with_root(&tmp);
    let room = Room::new(home.clone());
    let set = RootSet::start(room.deps.clone());

    until("the engine set to start the declared root", async || {
        (!set.slugs().await.is_empty()).then_some(())
    })
    .await;

    // A directory where the manifest belongs: every read of it fails.
    let manifest = grove_ops::roots::manifest_path(&home);
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir_all(&manifest).unwrap();

    // Deterministic without a sleep: the set's mailbox is FIFO and it handles the
    // resync — read included — before it answers the query behind it.
    set.resync();
    assert_eq!(
        set.slugs().await,
        vec![testfix::SLUG.to_owned()],
        "a failed manifest read must reconcile nothing, not stop everything"
    );

    set.shutdown();
}

// ── the clone permit and the lanes ──────────────────────────────────────────────

/// A reconcile parked on its **own** root's busy lane must not be holding one of the
/// global clone permits while it waits.
///
/// Acquiring dispatch-side and then awaiting lane admission inverts the bound: a
/// reconcile doing nothing at all starves roots whose lanes are completely free. One
/// permit here, so a permit held by the parked root is the only one there is — and a
/// whole-home doctor or a snapshot fan-out takes every root's foreground lane at
/// once, which is how four of these stall a home.
#[tokio::test]
async fn a_reconcile_parked_on_a_busy_lane_holds_no_clone_permit() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    testfix::fixture_repo(&src);
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    declare(&home, "o/parked", &src);
    declare(&home, "o/free", &src);

    let mut room = Room::with_clone_limit(home.clone(), 1);

    // `o/parked`'s lane is held by foreground work — a remove, a doctor pass, a
    // snapshot row — for as long as this test says so.
    let (release, held) = oneshot::channel::<()>();
    let (entered, running) = oneshot::channel::<()>();
    let holder = {
        let lanes = Arc::clone(&room.deps.lanes);
        tokio::spawn(async move {
            lanes
                .run("o/parked", Priority::Foreground, move || {
                    let _ = entered.send(());
                    let _ = held.blocking_recv();
                })
                .await
        })
    };
    running
        .await
        .expect("the holder reached the head of the lane");

    let _parked = room.start("o/parked");
    let _free = room.start("o/free");

    collect_until(
        &mut room.events,
        "the free root's reconcile to start",
        |e| {
            matches!(
                e,
                Event::TaskStarted { slug, kind: TaskKind::Reconcile } if slug == "o/free"
            )
        },
    )
    .await;

    release.send(()).unwrap();
    holder.await.unwrap().unwrap();
}
