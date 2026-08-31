//! Every budget the CLI spends waiting, in one place.
//!
//! v1 wrote these as literals at the call sites that spent them — a `10` beside the
//! reconcile POST, a `30` beside doctor's, two separate `5`s on the two health reads —
//! so nothing could be compared against anything else and the relationships that
//! actually matter (a stop grace that must exceed the daemon's own drain; a probe
//! budget that must stay short enough for a human to feel) were invisible. The values
//! carry over unchanged; only their home is new.
//!
//! Two kinds live here, and they are spent differently:
//!
//! - **Request budgets** bound one HTTP round trip and are handed to reqwest, which
//!   owns the timer. Nothing to fake: a test points the client at a listener that
//!   answers, hangs, or refuses.
//! - **Ladder budgets** bound a *loop* — poll until the port opens, wait for a pid to
//!   exit. Those resolve through the clock seam ([`Clock::deadline`]), never against
//!   a wall-clock read, which is what lets `ServerControl`'s ladders run in
//!   microseconds under a `TestClock`.
//!
//! [`Clock::deadline`]: grove_ops::clock::Clock::deadline

use std::time::Duration;

// ─── request budgets ─────────────────────────────────────────────────────────

/// The reachability probe (v1's `PROBE_BUDGET`).
///
/// Generous on purpose: a refused connection fails immediately regardless, so this
/// only bounds how long the CLI waits on a *slow* daemon before calling it busy. The
/// up/offline decision never hinges on the value — see
/// [`Reachability`](crate::api::Reachability).
pub const PROBE: Duration = Duration::from_secs(2);

/// A health read that wants an answer, not a classification: `grove ok`, and the
/// self-update gate's `ready_version`.
pub const HEALTH: Duration = Duration::from_secs(5);

/// `POST /api/roots/reconcile`. The daemon only schedules the work, so this bounds
/// the acknowledgement rather than any convergence.
pub const NUDGE: Duration = Duration::from_secs(10);

/// The synchronous ops routes — `POST /api/doctor`, `POST /api/roots/remove`,
/// `POST /api/worktrees/remove`. Long because the work runs on the root's lane behind
/// whatever that lane is already doing; the daemon caps its own doctor pass at the
/// same 30 s (`grove_daemon::routes::DOCTOR_BUDGET`), so this is the client half of
/// one budget rather than a second, looser one.
pub const OPS: Duration = Duration::from_secs(30);

/// `GET /api/roots` — the snapshot behind `grove tree list`. New in v2: v1's
/// `tree list` never asked the daemon anything. Shorter than [`OPS`] because a
/// snapshot read that is queued behind a clone is answered `unavailable` by the
/// daemon rather than waited out.
pub const SNAPSHOT: Duration = Duration::from_secs(10);

/// The readiness/identity probes process custody makes against a daemon it is
/// starting or stopping. Deliberately much shorter than [`PROBE`]: these run inside
/// polling ladders that re-ask, so a single slow answer must not eat the ladder's
/// whole budget.
pub const CUSTODY_PROBE: Duration = Duration::from_millis(500);

// ─── ladder budgets ──────────────────────────────────────────────────────────

/// How long `grove on` waits for a freshly launched daemon to answer ready.
pub const READY: Duration = Duration::from_secs(30);

/// How long `grove off` lets the daemon drain on its own before SIGTERM.
///
/// Must exceed `grove_daemon`'s own drain budget, or a graceful stop always ends in
/// the escalation ladder it exists to avoid — pinned from the daemon's side by
/// `the_drain_budget_fits_inside_the_cli_stop_grace`.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// How long `grove off` waits after SIGKILL before reporting the process survived.
pub const KILL: Duration = Duration::from_secs(5);

/// How long the self-update gate waits for the freshly-flipped daemon to report ready
/// **at the new version** before declaring the release bad and rolling back.
///
/// Its floor is [`READY`], not a number picked for the updater: the bounce underneath
/// it is a whole `grove off` + `grove on`, and a gate shorter than the start it is
/// waiting on would roll back every healthy release that merely booted slowly.
pub const HEALTH_GATE: Duration = Duration::from_secs(30);

/// Gap between liveness/readiness polls. The ladders are event-poor — a process
/// either exited or it did not — so this is the one sanctioned busy-wait, bounded by
/// a `Deadline` read off the clock seam.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[cfg(test)]
mod tests {
    use super::{HEALTH_GATE, KILL, OPS, PROBE, READY, STOP_GRACE};

    /// The relationships the constants exist to make checkable. A budget that
    /// violates one of these is not a slow CLI, it is a broken ladder: a stop grace
    /// under the daemon's drain never drains, and a probe as long as the ops budget
    /// makes `grove clone add` feel hung before it has decided anything.
    #[test]
    fn the_budgets_stay_in_their_intended_order() {
        assert!(PROBE < STOP_GRACE, "a probe must not outlast a whole stop");
        assert!(
            STOP_GRACE > grove_daemon::app::DEFAULT_DRAIN,
            "the daemon must be able to finish draining inside `grove off`'s grace"
        );
        assert!(KILL < STOP_GRACE);
        assert!(
            OPS >= STOP_GRACE,
            "a lane job may legitimately outlive a stop"
        );
        assert!(
            HEALTH_GATE >= READY,
            "the update gate waits on a whole restart; shorter than the start it is \
             waiting on and every slow-but-healthy release gets rolled back"
        );
    }
}
