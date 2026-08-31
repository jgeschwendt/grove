//! The daemon lifecycle axis: `booting → ready → stopping`, with a sticky
//! `degraded(reason)` beside it.
//!
//! Read by the readiness guard, `GET /api/health` and `GET /api/daemon/version`
//! (uptime). Written at exactly three moments: `mark_ready` once the process is
//! serving, `mark_stopping` from the shutdown route before it answers, and
//! `mark_degraded` from whatever discovers the daemon cannot do its job.

use std::sync::RwLock;
use std::time::Instant;

use grove_api::BootStatus;
use grove_ops::clock::Clock;

/// The lifecycle state plus the instant this process started serving.
///
/// `RwLock` rather than a channel: every reader is a request handler asking one
/// question, the writers are three rare transitions, and the answer must be visible
/// to the *next* line of the writer (the shutdown route marks stopping and then
/// answers, and nothing may slip past the readiness gate in between).
#[derive(Debug)]
pub struct BootState {
    status: RwLock<BootStatus>,
    started: Instant,
}

impl BootState {
    /// A fresh `booting` state, its uptime origin read off `clock`.
    #[must_use]
    pub fn new(clock: &(impl Clock + ?Sized)) -> Self {
        Self {
            status: RwLock::new(BootStatus::Booting),
            started: clock.now(),
        }
    }

    /// The current state. Clones — the degraded variant carries its reason.
    #[must_use]
    pub fn status(&self) -> BootStatus {
        self.read().clone()
    }

    /// Whether non-whitelisted routes may serve.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.read().is_ready()
    }

    /// How long this process has been up, per `clock`.
    #[must_use]
    pub fn uptime_ms(&self, clock: &(impl Clock + ?Sized)) -> u64 {
        u64::try_from(
            clock
                .now()
                .saturating_duration_since(self.started)
                .as_millis(),
        )
        .unwrap_or(u64::MAX)
    }

    /// Enter `ready` — **unless already degraded**.
    ///
    /// The stickiness is the point (v1 hazard `boot-degrade-sticky`): a degrade
    /// recorded while the process was coming up must survive the unconditional
    /// "the tree is started, we are ready" signal that follows it. Otherwise a
    /// daemon that came up unable to do its job would answer `/api/health` 200 and
    /// the self-update health gate would accept the bundle that broke it.
    // stele:landmark boot-degrade-sticky
    pub fn mark_ready(&self) {
        let mut status = self.write();
        if matches!(*status, BootStatus::Degraded { .. }) {
            return;
        }
        *status = BootStatus::Ready;
    }

    /// Enter `stopping`. Overrides a degrade: a daemon asked to drain *is* draining,
    /// and `off`'s ladder reads that state to know the request was accepted. Only
    /// [`mark_ready`](Self::mark_ready) is refused by a degrade.
    pub fn mark_stopping(&self) {
        *self.write() = BootStatus::Stopping;
    }

    /// Record a terminal fault with the reason `/api/health` will publish.
    pub fn mark_degraded(&self, reason: impl Into<String>) {
        *self.write() = BootStatus::Degraded {
            reason: reason.into(),
        };
    }

    /// A poisoned lock is not a reason to stop answering health: the guarded value
    /// is a plain enum, so no invariant can have been left half-written, and the
    /// alternative — panicking every handler — turns one panicked writer into a
    /// dead daemon.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, BootStatus> {
        self.status
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BootStatus> {
        self.status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::BootState;
    use grove_api::BootStatus;
    use grove_ops::clock::{Clock, TestClock};
    use std::time::Duration;

    #[test]
    fn a_fresh_state_is_booting_and_not_ready() {
        let state = BootState::new(&TestClock::new());
        assert_eq!(state.status(), BootStatus::Booting);
        assert!(!state.is_ready());
    }

    #[test]
    fn the_lifecycle_runs_booting_ready_stopping() {
        let state = BootState::new(&TestClock::new());
        state.mark_ready();
        assert_eq!(state.status(), BootStatus::Ready);
        assert!(state.is_ready());
        state.mark_stopping();
        assert_eq!(state.status(), BootStatus::Stopping);
        assert!(!state.is_ready());
    }

    /// The carried hazard: `mark_ready` never clears a degrade, so a fault recorded
    /// during boot survives the post-boot ready signal that follows it.
    #[test]
    fn a_degrade_survives_mark_ready() {
        let state = BootState::new(&TestClock::new());
        state.mark_degraded("ops_incompatible");

        state.mark_ready();

        assert_eq!(
            state.status(),
            BootStatus::Degraded {
                reason: "ops_incompatible".into()
            },
            "mark_ready must not clear a degrade"
        );
        assert!(!state.is_ready());

        // And it keeps surviving — the stickiness is not a one-shot latch that a
        // second ready signal gets past.
        state.mark_ready();
        assert!(matches!(state.status(), BootStatus::Degraded { .. }));
    }

    /// The one transition a degrade does *not* block: an operator asking a degraded
    /// daemon to stop must see it draining, or `grove off` cannot tell whether the
    /// shutdown request landed.
    #[test]
    fn a_degrade_does_not_block_mark_stopping() {
        let state = BootState::new(&TestClock::new());
        state.mark_degraded("ops_incompatible");
        state.mark_stopping();
        assert_eq!(state.status(), BootStatus::Stopping);
    }

    /// A degrade recorded *after* readiness replaces it — the fault is the newer,
    /// more specific fact.
    #[test]
    fn a_degrade_overrides_ready() {
        let state = BootState::new(&TestClock::new());
        state.mark_ready();
        state.mark_degraded("watcher_lost");
        assert_eq!(
            state.status(),
            BootStatus::Degraded {
                reason: "watcher_lost".into()
            }
        );
    }

    /// Uptime comes off the injected clock, so `/api/daemon/version` is testable
    /// without a real wait.
    #[test]
    fn uptime_reads_the_injected_clock() {
        let clock = TestClock::new();
        let state = BootState::new(&clock);
        assert_eq!(state.uptime_ms(&clock), 0);
        clock.advance(Duration::from_millis(1500));
        assert_eq!(state.uptime_ms(&clock), 1500);
    }

    /// Shared behind an `Arc` across handler tasks, which is the only way the app
    /// state holds it.
    #[test]
    fn state_is_shareable_across_threads() {
        let clock = std::sync::Arc::new(TestClock::new());
        let erased: std::sync::Arc<dyn Clock> = clock.clone();
        let state = std::sync::Arc::new(BootState::new(&*erased));
        let writer = std::sync::Arc::clone(&state);
        std::thread::spawn(move || writer.mark_degraded("from another thread"))
            .join()
            .unwrap();
        assert!(matches!(state.status(), BootStatus::Degraded { .. }));
    }
}
