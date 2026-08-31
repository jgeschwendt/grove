//! Request and response bodies, one pair per route.
//!
//! Every type here is the `data` half of an [`crate::Envelope`] — the envelope itself
//! is never written into these shapes. Slugs travel in the **body**, not the path:
//! they contain `/`.
//!
//! | Route | Request | `data` |
//! |---|---|---|
//! | `GET /api/health` | — | [`HealthData`] |
//! | `GET /api/daemon/version` | — | [`VersionData`] |
//! | `POST /api/daemon/shutdown` | — | [`ShutdownData`] |
//! | `POST /api/roots/reconcile` | — | [`ReconcileData`] |
//! | `POST /api/roots/sync` | [`SyncRequest`] | [`SyncData`] |
//! | `POST /api/roots/remove` | [`RemoveRootRequest`] | [`RemoveRootData`] |
//! | `POST /api/worktrees/remove` | [`RemoveWorktreeRequest`] | [`RemoveWorktreeData`] |
//! | `POST /api/doctor` | [`DoctorRequest`] | [`DoctorData`] |
//! | `GET /api/roots` | — | [`Snapshot`] |
//! | `GET /api/events` | — | a stream of [`crate::Event`] |

use grove_ops::doctor::Check;
use grove_ops::env::ShareOutcome;
use grove_ops::git::Status;
use grove_ops::pool::PoolStatus;
use serde::{Deserialize, Serialize};

use crate::RootStatus;
use crate::events::LogLine;
use crate::status::SyncNote;
use crate::vocab::wire_enum;

/// `GET /api/health` — identity, readiness, the running version, and **which home
/// this daemon realizes**.
///
/// The version is what the self-update health gate compares against the version it
/// just flipped to; a 200 alone would let a stale process still bound to the port
/// mask a failed update.
///
/// The home is the identity half, and it is on the wire because the CLI resolves
/// `GROVE_HOME` and `GROVE_BIND` as two unrelated facts: without it, a client pointed
/// at one home delegates `roots/remove` to whatever answers the bind, and the daemon
/// deletes from *its* home — a mismatch nothing in the protocol could detect. A
/// client compares it against the home it was pointed at (see `grove`'s
/// `ApiClient::reachable`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HealthData {
    pub status: HealthStatus,
    pub version: String,
    pub home: String,
}

/// The one status a *successful* health response can report. Every other boot state
/// answers 503 with an error envelope carrying a [`crate::BootStatus`] in
/// `error.data`, so "health said 200 but wasn't ready" is unrepresentable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Ready,
}

/// `GET /api/daemon/version` — the version plus how long this process has served.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VersionData {
    pub version: String,
    pub uptime_ms: u64,
}

/// `POST /api/daemon/shutdown` — the drain acknowledgement. Whitelisted past the
/// readiness gate, so it answers while already draining.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ShutdownData {
    pub stopping: bool,
}

impl ShutdownData {
    /// The only body this route emits: `{"stopping": true}`.
    pub const STOPPING: Self = Self { stopping: true };
}

/// `POST /api/roots/reconcile` — a fire-and-forget nudge; the acknowledgement says
/// the work was *scheduled*, never that it finished.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReconcileData {
    pub reconcile: ReconcileAck,
}

impl ReconcileData {
    pub const SCHEDULED: Self = Self {
        reconcile: ReconcileAck::Scheduled,
    };
}

wire_enum! {
    /// The nudge acknowledgement's one value. An enum rather than a `String` for the
    /// same reason as [`HealthStatus`]: the route has exactly one thing to say — and
    /// through [`wire_enum`], so that one thing is pinned in
    /// `contracts/wire-vocab.json` rather than living only in a Rust literal.
    pub enum ReconcileAck {
        /// Convergence was queued. Never "converged".
        Scheduled => "scheduled",
    }
}

