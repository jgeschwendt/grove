//! The clock seam: the one place `Instant::now()` is called.
//!
//! Every wall-clock budget in grove is expressed as a [`Deadline`] read off a
//! [`Clock`], never as an inline `Instant::now()`. Production wires [`SystemClock`];
//! tests wire [`TestClock`] and step time by hand, so a "60 s ready timeout" test
//! costs microseconds and cannot flake on a loaded machine.
//!
//! Every budget in the tree reads it: the clone watchdog and the bounded `run_git`
//! loop in [`crate::git`], the daemon's lane idle-reap, watcher debounce, snapshot,
//! doctor and drain budgets, and the CLI's ready/stop/kill and health-gate ladders.
//! Not by convention, but because `every_wall_clock_read_goes_through_the_clock_seam`
//! (the harness meta-check) fails the build the moment an `Instant::now()` appears
//! outside this file.
//!
//! [`Deadline`] is a plain value — an absolute instant plus the arithmetic callers
//! actually need ([`Deadline::remaining`], [`Deadline::expired`]). That keeps one
//! type serving both worlds: a sync loop polls `expired()` between `try_wait`s, and
//! an async caller hands `remaining()` to `tokio::time::timeout`. Neither needs an
//! async-aware clock trait, so the seam adds no runtime coupling.

#[cfg(any(test, feature = "test-util"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// A source of monotonic time. `Send + Sync` so a lane actor or an axum handler can
/// hold one behind an `Arc` and share it across tasks.
pub trait Clock: Send + Sync {
    /// The current monotonic instant.
    fn now(&self) -> Instant;

    /// A deadline `budget` from now.
    fn deadline(&self, budget: Duration) -> Deadline {
        Deadline::at(self.now() + budget)
    }

    /// Wall-clock time, for **labelling only** — a log line's timestamp, never a
    /// budget.
    ///
    /// [`now`](Self::now) is monotonic and unmappable to a calendar, so a UI that
    /// draws "14:02:11" beside a log line needs this. It is on the seam rather than
    /// beside the caller for the seam's own reason: one place reads the host clock,
    /// so a test can freeze what it renders instead of asserting around a moving
    /// value. Nothing waits on it — a wall clock steps backwards over NTP and would
    /// hang or expire a budget early.
    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// The real monotonic clock. The only implementation that reaches `Instant::now()`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A hand-driven clock for tests: time advances only via [`TestClock::advance`].
///
/// Lock-free (an atomic nanosecond offset from a fixed base) so it is `Sync` without
/// a mutex, and so advancing it from another thread mid-test cannot deadlock against
/// the code under test.
///
/// Test-only, and gated so it cannot escape into the shipped binary: a never-advancing
/// clock reachable from production is a lane or drain ladder that never times out.
/// Other crates reach it by depending on `grove-ops` with `features = ["test-util"]`
/// under `[dev-dependencies]` only.
#[cfg(any(test, feature = "test-util"))]
#[derive(Debug)]
pub struct TestClock {
    base: Instant,
    offset_nanos: AtomicU64,
}

#[cfg(any(test, feature = "test-util"))]
impl TestClock {
    /// A clock frozen at the moment of construction.
    #[must_use]
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            offset_nanos: AtomicU64::new(0),
        }
    }

    /// Move time forward by `delta`. Saturates rather than panicking on an absurd
    /// step — a test that overflows 584 years of nanoseconds has a different bug.
    pub fn advance(&self, delta: Duration) {
        let nanos = u64::try_from(delta.as_nanos()).unwrap_or(u64::MAX);
        self.offset_nanos.fetch_add(nanos, Ordering::Relaxed);
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Clock for TestClock {
    fn now(&self) -> Instant {
        self.base + Duration::from_nanos(self.offset_nanos.load(Ordering::Relaxed))
    }

    /// A fixed calendar origin plus whatever has been advanced, so anything that
    /// *renders* a timestamp renders the same bytes on every run.
    fn wall(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(TEST_EPOCH_SECS)
            + Duration::from_nanos(self.offset_nanos.load(Ordering::Relaxed))
    }
}

/// The [`TestClock`]'s calendar origin: 2020-01-01T00:00:00Z. Arbitrary, fixed, and
/// far from every boundary a formatter might round across.
#[cfg(any(test, feature = "test-util"))]
pub const TEST_EPOCH_SECS: u64 = 1_577_836_800;

/// An absolute point a budget expires at. Pass this down instead of a `Duration`:
/// a `Duration` re-based at each layer silently multiplies the budget by the depth
/// of the call chain, which is exactly the bug the v1 per-op timeout literals had.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Deadline {
    at: Instant,
}

