//! The status transition table — every status change in the engine goes through
//! this one function.
//!
//! The question it answers is always the same: *preserve the transient the driver
//! owns, or trust disk?* Getting it wrong in either direction is a real defect —
//! trust disk too eagerly and an in-flight clone reads `missing`, so the next event
//! dispatches a second one; preserve too eagerly and a root that finished cloning
//! stays `cloning` forever. v1 answered it in one five-clause table so no call site
//! could answer it differently; this is that table, ported clause for clause.

use std::path::Path;

use grove_api::status::RootStatus;

/// What disk says about a root, and the *only* filesystem read the engine does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiskStatus {
    /// The bare clone and the trunk checkout are both there.
    Ready,
    /// One or both are absent.
    Missing,
}

/// Why the status is being recomputed.
///
/// The disk verdict rides on the variants that consult it, so the dispatch case
/// cannot accidentally be handed one — and cannot pay for a filesystem read it does
/// not use. v1 passed `nil` there and relied on clause order to ignore it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transition {
    /// A reconcile just went out. Disk is deliberately not consulted: the dispatch
    /// precedes any read, and the point is to surface the *intent* to clone.
    ReconcileDispatched,
    /// An event asked for a fresh derivation.
    Derive(DiskStatus),
    /// A reconcile ended in a transport-level error rather than an outcome.
    ReconcileError(DiskStatus),
}

/// `next_status(current, transition)` — the whole state machine.
///
/// - **`ReconcileDispatched`** — a root not yet on disk (`missing`/`unknown`/
///   `degraded`, including a degraded root retrying its clone) surfaces `cloning`;
///   anything else is unchanged.
/// - **`Derive`** — disk `ready` always wins upward, because a completed clone is
///   authoritative. Otherwise a driver-owned transient (`cloning`, `degraded`) is
///   preserved rather than masked as `missing`, and everything else takes the honest
///   disk verdict.
/// - **`ReconcileError`** — the attempt is over, so trust disk *plainly*: preserving
///   `cloning` here would strand a transient that nothing is going to clear.
#[must_use]
// stele:landmark status-is-a-cache
pub const fn next_status(current: RootStatus, transition: Transition) -> RootStatus {
    match transition {
        Transition::ReconcileDispatched => match current {
            RootStatus::Missing | RootStatus::Unknown | RootStatus::Degraded => RootStatus::Cloning,
            RootStatus::Ready | RootStatus::Cloning | RootStatus::Unavailable => current,
        },
        Transition::Derive(DiskStatus::Ready) => RootStatus::Ready,
        Transition::Derive(DiskStatus::Missing) => match current {
            RootStatus::Cloning => RootStatus::Cloning,
            RootStatus::Degraded => RootStatus::Degraded,
            RootStatus::Ready
            | RootStatus::Missing
            | RootStatus::Unknown
            | RootStatus::Unavailable => RootStatus::Missing,
        },
        Transition::ReconcileError(disk) => match disk {
            DiskStatus::Ready => RootStatus::Ready,
            DiskStatus::Missing => RootStatus::Missing,
        },
    }
}

/// The engine's one filesystem read: a root is `Ready` on disk when its bare clone
/// **and** its trunk checkout are both present.
///
/// Both, not either: a bare with no trunk checkout is exactly the half-realized state
/// reconcile exists to finish, and calling it ready would leave the share source —
/// and every worktree link into it — dangling.
#[must_use]
pub fn disk_status(home: &Path, slug: &str) -> DiskStatus {
    if grove_ops::roots::bare_dir(home, slug).is_dir()
        && grove_ops::roots::trunk_dir(home, slug).is_dir()
    {
        DiskStatus::Ready
    } else {
        DiskStatus::Missing
    }
}

#[cfg(test)]
mod tests {
    use super::{DiskStatus, Transition, disk_status, next_status};
    use grove_api::status::RootStatus;
    use grove_ops::testfix;
    use tempfile::TempDir;

    const ALL: [RootStatus; 6] = RootStatus::ALL;

    /// A dispatched reconcile surfaces `cloning` from exactly the three not-on-disk
    /// states, and leaves every other status alone.
    #[test]
    fn a_dispatched_reconcile_clones_from_missing_unknown_and_degraded() {
        for status in [
            RootStatus::Missing,
            RootStatus::Unknown,
            RootStatus::Degraded,
        ] {
            assert_eq!(
                next_status(status, Transition::ReconcileDispatched),
                RootStatus::Cloning,
                "{status} must surface cloning"
            );
        }
        for status in [
            RootStatus::Ready,
            RootStatus::Cloning,
            RootStatus::Unavailable,
        ] {
            assert_eq!(
                next_status(status, Transition::ReconcileDispatched),
                status,
                "{status} must be left alone"
            );
        }
    }

