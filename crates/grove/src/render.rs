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

use std::fmt::Write;

use grove_api::{DoctorData, RootStatus};
use grove_ops::doctor::{Check, CheckKind};
use grove_ops::env::ShareOutcome;
use grove_ops::git::FastForward;
use grove_ops::pool::PoolStatus;
use grove_ops::roots::{Applied, SyncReport};
use grove_ops::wire::ReconcileStatus;

use crate::CliError;

/// One line per root touched by an apply or a `clone add`, then exit 1 when any root
/// failed to realize. Failures go to stderr: they are the half an operator pipes
/// somewhere else.
pub fn applied(rows: &[Applied]) -> Result<(), CliError> {
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
    if rows.iter().any(|row| row.status == ReconcileStatus::Failed) {
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
pub fn sync_report(slug: &str, report: &SyncReport) -> String {
    let not = if report.fetched { "" } else { "not " };
    let mut out = format!(
        "synced {slug}: {not}fetched, trunk {}\n  tip {}\n",
        trunk(report.trunk),
        report.tip
    );
    // Silent at zero: a steady-state sync prunes nothing, and a line saying so every
    // time trains an operator to skip the place the interesting number appears.
    if let n @ 1.. = report.stale_slots_pruned {
        let s = if n == 1 { "" } else { "s" };
        let _ = writeln!(out, "  {n} stale pool slot{s} recycled");
    }
    out
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

/// The whole doctor report: shares, pools, non-`ready` roots, then the plumbing checks.
pub fn doctor(d: &DoctorData) -> String {
    let mut out = String::new();
    if d.report.is_empty() {
        out += "no shares declared\n";
    }
    for row in &d.report {
        let at = suffix(" @", row.worktree.as_deref());
        let why = suffix(" — ", row.reason.as_deref());
        let _ = writeln!(out, "{} {}{at}: {:?}{why}", row.slug, row.path, row.status);
    }
    // Under-fill is the operator's recovery signal when a background refill failed (it
    // retries on the next event, not a timer).
    for PoolStatus {
        slug,
        observed,
        target,
    } in &d.pools
    {
        let under = *observed < *target as usize;
        let flag = if under { " (under-filled)" } else { "" };
        let _ = writeln!(out, "pool {slug}: {observed}/{target}{flag}");
    }
    // Degraded-first, the recovery signal when a clone wedged; steady `ready` roots stay
    // quiet. The error is indented under its row: `degraded` alone reads as "the clone
    // broke", and grove's refusals — a legacy layout, an occupied root directory — are
    // the opposite, each naming the command that clears it.
    let mut rows: Vec<_> = d
        .statuses
        .iter()
        .filter(|row| row.status != RootStatus::Ready)
        .collect();
    rows.sort_by_key(|&row| (row.status.as_str(), &row.slug));
    for row in rows {
        let _ = writeln!(out, "root {}: {}", row.slug, row.status.as_str());
        if let Some(error) = &row.error {
            let _ = writeln!(out, "  {error}");
        }
    }
    // Grouped by kind, because the kinds answer different questions and an operator
    // scans for the group, not the row. Passing checks are summarized in one line per
    // kind; findings print in full, because a finding is the reason to run this.
    for kind in CheckKind::ALL {
        let (findings, ok): (Vec<&Check>, Vec<_>) = d
            .checks
            .iter()
            .filter(|row| row.check == kind)
            .partition(|row| row.is_finding());
        match (findings.len(), findings.len() + ok.len()) {
            (_, 0) => {}
            (0, n) => {
                let _ = writeln!(out, "check {}: {n} ok", label(kind));
            }
            (f, n) => {
                let _ = writeln!(out, "check {}: {f} of {n} need attention", label(kind));
                for row in findings {
                    let at = [row.slug.as_deref(), row.name.as_deref()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let at = suffix(" ", Some(at.as_str()).filter(|at| !at.is_empty()));
                    let why = suffix(" — ", row.detail.as_deref());
                    let _ = writeln!(out, "  {:?}{at}{why}", row.status);
                }
            }
        }
    }
    out
}

/// An optional tail: `sep` and the value when there is one, nothing when there is not.
fn suffix(sep: &str, value: Option<&str>) -> String {
    value.map_or_else(String::new, |v| format!("{sep}{v}"))
}

/// A check kind's operator-facing name. Total over [`CheckKind`], so a new kind must
/// be named here before it can ship.
const fn label(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Manifest => "manifest",
        CheckKind::InstallUnderHome => "install-under-home",
        CheckKind::LegacyLayout => "legacy-layout",
        CheckKind::OrphanCode => "orphan-code",
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
/// The plumbing checks [`doctor`] renders are **report-only** and deliberately
/// exit-neutral, for the reason `grove_ops::doctor` states: a
/// `missing`/`undeclared`/`mismatch` finding is either drift the reconciler already
/// owns or a human's edit to reconcile with, and a `grove doctor` that exits non-zero
/// on ordinary transient drift is a gate nobody can wire into anything.
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
    use super::*;
    use grove_api::RootStatusEntry;
    use grove_ops::doctor::CheckStatus;
    use grove_ops::env::ShareStatus;

    fn share(status: ShareStatus) -> ShareOutcome {
        ShareOutcome {
            slug: "o/r".into(),
            worktree: Some("feat".into()),
            path: ".env".into(),
            status,
            reason: None,
        }
    }

    fn code(rows: &[ShareOutcome]) -> u8 {
        fail_on_unresolved(rows).unwrap_err().exit_code()
    }

    /// v1's `doctor_exit_code_is_conflict_on_unresolved`, carried whole — and the
    /// conflict count is the message, so a CI log says how much work is left.
    #[test]
    fn doctor_exit_code_is_conflict_on_unresolved() {
        use ShareStatus::{Conflict, Error, Linked, Ok};
        assert!(fail_on_unresolved(&[share(Ok), share(Linked)]).is_ok());
        assert_eq!(code(&[share(Conflict)]), 5);
        assert_eq!(code(&[share(Error)]), 1);
        // An error outranks a conflict (exit 1 wins).
        assert_eq!(code(&[share(Conflict), share(Error)]), 1);
        let err = fail_on_unresolved(&[share(Conflict), share(Conflict)]).unwrap_err();
        assert_eq!(err.to_string(), "2 unresolved conflict(s)");
    }

    /// The v1 defect this closes: the verdict read `a.status == "failed"`, so a
    /// renamed wire spelling would have made a failed apply exit 0. Typed now.
    #[test]
    fn a_failed_root_is_exit_1_and_the_others_are_not() {
        use ReconcileStatus::{Cloned, Failed, Present};
        let row = |status| Applied {
            slug: "o/r".into(),
            status,
            default_branch: Some("main".into()),
            error: None,
        };
        assert!(applied(&[row(Cloned), row(Present)]).is_ok());
        let err = applied(&[row(Present), row(Failed)]).unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    /// Every fast-forward outcome in words — the two that leave the trunk untouched say
    /// "nothing forced", since carried law 9 makes that the answer — and the prune
    /// line silent at zero.
    #[test]
    fn the_sync_report_reads_exactly_so() {
        use FastForward::{AlreadyCurrent, Dirty, Diverged, Updated};
        let sync = |fetched, trunk, stale_slots_pruned| {
            sync_report(
                "o/r",
                &SyncReport {
                    fetched,
                    trunk,
                    tip: "abc".into(),
                    stale_slots_pruned,
                },
            )
        };
        assert_eq!(
            [
                sync(true, Updated, 0),
                sync(false, AlreadyCurrent, 1),
                sync(true, Diverged, 3),
                sync(true, Dirty, 0),
            ]
            .concat(),
            "synced o/r: fetched, trunk updated
  tip abc
synced o/r: not fetched, trunk already current
  tip abc
  1 stale pool slot recycled
synced o/r: fetched, trunk diverged — local commits kept, nothing forced
  tip abc
  3 stale pool slots recycled
synced o/r: fetched, trunk dirty — uncommitted tracked files kept, nothing forced
  tip abc
"
        );
    }

    /// Every arm of every section, and all nine check labels, byte for byte. The
    /// orphan-code finding's `Some("")` slug is the manifest audit's for a `[roots.""]`
    /// key, and must not leave a trailing space.
    #[test]
    fn the_doctor_report_reads_exactly_so() {
        use CheckKind::{Drift, Manifest, OrphanCode, Worktree};
        use CheckStatus::{Invalid, Mismatch, Ok, Undeclared};
        assert_eq!(doctor(&DoctorData::default()), "no shares declared\n");
        let check =
            |check, status, slug: Option<&str>, name: Option<&str>, detail: Option<&str>| Check {
                check,
                status,
                slug: slug.map(Into::into),
                name: name.map(Into::into),
                detail: detail.map(Into::into),
            };
        let mut checks = vec![
            check(Manifest, Invalid, None, None, None),
            check(OrphanCode, Mismatch, Some(""), None, None),
            check(Worktree, Mismatch, Some("o/r"), Some("feat"), Some("x")),
            check(Drift, Undeclared, Some("o/r"), None, None),
        ];
        checks.extend(CheckKind::ALL.map(|k| check(k, Ok, None, None, None)));
        let root = |slug: &str, status, error: Option<&str>| RootStatusEntry {
            slug: slug.into(),
            status,
            error: error.map(Into::into),
        };
        let pool = |slug: &str, observed, target| PoolStatus {
            slug: slug.into(),
            observed,
            target,
        };
        let data = DoctorData {
            report: vec![
                share(ShareStatus::Linked),
                ShareOutcome {
                    worktree: None,
                    path: "cfg".into(),
                    reason: Some("differs".into()),
                    ..share(ShareStatus::Conflict)
                },
            ],
            pools: vec![pool("o/r", 2, 2), pool("x/y", 0, 3)],
            statuses: vec![
                root("a/a", RootStatus::Ready, None),
                root("z/z", RootStatus::Degraded, Some("occupied")),
                root("c/c", RootStatus::Missing, None),
                root("b/b", RootStatus::Degraded, None),
            ],
            checks,
        };
        assert_eq!(
            doctor(&data),
            "o/r .env @feat: Linked
o/r cfg: Conflict — differs
pool o/r: 2/2
pool x/y: 0/3 (under-filled)
root b/b: degraded
root z/z: degraded
  occupied
root c/c: missing
check manifest: 1 of 2 need attention
  Invalid
check install-under-home: 1 ok
check legacy-layout: 1 ok
check orphan-code: 1 of 2 need attention
  Mismatch
check root: 1 ok
check bare: 1 ok
check trunk: 1 ok
check worktree: 1 of 2 need attention
  Mismatch o/r feat — x
check drift: 1 of 2 need attention
  Undeclared o/r
"
        );
    }
}
