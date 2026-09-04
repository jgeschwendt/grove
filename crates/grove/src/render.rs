//! What the commands print, and the exit verdicts they draw from the same reports.
//!
//! Nothing machine-parses grove's stdout today, and nothing should start: the API is
//! the machine surface. These shapes are still stable and still tested, because an
//! operator reads them at 3am and a changed line is a changed answer.
//!
//! Every renderer here takes **typed** input. v1 matched raw wire strings
//! (`"cloned"`, `"present"`, `"failed"`) inside `print_applied` and
//! `fail_if_any_failed`, so a rename in `grove-ops` would have silently fallen through
//! to the catch-all arm and turned a failed apply into a success. The enums make that
//! unrepresentable — every match below is total.

use grove_api::RootStatusEntry;
use grove_ops::doctor::{Check, CheckKind};
use grove_ops::env::ShareOutcome;
use grove_ops::git::FastForward;
use grove_ops::pool::PoolStatus;
use grove_ops::roots::{Applied, SyncReport};
use grove_ops::wire::ReconcileStatus;

use crate::CliError;

/// One line per root touched by an apply or a `clone add`. Failures go to stderr:
/// they are the half an operator pipes somewhere else.
pub fn applied(rows: &[Applied]) {
    for row in rows {
        match row.status {
            ReconcileStatus::Cloned => println!(
                "cloned  {} ({})",
                row.slug,
                row.default_branch.as_deref().unwrap_or("?")
            ),
            ReconcileStatus::Present => println!("present {}", row.slug),
            ReconcileStatus::Failed => eprintln!(
                "failed  {}: {}",
                row.slug,
                row.error.as_deref().unwrap_or("unknown")
            ),
        }
    }
}

/// Exit 1 when any root failed to realize. Typed, not string-matched: a new
/// [`ReconcileStatus`] must be classified here before this compiles.
pub fn fail_if_any_failed(rows: &[Applied]) -> Result<(), CliError> {
    if rows
        .iter()
        .any(|row| matches!(row.status, ReconcileStatus::Failed))
    {
        Err(CliError::Api(
            "one or more repositories failed to apply".into(),
        ))
    } else {
        Ok(())
    }
}

/// What an in-process `grove sync` did — the offline arm's whole output.
///
/// Only the offline arm renders this. With a daemon up the sync is accept-only, so
/// there is no report to print and the operator reads `grove tree list` or a UI
/// instead; printing a fabricated one there would be the CLI answering a question the
/// contract deliberately does not.
///
/// The lines, in the order an operator reads them: what happened to the trunk, then
/// where it now sits, then what the prune cost the warm pool. The tip and the prune
/// count are indented because they are consequences of the first line, not peers of it.
pub fn sync_report(slug: &str, report: &SyncReport) {
    let fetched = if report.fetched {
        "fetched"
    } else {
        "not fetched"
    };
    println!("synced {slug}: {fetched}, trunk {}", trunk(report.trunk));
    println!("  tip {}", report.tip);
    // Silent at zero: a steady-state sync prunes nothing, and a line saying so every
    // time trains an operator to skip the place the interesting number appears.
    match report.stale_slots_pruned {
        0 => {}
        1 => println!("  1 stale pool slot recycled"),
        n => println!("  {n} stale pool slots recycled"),
    }
}

/// A fast-forward outcome as an operator reads it. Total over [`FastForward`], so a
/// new outcome must be given words here before it can ship — and the two that leave
/// the trunk untouched say *why* nothing moved, because carried law 9 makes that the
/// answer rather than a failure.
const fn trunk(outcome: FastForward) -> &'static str {
    match outcome {
        FastForward::Updated => "updated",
        FastForward::AlreadyCurrent => "already current",
        FastForward::Diverged => "diverged — local commits kept, nothing forced",
        FastForward::Dirty => "dirty — uncommitted tracked files kept, nothing forced",
    }
}

