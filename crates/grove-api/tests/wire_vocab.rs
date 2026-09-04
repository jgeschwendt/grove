//! Snapshot the wire vocabulary to `contracts/wire-vocab.json`.
//!
//! Two kinds of group live in the fixture, and both are derived from the producers
//! rather than written by hand:
//!
//! - **vocabularies** — the closed sets of strings a value can be (`root_status`,
//!   `http_error_codes`, the SSE `events` names, …);
//! - **key sets** — the field names of the payload shapes the two read surfaces
//!   carry (`health_keys`, `snapshot_keys`, `root_view_keys`, `pool_view_keys`,
//!   `worktree_view_keys`, `log_line_keys`, and `event_keys` for the union of every
//!   `GET /api/events` frame). Built from a *fully-populated* value, so the optionals
//!   that `skip_serializing_if` would hide are pinned too.
//!
//!   **Each key set is shallow — one object, one level.** [`keys`] reads only the
//!   top-level field names, so a nested shape needs its own group or it is pinned by
//!   nothing: `log_line_keys` pins the string `"fields"` and not the `{name, value}`
//!   inside it, and neither read surface's `git::Status` appears in any parent's key
//!   list. `git_status_keys` and `log_field_keys` are those groups; a nested shape
//!   added later needs one too.
//!
//! The key sets are here because the fixture is the one artifact an external consumer
//! *can* read — the only rendering of these shapes outside the crate that owns them: the
//! literal-JSON round-trips inside grove-api pin the same shapes precisely, but they
//! are invisible from outside the crate, and "the shapes are pinned somewhere you
//! cannot see" is the drift class this file exists to close.
//!
//! This is the guard that turns a silent cross-language rename into a failing test.
//! The expected value is built here from the *real* producers — the `grove_ops::wire`
//! enums, the serde spelling of `git::FastForward` / `pool::ColdReason` /
//! `env::ShareStatus`, `Error::code()`, the `WorktreeStatus` field names, and
//! grove-api's own `ErrorCode` / `RootStatus` — so any rename in either crate changes
//! this output and fails the assertion. The checked-in fixture is what an external UI
//! *would* read its unions from — none does today, so this test is the whole guard and
//! it guards only this side; re-bless a deliberate change with
//! `BLESS_WIRE=1 cargo test -p grove-api`.
//!
//! It lives in grove-api rather than grove-ops because the vocabulary now spans both
//! crates and the dependency runs one way: grove-api can see every producer, grove-ops
//! cannot see the HTTP ones. One fixture wants one guard, so the guard sits at the
//! end that can reach everything.

use grove_api::events::{LogField, LogLine};
use grove_api::policy::{RECONCILE_DEGRADED, RECONCILE_READY};
use grove_api::routes::{
    HealthData, HealthStatus, PoolView, ReconcileAck, RootView, Snapshot, SyncAck, WorktreeView,
};
use grove_api::{ErrorCode, Event, LogLevel, RootStatus, SyncNote, TaskKind, TaskOutcome};
use grove_ops::Error;
use grove_ops::doctor::{CheckKind, CheckStatus};
use grove_ops::env::ShareStatus;
use grove_ops::git::FastForward;
use grove_ops::pool::ColdReason;
use grove_ops::wire::{AdoptStatus, ReconcileStatus, WorktreeOutcomeStatus, promotion};
use grove_ops::worktrees::WorktreeStatus;
use serde::Serialize;
use serde_json::{Value, json};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../contracts/wire-vocab.json"
);

/// The serde wire spelling of an enum value (its `rename_all` form).
fn s<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

