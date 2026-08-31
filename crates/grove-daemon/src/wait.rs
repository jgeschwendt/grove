//! Awaiting a [`Deadline`] that may be driven by a hand-wound clock.
//!
//! Every wall-clock budget in grove is a `Deadline` read off the injected
//! [`Clock`] (carried law: the clock seam), and the daemon's two timers — the lane
//! idle-reap and the watcher's debounce — are no exception. But `tokio::time` has
//! its own clock, so `sleep(deadline.remaining(&clock))` under a [`TestClock`]
//! would sleep the *real* 60 seconds a test just tried to skip.
//!
//! So the wait is a poll: sleep at most [`TICK`], re-read the injected clock, stop
//! once it has passed the deadline. Under `SystemClock` that costs one timer
//! wake-up per tick while a timer is armed and is otherwise exact; under
//! `TestClock` an `advance()` is observed within one tick, which is what makes a
//! "60 s idle reap" test cost milliseconds.
//!
//! The alternative — an async-aware clock trait with its own timer — would put a
//! second scheduler behind every budget in the tree to save a handful of wake-ups
//! on a process that is idle by definition while one is armed.
//!
//! [`TestClock`]: grove_ops::clock::TestClock

use std::time::Duration;

use grove_ops::clock::{Clock, Deadline};

/// The longest a wait sleeps before re-reading the injected clock. Also the upper
/// bound on how late a fake-clock `advance()` is noticed.
pub const TICK: Duration = Duration::from_millis(50);

/// Resolve once `clock` has reached `deadline` — immediately, if it already has.
///
/// Cancellation-safe: the deadline is absolute, so a caller that drops this future
/// in a `select!` and rebuilds it next iteration waits exactly as long in total.
pub async fn until(deadline: Deadline, clock: &(impl Clock + ?Sized)) {
    loop {
        let remaining = deadline.remaining(clock);
        if remaining.is_zero() {
            return;
        }
        tokio::time::sleep(remaining.min(TICK)).await;
    }
}

/// Run `work` under `deadline`, or give up on it — `None` when the clock passed the
/// deadline first.
///
/// `tokio::time::timeout(deadline.remaining(clock), …)` looks equivalent and is not:
/// it reads the injected clock once and then waits on tokio's own, so a `TestClock`
/// advance never shortens it and the budget costs its real wall time in every test
/// that exercises it. This races [`until`] instead, so the budget is the seam's.
///
/// Giving up drops `work`. Every caller here is a *read* whose lane job runs to
/// completion regardless (see `Lanes::run`), so nothing is left half-applied.
pub async fn within<T>(
    deadline: Deadline,
    clock: &(impl Clock + ?Sized),
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        out = work => Some(out),
        () = until(deadline, clock) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{TICK, until, within};
    use grove_ops::clock::{Clock, SystemClock, TestClock};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn a_passed_deadline_resolves_immediately() {
        let clock = SystemClock;
        let deadline = clock.deadline(Duration::ZERO);
        tokio::time::timeout(Duration::from_secs(1), until(deadline, &clock))
            .await
            .expect("an expired deadline does not wait");
    }

    /// The property the seam exists for: a minute-long budget is skipped by moving
    /// the fake clock, not by waiting a minute.
    #[tokio::test]
    async fn a_fake_clock_advance_ends_the_wait_within_a_tick() {
        let clock = Arc::new(TestClock::new());
        let deadline = clock.deadline(Duration::from_secs(60));
        let mover = Arc::clone(&clock);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            mover.advance(Duration::from_secs(60));
        });

        tokio::time::timeout(TICK * 20, until(deadline, &*clock))
            .await
            .expect("the advance is observed within a tick");
    }

    /// A budget that outlives its work returns the answer; one the fake clock runs
    /// past gives up — without the test waiting the budget out.
    #[tokio::test]
    async fn a_budget_is_the_injected_clocks_and_not_tokios() {
        let clock = Arc::new(TestClock::new());
        let done = within(clock.deadline(Duration::from_secs(30)), &*clock, async {
            7
        })
        .await;
        assert_eq!(done, Some(7));

        let deadline = clock.deadline(Duration::from_secs(30));
        let mover = Arc::clone(&clock);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            mover.advance(Duration::from_secs(30));
        });
        let never = within(deadline, &*clock, std::future::pending::<u8>()).await;
        assert_eq!(
            never, None,
            "the advance ended a 30 s budget in milliseconds"
        );
    }

    /// And it does *not* end early: a wait on a frozen clock outlives several ticks.
    #[tokio::test]
    async fn a_frozen_clock_never_reaches_the_deadline() {
        let clock = TestClock::new();
        let deadline = clock.deadline(Duration::from_secs(60));
        assert!(
            tokio::time::timeout(TICK * 3, until(deadline, &clock))
                .await
                .is_err(),
            "time must not move on its own"
        );
    }
}