/// `POST /api/roots/sync {slug}` — fetch one root's default branch and fast-forward
/// its `.trunk`.
///
/// A body-carrying POST for the reason every other slug-naming route is one: slugs
/// contain `/`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SyncRequest {
    pub slug: String,
}

/// `POST /api/roots/sync` — the sync acknowledgement.
///
/// The mirror of [`ReconcileData`], and for the same reason: the engine's sync is
/// **accept-only**, so the route can only ever report that the request was recorded.
/// What the sync *did* is observed elsewhere — the `root_sync_changed` event on
/// `GET /api/events`, and the `syncing`/`sync_note` fields of every [`Snapshot`]
/// afterwards. A route that waited for the fetch would be a second realizer's worth
/// of latency in front of a level-triggered fact already published twice.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SyncData {
    pub sync: SyncAck,
}

impl SyncData {
    /// The only body this route emits: `{"sync": "accepted"}`.
    pub const ACCEPTED: Self = Self {
        sync: SyncAck::Accepted,
    };
}

wire_enum! {
    /// The sync acknowledgement's one value.
    ///
    /// Deliberately **not** `scheduled`: a reconcile nudge is level-triggered over
    /// the whole home and says nothing about any one root, while this names a root
    /// whose engine has recorded a pending sync. Spelling them apart keeps a consumer
    /// from reading one route's ack as the other's guarantee.
    pub enum SyncAck {
        /// The engine recorded the request. Never "synced".
        Accepted => "accepted",
    }
}

/// `POST /api/roots/remove` — undeclare a root and delete it from disk.
///
/// `force` defaults to false, which is the guarded remove: the root is surveyed for
/// uncommitted tracked changes and unpushed commits across every worktree under it,
/// and a find is a `conflict` rather than a deletion.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RemoveRootRequest {
    pub slug: String,
    #[serde(default)]
    pub force: bool,
}

/// The removed slug, echoed back.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RemoveRootData {
    pub removed: String,
}

/// `POST /api/worktrees/remove` — remove one worktree of one root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RemoveWorktreeRequest {
    pub slug: String,
    pub name: String,
}

/// The removed worktree *name* (not the slug), echoed back.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RemoveWorktreeData {
    pub removed: String,
}

/// `POST /api/doctor` — converge (or merely diagnose) the worktree environment.
///
/// Every field defaults: v1's controller coerced with a strict `== true`, so a body
/// omitting `dry_run`/`fix` read them as false and a missing `slug` meant "every
/// root". `#[serde(default)]` reproduces that exactly, and an empty body `{}` is a
/// valid whole-home fix-nothing run.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DoctorRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub fix: bool,
}

/// The doctor payload: the share report, per-root pool levels, per-root engine
/// statuses, and the git-plumbing checks.
///
/// Every field but `report` defaults to empty because an older or offline producer
/// may omit it; `report` does not — a doctor answer with no report is not an answer.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DoctorData {
    pub report: Vec<ShareOutcome>,
    #[serde(default)]
    pub pools: Vec<PoolStatus>,
    #[serde(default)]
    pub statuses: Vec<RootStatusEntry>,
    /// The plumbing pass: manifest validity, bare/`.trunk` presence, declared
    /// worktrees on their declared branches, and undeclared drift. **Report-only** —
    /// `fix` still converges shares and nothing else.
    #[serde(default)]
    pub checks: Vec<Check>,
}

/// One root's engine status inside [`DoctorData::statuses`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RootStatusEntry {
    pub slug: String,
    pub status: RootStatus,
}

/// The whole observable world: `GET /api/roots`' body, and the first frame of
/// `GET /api/events` ([`crate::Event::Snapshot`]).
///
/// One type for both on purpose. A UI attaching to the stream must render before its
/// first event arrives, and a UI polling the route must render the same thing — two
/// shapes would be two decoders and two chances to disagree about what a root is.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Snapshot {
    pub roots: Vec<RootView>,
    /// The daemon's log ring, oldest first. Bounded (500 lines) and cheap to carry;
    /// a viewer that opened after the daemon started still sees what it missed.
    #[serde(default)]
    pub logs: Vec<LogLine>,
}