impl Deadline {
    /// A deadline at an absolute instant.
    #[must_use]
    pub fn at(at: Instant) -> Self {
        Self { at }
    }

    /// The absolute instant this deadline fires at.
    #[must_use]
    pub fn instant(&self) -> Instant {
        self.at
    }

    /// Time left on `clock`; `Duration::ZERO` once passed (never negative, so it
    /// drops straight into `tokio::time::timeout` / a poll interval).
    #[must_use]
    pub fn remaining(&self, clock: &(impl Clock + ?Sized)) -> Duration {
        self.at.saturating_duration_since(clock.now())
    }

    /// Whether `clock` has reached this deadline.
    #[must_use]
    pub fn expired(&self, clock: &(impl Clock + ?Sized)) -> bool {
        clock.now() >= self.at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_is_frozen_until_advanced() {
        let clock = TestClock::new();
        let first = clock.now();
        assert_eq!(clock.now(), first, "time must not move on its own");
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.now(), first + Duration::from_secs(1));
    }

    #[test]
    fn deadline_counts_down_and_expires_on_the_test_clock() {
        let clock = TestClock::new();
        let deadline = clock.deadline(Duration::from_secs(60));

        assert_eq!(deadline.remaining(&clock), Duration::from_secs(60));
        assert!(!deadline.expired(&clock));

        clock.advance(Duration::from_secs(59));
        assert_eq!(deadline.remaining(&clock), Duration::from_secs(1));
        assert!(!deadline.expired(&clock));

        clock.advance(Duration::from_secs(1));
        assert!(deadline.expired(&clock));
    }

    /// A passed deadline reports zero, not a wrapped/negative duration — callers
    /// feed `remaining()` straight to a timeout without a guard.
    #[test]
    fn remaining_saturates_at_zero() {
        let clock = TestClock::new();
        let deadline = clock.deadline(Duration::from_secs(1));
        clock.advance(Duration::from_secs(10));
        assert_eq!(deadline.remaining(&clock), Duration::ZERO);
    }

    /// A deadline handed down a call chain keeps its absolute instant: re-deriving
    /// a budget at each layer is what multiplies a 60 s cap into 180 s.
    #[test]
    fn deadline_is_absolute_not_rebased() {
        let clock = TestClock::new();
        let outer = clock.deadline(Duration::from_secs(10));
        clock.advance(Duration::from_secs(4));
        let inner = outer; // handed down, not recomputed
        assert_eq!(inner.instant(), outer.instant());
        assert_eq!(inner.remaining(&clock), Duration::from_secs(6));
    }

    /// The wall clock is a *label*, and on the test clock it is a deterministic one:
    /// frozen at the fixed origin, moving only with `advance`. That is what lets a
    /// rendered timestamp be asserted byte for byte instead of merely bounded.
    #[test]
    fn the_test_clock_stamps_a_fixed_calendar_origin() {
        let clock = TestClock::new();
        let epoch = |c: &TestClock| {
            c.wall()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        assert_eq!(epoch(&clock), TEST_EPOCH_SECS);
        clock.advance(Duration::from_secs(90));
        assert_eq!(epoch(&clock), TEST_EPOCH_SECS + 90);
        // …and the real one is the host's, which is only ever *after* that origin.
        assert!(SystemClock.wall() > SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn system_clock_moves_forward() {
        let clock = SystemClock;
        let first = clock.now();
        let deadline = clock.deadline(Duration::from_millis(0));
        assert!(deadline.expired(&clock));
        assert!(clock.now() >= first);
    }

    /// The trait is dyn-compatible and `Sync`, and `Deadline` reads through a
    /// `?Sized` clock: the daemon shares one `Arc<dyn Clock>` across every lane.
    #[test]
    fn clock_is_shareable_across_threads_as_a_trait_object() {
        let clock = std::sync::Arc::new(TestClock::new());
        let erased: std::sync::Arc<dyn Clock> = clock.clone();
        let deadline = erased.deadline(Duration::from_secs(5));
        let mover = std::sync::Arc::clone(&clock);
        std::thread::spawn(move || mover.advance(Duration::from_secs(5)))
            .join()
            .unwrap();
        assert!(deadline.expired(&*erased));
    }
}