    /// Disk `ready` wins from every current status — a completed clone is
    /// authoritative, and no transient outranks it.
    #[test]
    fn a_ready_disk_always_wins_upward() {
        for status in ALL {
            assert_eq!(
                next_status(status, Transition::Derive(DiskStatus::Ready)),
                RootStatus::Ready,
                "{status} + disk ready"
            );
        }
    }

    /// A missing disk preserves the two statuses the driver owns and reports the
    /// honest `missing` for everything else. This is the clause that keeps a root
    /// mid-clone from reading `missing` — which is what would dispatch a second
    /// clone on the next event.
    #[test]
    fn a_missing_disk_preserves_the_driver_owned_transients() {
        assert_eq!(
            next_status(RootStatus::Cloning, Transition::Derive(DiskStatus::Missing)),
            RootStatus::Cloning
        );
        assert_eq!(
            next_status(
                RootStatus::Degraded,
                Transition::Derive(DiskStatus::Missing)
            ),
            RootStatus::Degraded
        );
        for status in [
            RootStatus::Ready,
            RootStatus::Missing,
            RootStatus::Unknown,
            RootStatus::Unavailable,
        ] {
            assert_eq!(
                next_status(status, Transition::Derive(DiskStatus::Missing)),
                RootStatus::Missing,
                "{status} + disk missing"
            );
        }
    }

    /// A transport error ends the attempt, so disk is trusted plainly — `cloning` is
    /// *not* preserved here, unlike under `Derive`. The asymmetry is the whole point
    /// of having two events instead of one.
    #[test]
    fn a_reconcile_error_trusts_disk_plainly() {
        for status in ALL {
            assert_eq!(
                next_status(status, Transition::ReconcileError(DiskStatus::Missing)),
                RootStatus::Missing,
                "{status} must not keep a stale transient"
            );
            assert_eq!(
                next_status(status, Transition::ReconcileError(DiskStatus::Ready)),
                RootStatus::Ready
            );
        }
        assert_ne!(
            next_status(
                RootStatus::Cloning,
                Transition::ReconcileError(DiskStatus::Missing)
            ),
            next_status(RootStatus::Cloning, Transition::Derive(DiskStatus::Missing)),
            "derive preserves the transient; a reconcile error must not"
        );
    }

    /// The table is total by construction — every match is exhaustive and there is no
    /// catch-all — and this walks the whole cross product to prove no input can
    /// produce a status outside the published vocabulary.
    #[test]
    fn the_table_is_total_over_every_status_and_transition() {
        for status in ALL {
            for transition in [
                Transition::ReconcileDispatched,
                Transition::Derive(DiskStatus::Ready),
                Transition::Derive(DiskStatus::Missing),
                Transition::ReconcileError(DiskStatus::Ready),
                Transition::ReconcileError(DiskStatus::Missing),
            ] {
                let next = next_status(status, transition);
                assert!(
                    ALL.contains(&next),
                    "{status} + {transition:?} left the vocabulary"
                );
                // `unknown` is an entry state the table never returns to, and
                // `unavailable` is only ever *preserved* (a dispatch against a root
                // whose engine could not be reached), never introduced.
                assert_ne!(next, RootStatus::Unknown, "{status} + {transition:?}");
                assert!(
                    next != RootStatus::Unavailable || next == status,
                    "{status} + {transition:?} invented an unavailable"
                );
            }
        }
    }

    /// A status is idempotent under re-derivation with an unchanged disk — the
    /// property that makes a burst of manifest events cost nothing.
    #[test]
    fn re_deriving_an_unchanged_disk_is_a_fixed_point() {
        for status in ALL {
            for disk in [DiskStatus::Ready, DiskStatus::Missing] {
                let once = next_status(status, Transition::Derive(disk));
                assert_eq!(once, next_status(once, Transition::Derive(disk)));
            }
        }
    }

    /// The disk read itself, against real fixtures: a realized root is `Ready`, an
    /// undeclared one is `Missing`, and a bare whose trunk checkout vanished is `Missing`
    /// rather than half-ready.
    #[test]
    fn disk_status_wants_both_the_bare_and_the_trunk() {
        let tmp = TempDir::new().unwrap();
        let home = testfix::home_with_root(&tmp);
        assert_eq!(disk_status(&home, testfix::SLUG), DiskStatus::Ready);
        assert_eq!(disk_status(&home, "o/never-declared"), DiskStatus::Missing);

        std::fs::remove_dir_all(testfix::trunk_dir(&home, testfix::SLUG)).unwrap();
        assert_eq!(
            disk_status(&home, testfix::SLUG),
            DiskStatus::Missing,
            "a bare with no trunk checkout is not ready — that is what reconcile finishes"
        );
    }
}