/// The share half of a doctor report — one row per declared share path.
pub fn report(rows: &[ShareOutcome]) {
    if rows.is_empty() {
        println!("no shares declared");
    }
    for row in rows {
        let at = row
            .worktree
            .as_deref()
            .map_or(String::new(), |w| format!(" @{w}"));
        let why = row
            .reason
            .as_deref()
            .map_or(String::new(), |r| format!(" — {r}"));
        println!("{} {}{at}: {:?}{why}", row.slug, row.path, row.status);
    }
}

/// Pool levels, flagging under-fill — the operator's recovery signal when a
/// background refill failed (it retries on the next event, not a timer).
pub fn pools(rows: &[PoolStatus]) {
    for row in rows {
        let flag = if row.observed < row.target as usize {
            " (under-filled)"
        } else {
            ""
        };
        println!("pool {}: {}/{}{flag}", row.slug, row.observed, row.target);
    }
}

/// Per-root engine status, degraded-first — the operator's recovery signal when a
/// clone wedged. Steady `ready` roots stay quiet; only non-`ready` rows print.
pub fn statuses(rows: &[RootStatusEntry]) {
    let mut rows: Vec<_> = rows
        .iter()
        .filter(|row| row.status != grove_api::RootStatus::Ready)
        .collect();
    rows.sort_by(|a, b| {
        a.status
            .as_str()
            .cmp(b.status.as_str())
            .then_with(|| a.slug.cmp(&b.slug))
    });
    for row in rows {
        println!("root {}: {}", row.slug, row.status.as_str());
    }
}

/// The git-plumbing pass, grouped by what was inspected.
///
/// Grouped rather than interleaved because the kinds answer different questions — "is
/// the manifest sound", "is this root's git there", "does declared match actual" — and
/// an operator scans for the group, not the row. Passing checks are summarized in one
/// line per kind; findings print in full, because a finding is the reason to run this.
pub fn checks(rows: &[Check]) {
    for kind in CheckKind::ALL {
        let group: Vec<&Check> = rows.iter().filter(|row| row.check == kind).collect();
        if group.is_empty() {
            continue;
        }
        let findings: Vec<&&Check> = group.iter().filter(|row| row.is_finding()).collect();
        if findings.is_empty() {
            println!("check {}: {} ok", label(kind), group.len());
            continue;
        }
        println!(
            "check {}: {} of {} need attention",
            label(kind),
            findings.len(),
            group.len()
        );
        for row in findings {
            let at = [row.slug.as_deref(), row.name.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let at = if at.is_empty() {
                String::new()
            } else {
                format!(" {at}")
            };
            let why = row
                .detail
                .as_deref()
                .map_or(String::new(), |d| format!(" — {d}"));
            println!("  {:?}{at}{why}", row.status);
        }
    }
}

/// A check kind's operator-facing name. Total over [`CheckKind`], so a new kind must
/// be named here before it can ship.
const fn label(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Manifest => "manifest",
        CheckKind::InstallUnderHome => "install-under-home",
        CheckKind::Root => "root",
        CheckKind::Bare => "bare",
        CheckKind::Trunk => "trunk",
        CheckKind::Worktree => "worktree",
        CheckKind::Drift => "drift",
    }
}