/// The JSON object field names a `WorktreeStatus` serializes to, sorted. `base` and
/// `status` are `skip_serializing_if = None`, so build the fully-populated variant to
/// surface them.
fn worktree_keys() -> Vec<String> {
    let wt = WorktreeStatus {
        name: "feat".into(),
        branch: "feat".into(),
        base: Some("main".into()),
        present: true,
        declared: true,
        status: Some(git_status()),
    };
    let mut keys: Vec<String> = serde_json::to_value(&wt)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// The sorted JSON field names a fully-populated value serializes to.
///
/// "Fully-populated" is load-bearing: every optional on these types is
/// `skip_serializing_if = None`, so a default value would pin a *subset* of the shape
/// and a field could be renamed without the fixture noticing.
fn keys<T: Serialize>(v: &T) -> Vec<String> {
    let mut keys: Vec<String> = serde_json::to_value(v)
        .unwrap()
        .as_object()
        .expect("a struct payload serializes to an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// A `git::Status` with nothing omitted. It is nested on both read surfaces and in
/// no parent's key list, so it is pinned here or nowhere: every snapshot round-trip
/// in the tree builds it as `None`, which is exactly how nine field names crossed the
/// wire guarded by nothing.
fn git_status() -> grove_ops::git::Status {
    grove_ops::git::Status {
        ahead: 2,
        behind: 1,
        branch: Some("feature/x".into()),
        conflicted: 1,
        head: Some("0123456789abcdef0123456789abcdef01234567".into()),
        staged: 3,
        unstaged: 4,
        untracked: 5,
        upstream: Some("origin/feature/x".into()),
    }
}

/// One `RootView` with nothing omitted — the row `GET /api/roots` repeats per root,
/// and the row `Event::Snapshot`/`Resync` carry.
fn root_view() -> RootView {
    RootView {
        slug: "o/r".into(),
        url: "https://example.invalid/o/r.git".into(),
        status: RootStatus::Ready,
        pool: PoolView {
            observed: 1,
            target: 2,
        },
        syncing: true,
        sync_note: Some(SyncNote::Diverged),
        trunk: "/home/code/o/r/main".into(),
        trunk_branch: "main".into(),
        trunk_status: Some(git_status()),
        worktrees: vec![WorktreeView {
            name: "feat".into(),
            branch: "feature/x".into(),
            base: Some("main".into()),
            declared: true,
            present: true,
            path: "/home/code/o/r/feat".into(),
            status: Some(git_status()),
        }],
    }
}

fn log_line() -> LogLine {
    LogLine {
        at_ms: 1_577_836_800_000,
        level: LogLevel::Info,
        target: "grove_daemon::engine".into(),
        message: "engine reconciled".into(),
        fields: vec![LogField {
            name: "slug".into(),
            value: "o/r".into(),
        }],
    }
}

/// Every field name any `GET /api/events` frame can carry, sorted and deduplicated —
/// the tag itself plus the union of the seven payloads. A union rather than a group
/// per event because the fixture is a flat map of string lists, and it still does the
/// job a consumer needs: no field can be added, renamed or dropped on any event
/// without this changing.
fn event_keys() -> Vec<String> {
    let snapshot = Snapshot {
        roots: vec![root_view()],
        logs: vec![log_line()],
    };
    let every = [
        Event::RootsChanged {
            roots: vec!["o/r".into()],
        },
        Event::RootSyncChanged { slug: "o/r".into() },
        Event::TaskStarted {
            slug: "o/r".into(),
            kind: TaskKind::Reconcile,
        },
        Event::TaskFinished {
            slug: "o/r".into(),
            kind: TaskKind::Fill,
            outcome: TaskOutcome::Failed,
        },
        Event::Snapshot(snapshot.clone()),
        Event::Resync(snapshot),
        Event::Log(log_line()),
    ];
    let mut keys: Vec<String> = every.iter().flat_map(keys).collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Every `Error` category's wire `code()`, in declaration order.
fn error_codes() -> Vec<&'static str> {
    let m = String::new;
    [
        Error::NotDeclared(m()),
        Error::NotReady(m()),
        Error::InvalidInput(m()),
        Error::Conflict(m()),
        Error::Network(m()),
        Error::Git(m()),
        Error::Io(m()),
    ]
    .iter()
    .map(Error::code)
    .collect()
}

fn vocab() -> Value {
    json!({
        "adopt_status": [s(&AdoptStatus::Adopted), s(&AdoptStatus::Skipped)],
        "check_kind": CheckKind::ALL.map(|k| s(&k)),
        "check_status": CheckStatus::ALL.map(|c| s(&c)),
        "cold_reason": [s(&ColdReason::Empty), s(&ColdReason::Conflict)],
        "error_codes": error_codes(),
        "event_keys": event_keys(),
        "events": Event::NAMES,
        // Nested inside `RootView.trunk_status`, `WorktreeView.status` and
        // `WorktreeStatus.status` — every one of which pins only the string
        // `"trunk_status"`/`"status"`, never what is under it.
        "git_status_keys": keys(&git_status()),
        "health_keys": keys(&HealthData {
            status: HealthStatus::Ready,
            version: "0.1.0".into(),
            home: "/home/.grove".into(),
        }),
        // Nested inside `LogLine.fields[]`, for the same reason.
        "log_field_keys": keys(&LogField {
            name: "slug".into(),
            value: "o/r".into(),
        }),
        "log_line_keys": keys(&log_line()),
        "pool_view_keys": keys(&PoolView { observed: 1, target: 2 }),
        "root_view_keys": keys(&root_view()),
        "fast_forward": [
            s(&FastForward::Updated),
            s(&FastForward::AlreadyCurrent),
            s(&FastForward::Diverged),
            s(&FastForward::Dirty)
        ],
        "http_error_codes": ErrorCode::ALL.map(|c| s(&c)),
        "log_level": LogLevel::ALL.map(|l| s(&l)),
        "promotion": [promotion::PROMOTED, promotion::COLD],
        // The two accept-only acknowledgements. Single-valued vocabularies, pinned
        // for the reason every other one is: a client reads `POST /api/roots/sync`'s
        // answer to learn its request was recorded, and a route that started
        // answering the other spelling would tell it something different in silence.
        "reconcile_ack": ReconcileAck::ALL.map(|a| s(&a)),
        "reconcile_status": [
            s(&ReconcileStatus::Cloned),
            s(&ReconcileStatus::Present),
            s(&ReconcileStatus::Failed)
        ],
        "root_status": RootStatus::ALL.map(|r| s(&r)),
        "sync_note": SyncNote::ALL.map(|n| s(&n)),
        "task_kind": TaskKind::ALL.map(|k| s(&k)),
        "task_outcome": TaskOutcome::ALL.map(|o| s(&o)),
        "snapshot_keys": keys(&Snapshot {
            roots: vec![root_view()],
            logs: vec![log_line()],
        }),
        "sync_ack": SyncAck::ALL.map(|a| s(&a)),
        "share_status": [
            s(&ShareStatus::Ok),
            s(&ShareStatus::Created),
            s(&ShareStatus::Linked),
            s(&ShareStatus::Copied),
            s(&ShareStatus::Repointed),
            s(&ShareStatus::Conflict),
            s(&ShareStatus::Gc),
            s(&ShareStatus::Error)
        ],
        "worktree_keys": worktree_keys(),
        "worktree_view_keys": keys(&root_view().worktrees[0]),
        "worktree_outcome": [
            s(&WorktreeOutcomeStatus::Recreated),
            s(&WorktreeOutcomeStatus::Adopted),
            s(&WorktreeOutcomeStatus::Failed)
        ],
    })
}

/// Render the fixture the way it is checked in: one group per line, members inline.
/// `to_string_pretty` puts every member on its own line, which turns a one-word
/// rename into a whole-file diff — and, because the assertion below compares parsed
/// values rather than bytes, would let the committed formatting drift from what a
/// re-bless emits without any test noticing.
fn render(vocab: &Value) -> String {
    let groups: Vec<String> = vocab
        .as_object()
        .unwrap()
        .iter()
        .map(|(group, members)| {
            let members: Vec<String> = members
                .as_array()
                .unwrap()
                .iter()
                .map(ToString::to_string)
                .collect();
            format!(
                "  {}: [{}]",
                Value::String(group.clone()),
                members.join(", ")
            )
        })
        .collect();
    format!("{{\n{}\n}}\n", groups.join(",\n"))
}

/// The fixture as it is checked in.
fn fixture() -> Value {
    let text = std::fs::read_to_string(FIXTURE).unwrap_or_else(|e| {
        panic!("missing {FIXTURE}: {e}. Re-bless with `BLESS_WIRE=1 cargo test -p grove-api`.")
    });
    serde_json::from_str(&text).expect("fixture is valid JSON")
}

#[test]
fn wire_vocab_matches_fixture() {
    let expected = vocab();

    if std::env::var_os("BLESS_WIRE").is_some() {
        std::fs::write(FIXTURE, render(&expected)).expect("write blessed fixture");
        return;
    }

    let on_disk = fixture();

    assert_eq!(
        on_disk, expected,
        "wire vocabulary drifted from contracts/wire-vocab.json. A rename would reach \
         the fixture's consumers silently — re-bless with `BLESS_WIRE=1 cargo test -p \
         grove-api`, then update the consumers the fixture feeds."
    );
}

/// The runtime half of the reconcile-partition guard (`wire_test.exs:24`): the
/// engine's two policy lists cover the *published* vocabulary exactly, so a status a
/// consumer can see is never one the engine has no opinion about. The compile-time
/// half lives in `policy::classify_reconcile`, whose match has no catch-all.
#[test]
fn the_reconcile_partition_covers_the_published_vocabulary() {
    let mut classified: Vec<String> = RECONCILE_READY
        .iter()
        .chain(RECONCILE_DEGRADED.iter())
        .map(s)
        .collect();
    classified.sort();

    let mut published: Vec<String> = vocab()["reconcile_status"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    published.sort();

    assert_eq!(
        classified, published,
        "reconcile_ready ++ reconcile_degraded must be exactly the reconcile_status \
         vocabulary — a member of neither reaches the engine unclassified"
    );
}

/// The two groups grove-api owns, checked against the file rather than against the
/// value [`vocab`] just built from the same `ALL` — that comparison is `ALL == ALL`
/// and can never fail. Redundant with [`wire_vocab_matches_fixture`] by construction,
/// and kept for its message: this one names the member the checked-in fixture is
/// missing, which is the failure a new variant produces.
#[test]
fn the_checked_in_fixture_lists_every_http_code_and_root_status() {
    // A bless run rewrites the fixture from the sibling test's process; reading it
    // here mid-flight would compare against whichever version won the race.
    if std::env::var_os("BLESS_WIRE").is_some() {
        return;
    }
    let fixture = fixture();
    let group = |name: &str| -> Vec<String> {
        fixture[name]
            .as_array()
            .unwrap_or_else(|| panic!("the fixture has a `{name}` group"))
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };

    let codes = group("http_error_codes");
    assert_eq!(codes.len(), ErrorCode::ALL.len());
    for code in ErrorCode::ALL {
        assert!(
            codes.iter().any(|c| c == code.as_str()),
            "{code} missing from the checked-in fixture — re-bless it"
        );
    }

    let statuses = group("root_status");
    assert_eq!(statuses.len(), RootStatus::ALL.len());
    for status in RootStatus::ALL {
        assert!(
            statuses.iter().any(|s| s == status.as_str()),
            "{status} missing from the checked-in fixture — re-bless it"
        );
    }

    // The event names carry the same risk one step further: `Event`'s variants hold
    // payloads, so they cannot be declared through `wire_enum!` and `NAMES` is
    // hand-written. The crate's own test pins `NAMES` against the variants; this
    // pins the checked-in fixture against `NAMES`.
    let events = group("events");
    assert_eq!(events.len(), Event::NAMES.len());
    for name in Event::NAMES {
        assert!(
            events.iter().any(|e| e == name),
            "{name} missing from the checked-in fixture — re-bless it"
        );
    }
}