/// One root, as a dashboard draws it — the data v1's `LiveView` island assembled from
/// one `worktree.list` call plus two in-memory engine reads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RootView {
    pub slug: String,
    pub url: String,
    /// The engine's cached status. `unavailable` when this root's reads could not be
    /// completed — the row is then structurally present but its git-derived fields
    /// are empty, and saying "ready" beside an empty worktree list would be a lie a
    /// UI cannot detect.
    pub status: RootStatus,
    pub pool: PoolView,
    /// A sync is pending or in flight.
    pub syncing: bool,
    /// What the last sync left behind, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_note: Option<SyncNote>,
    /// The absolute path of `.trunk` — what a UI opens a terminal or an editor at.
    pub trunk: String,
    /// `.trunk`'s own git drift, or `None` when it is not on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trunk_status: Option<Status>,
    pub worktrees: Vec<WorktreeView>,
}

/// A root's warm-slot pool: what is on disk against what is declared.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PoolView {
    pub observed: usize,
    pub target: u32,
}

/// One worktree of one root: the manifest's view and git's, side by side.
///
/// `branch` is what the manifest **declares**; `status.branch` is what is actually
/// checked out. The two disagreeing is the drift a dashboard flags and doctor's
/// `worktree` check reports as a `mismatch` — which is why both travel rather than
/// one resolved answer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorktreeView {
    pub name: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Recorded in the manifest.
    pub declared: bool,
    /// Realized in git.
    pub present: bool,
    /// The absolute path of the checkout.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

#[cfg(test)]
mod tests {
    use super::{
        DoctorData, DoctorRequest, PoolView, ReconcileData, RemoveRootRequest, RootView,
        ShutdownData, Snapshot, SyncData, SyncRequest, VersionData, WorktreeView,
    };
    use crate::RootStatus;
    use crate::status::SyncNote;
    use grove_ops::doctor::{Check, CheckKind, CheckStatus};
    use serde_json::json;

    /// The snapshot's wire shape, byte for byte — this is what the UI renders from,
    /// and the one place its field names are pinned. Note the two branches side by
    /// side: `branch` is what the manifest declares, `status.branch` what is checked
    /// out, and the drift between them is a thing the UI draws.
    #[test]
    fn a_snapshot_serializes_the_shape_a_dashboard_renders() {
        let snapshot = Snapshot {
            roots: vec![RootView {
                slug: "o/r".into(),
                url: "git@github.com:o/r.git".into(),
                status: RootStatus::Ready,
                pool: PoolView {
                    observed: 1,
                    target: 2,
                },
                syncing: true,
                sync_note: Some(SyncNote::Diverged),
                trunk: "/home/code/o/r/.trunk".into(),
                trunk_status: None,
                worktrees: vec![WorktreeView {
                    name: "feat".into(),
                    branch: "feature/x".into(),
                    base: None,
                    declared: true,
                    present: true,
                    path: "/home/code/o/r/feat".into(),
                    status: None,
                }],
            }],
            logs: Vec::new(),
        };

        assert_eq!(
            serde_json::to_value(&snapshot).unwrap(),
            json!({
                "roots": [{
                    "slug": "o/r",
                    "url": "git@github.com:o/r.git",
                    "status": "ready",
                    "pool": {"observed": 1, "target": 2},
                    "syncing": true,
                    "sync_note": "diverged",
                    "trunk": "/home/code/o/r/.trunk",
                    "worktrees": [{
                        "name": "feat",
                        "branch": "feature/x",
                        "declared": true,
                        "present": true,
                        "path": "/home/code/o/r/feat"
                    }]
                }],
                "logs": []
            }),
            "absent optionals are omitted, not rendered as null"
        );
        assert_eq!(
            serde_json::from_value::<Snapshot>(serde_json::to_value(&snapshot).unwrap()).unwrap(),
            snapshot
        );
    }