/// Doctor's verdict, carried from v1 exactly: any errored share → exit 1; else N
/// unresolved conflicts → exit 5 (`Conflict`, a usable CI gate even under
/// `--dry-run`); else success.
///
/// The plumbing [`checks`] are **report-only** and deliberately exit-neutral, for the
/// reason `grove_ops::doctor` states: a `missing`/`undeclared`/`mismatch` finding is
/// either drift the reconciler already owns or a human's edit to reconcile with, and a
/// `grove doctor` that exits non-zero on ordinary transient drift is a gate nobody can
/// wire into anything.
pub fn fail_on_unresolved(rows: &[ShareOutcome]) -> Result<(), CliError> {
    if rows.iter().any(|row| row.status.is_error()) {
        Err(CliError::Api("doctor: one or more checks errored".into()))
    } else if let n @ 1.. = rows.iter().filter(|row| row.status.is_conflict()).count() {
        Err(CliError::Conflict(format!("{n} unresolved conflict(s)")))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{fail_if_any_failed, fail_on_unresolved};
    use grove_ops::env::{ShareOutcome, ShareStatus};
    use grove_ops::roots::Applied;
    use grove_ops::wire::ReconcileStatus;

    fn share(status: ShareStatus) -> ShareOutcome {
        ShareOutcome {
            slug: "o/r".into(),
            worktree: Some("feat".into()),
            path: ".env".into(),
            status,
            reason: None,
        }
    }

    fn applied(status: ReconcileStatus) -> Applied {
        Applied {
            slug: "o/r".into(),
            status,
            default_branch: Some("main".into()),
            error: None,
        }
    }

    /// v1's `doctor_exit_code_is_conflict_on_unresolved`, carried whole.
    #[test]
    fn doctor_exit_code_is_conflict_on_unresolved() {
        assert!(fail_on_unresolved(&[share(ShareStatus::Ok), share(ShareStatus::Linked)]).is_ok());
        assert_eq!(
            fail_on_unresolved(&[share(ShareStatus::Conflict)])
                .unwrap_err()
                .exit_code(),
            5
        );
        assert_eq!(
            fail_on_unresolved(&[share(ShareStatus::Error)])
                .unwrap_err()
                .exit_code(),
            1
        );
        // An error outranks a conflict (exit 1 wins).
        assert_eq!(
            fail_on_unresolved(&[share(ShareStatus::Conflict), share(ShareStatus::Error)])
                .unwrap_err()
                .exit_code(),
            1
        );
    }

    /// The conflict count is the message, so a CI log says how much work is left.
    #[test]
    fn the_conflict_count_reaches_the_message() {
        let err = fail_on_unresolved(&[share(ShareStatus::Conflict), share(ShareStatus::Conflict)])
            .unwrap_err();
        assert!(err.to_string().starts_with("2 unresolved"), "{err}");
    }

    /// The v1 defect this closes: the verdict read `a.status == "failed"`, so a
    /// renamed wire spelling would have made a failed apply exit 0. Typed now.
    #[test]
    fn a_failed_root_is_exit_1_and_the_others_are_not() {
        assert!(
            fail_if_any_failed(&[
                applied(ReconcileStatus::Cloned),
                applied(ReconcileStatus::Present)
            ])
            .is_ok()
        );
        let err = fail_if_any_failed(&[
            applied(ReconcileStatus::Present),
            applied(ReconcileStatus::Failed),
        ])
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    /// Every fast-forward outcome is given words, and the two that leave the trunk
    /// untouched say so — carried law 9 makes "nothing moved" the answer, and an
    /// operator who reads only `diverged` has no idea whether their commits survived.
    #[test]
    fn every_trunk_outcome_reads_as_prose_and_the_untouched_ones_say_so() {
        use grove_ops::git::FastForward;

        for outcome in [
            FastForward::Updated,
            FastForward::AlreadyCurrent,
            FastForward::Diverged,
            FastForward::Dirty,
        ] {
            assert!(!super::trunk(outcome).is_empty(), "{outcome:?}");
        }
        for outcome in [FastForward::Diverged, FastForward::Dirty] {
            assert!(
                super::trunk(outcome).contains("nothing forced"),
                "{outcome:?}: {}",
                super::trunk(outcome)
            );
        }
    }

    /// Every check kind has an operator-facing label — the match is total, so this is
    /// a guard on the labels themselves being distinct and non-empty rather than on
    /// coverage, which the compiler already owns.
    #[test]
    fn every_check_kind_has_a_distinct_label() {
        let mut labels: Vec<&str> = grove_ops::doctor::CheckKind::ALL
            .iter()
            .map(|kind| super::label(*kind))
            .collect();
        assert!(labels.iter().all(|l| !l.is_empty()));
        labels.sort_unstable();
        let len = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), len);
    }
}