    /// Doctor's new array, in the shape a CLI renderer will group on: one row per
    /// finding, each naming what was checked, how it came out, and where.
    #[test]
    fn a_doctor_check_serializes_as_a_grouped_finding() {
        let data = DoctorData {
            checks: vec![
                Check {
                    check: CheckKind::Manifest,
                    status: CheckStatus::Ok,
                    slug: None,
                    name: None,
                    detail: None,
                },
                Check {
                    check: CheckKind::Worktree,
                    status: CheckStatus::Mismatch,
                    slug: Some("o/r".into()),
                    name: Some("feat".into()),
                    detail: Some("checked out on `main`, declared `feature/x`".into()),
                },
            ],
            ..DoctorData::default()
        };

        assert_eq!(
            serde_json::to_value(&data).unwrap()["checks"],
            json!([
                {"check": "manifest", "status": "ok"},
                {
                    "check": "worktree", "status": "mismatch", "slug": "o/r",
                    "name": "feat", "detail": "checked out on `main`, declared `feature/x`"
                }
            ])
        );
        assert!(!data.checks[0].is_finding() && data.checks[1].is_finding());
    }

    /// The three single-value bodies, byte for byte.
    #[test]
    fn constant_bodies_match_the_contract() {
        assert_eq!(
            serde_json::to_value(ShutdownData::STOPPING).unwrap(),
            json!({"stopping": true})
        );
        assert_eq!(
            serde_json::to_value(ReconcileData::SCHEDULED).unwrap(),
            json!({"reconcile": "scheduled"})
        );
        assert_eq!(
            serde_json::to_value(SyncData::ACCEPTED).unwrap(),
            json!({"sync": "accepted"})
        );
        assert_eq!(
            serde_json::to_value(VersionData {
                version: "0.1.0".into(),
                uptime_ms: 1234
            })
            .unwrap(),
            json!({"version": "0.1.0", "uptime_ms": 1234})
        );
    }

    /// Carried from the Elixir controller's strict `== true` coercion: an empty body
    /// is a whole-home, non-dry, non-fixing run.
    #[test]
    fn an_empty_doctor_body_is_a_whole_home_run() {
        let req: DoctorRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req, DoctorRequest::default());
        assert!(req.slug.is_none());
        assert!(!req.dry_run && !req.fix);
        // A `null` slug is the same as an absent one — the CLI sends it explicitly.
        let explicit: DoctorRequest =
            serde_json::from_str(r#"{"slug":null,"dry_run":true,"fix":false}"#).unwrap();
        assert!(explicit.slug.is_none() && explicit.dry_run);
    }

    /// Slugs contain `/`, which is why every one of these routes is a body-carrying
    /// POST rather than a path parameter.
    #[test]
    fn a_slug_travels_in_the_body() {
        let req: RemoveRootRequest = serde_json::from_str(r#"{"slug":"o/r"}"#).unwrap();
        assert_eq!(req.slug, "o/r");
        let req: SyncRequest = serde_json::from_str(r#"{"slug":"o/r"}"#).unwrap();
        assert_eq!(req.slug, "o/r");
    }

    /// The two accept-only acks are spelled apart on purpose — `scheduled` is the
    /// whole-home nudge, `accepted` is one root's recorded sync — and neither decodes
    /// as the other. A route that started answering the wrong one would be a client
    /// silently reading a home-wide promise as a per-root one.
    #[test]
    fn the_two_acks_do_not_decode_as_each_other() {
        assert_eq!(
            serde_json::from_str::<SyncData>(r#"{"sync":"accepted"}"#).unwrap(),
            SyncData::ACCEPTED
        );
        assert!(serde_json::from_str::<SyncData>(r#"{"sync":"scheduled"}"#).is_err());
        assert!(serde_json::from_str::<ReconcileData>(r#"{"reconcile":"accepted"}"#).is_err());
    }
}
