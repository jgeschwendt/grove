//! The event vocabulary the daemon pushes — what v1's dashboard `PubSub` topic
//! carried, given a wire shape.
//!
//! v1 broadcast three Erlang terms on a `"roots"` topic (`{:roots_changed, roots}`,
//! `{:root_sync_changed, slug}`, plus per-op telemetry that never left the BEAM).
//! Only a view in the same process could read them, so they were never
//! contract. Here they are one externally-tagged enum: the `event` field is the
//! discriminator, the rest of the object is the payload, and the whole set is
//! snapshotted into `contracts/wire-vocab.json` beside every other vocabulary.
//!
//! ```jsonc
//! {"event": "roots_changed",     "roots": ["o/r"]}
//! {"event": "root_sync_changed", "slug": "o/r"}
//! {"event": "task_started",      "slug": "o/r", "kind": "reconcile"}
//! {"event": "task_finished",     "slug": "o/r", "kind": "reconcile", "outcome": "ok"}
//! {"event": "snapshot",          "roots": [...], "logs": [...]}
//! {"event": "resync",            "roots": [...], "logs": [...]}
//! {"event": "log",               "level": "info", "target": "…", "message": "…", …}
//! ```
//!
//! **Level-triggered, not a log** — the first four, at least. Each says *something
//! changed, re-read it* and none carries state a consumer should accumulate, so a
//! dropped one costs a stale view until the next, never a divergent one. That is what
//! lets the bus be a bounded broadcast channel that drops for a slow subscriber
//! rather than growing without bound.
//!
//! ## Three producers, one vocabulary
//!
//! The last three are not published on that bus, and the types say where each comes
//! from:
//!
//! - the four **state** events are what an engine or the watcher publishes onto
//!   `EventBus`;
//! - [`Event::Snapshot`] and [`Event::Resync`] are synthesized per connection — the
//!   first frame a subscriber receives, and the frame it receives again after the bus
//!   tells it that it fell behind;
//! - [`Event::Log`] carries a [`LogLine`] off the daemon's log ring, which is a
//!   *parallel* channel: a log line is not level-triggered (dropping one loses it for
//!   good), and mixing the two would let a chatty minute evict pending state events
//!   from the bus and leave a UI stale.
//!
//! They share this enum because they share a wire: every one is a frame of
//! `GET /api/events`, `name()` is its SSE `event:` line, and one vocabulary means one
//! fixture group and one set of names a client registers listeners for.

use serde::{Deserialize, Serialize};

use crate::routes::Snapshot;
use crate::vocab::wire_enum;

/// One push from the daemon.
///
/// Externally tagged on `event` for the same reason [`crate::Envelope`] is tagged on
/// `ok`: a consumer must be able to read the discriminator without guessing from
/// which fields happen to be present.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The declared root set changed (or was re-announced). `roots` is the full
    /// declared set, not a delta — the engine set diffs against it.
    RootsChanged { roots: Vec<String> },
    /// A root's sync state moved: accepted, completed, or failed. Carries no detail
    /// — v1's dashboards re-read `sync_info` on it, and so does today's UI.
    RootSyncChanged { slug: String },
    /// A background op took the root's background slot.
    TaskStarted { slug: String, kind: TaskKind },
    /// That op finished, with the disposition the engine recorded.
    TaskFinished {
        slug: String,
        kind: TaskKind,
        outcome: TaskOutcome,
    },
    /// The whole world, sent as the first frame of every connection so a UI renders
    /// without a second request.
    Snapshot(Snapshot),
    /// The whole world again, because this subscriber fell behind the bus and the
    /// events it missed are gone. Distinct from [`Self::Snapshot`] so a client can
    /// tell "here is the start" from "you lost your place" — the payload is the same
    /// because the recovery is the same: replace what you hold.
    Resync(Snapshot),
    /// One line off the daemon's log ring.
    Log(LogLine),
}

impl Event {
    /// The `event` tag, without serializing the payload — the SSE `event:` line and
    /// what `contracts/wire-vocab.json` pins as the `events` group.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::RootsChanged { .. } => "roots_changed",
            Self::RootSyncChanged { .. } => "root_sync_changed",
            Self::TaskStarted { .. } => "task_started",
            Self::TaskFinished { .. } => "task_finished",
            Self::Snapshot(_) => "snapshot",
            Self::Resync(_) => "resync",
            Self::Log(_) => "log",
        }
    }

    /// Whether this is one of the four **state** events the daemon's event bus
    /// carries, as opposed to a frame synthesized at the stream's edge.
    ///
    /// The bus is typed on this enum for one reason — every frame goes out the same
    /// wire — so this is what a publisher asserts against rather than the type
    /// system: a snapshot broadcast to every subscriber would be a per-connection
    /// answer sent to the wrong people, and a log line on the bus would evict the
    /// state events it shares a buffer with.
    #[must_use]
    pub const fn is_state(&self) -> bool {
        matches!(
            self,
            Self::RootsChanged { .. }
                | Self::RootSyncChanged { .. }
                | Self::TaskStarted { .. }
                | Self::TaskFinished { .. }
        )
    }

    /// Every event name, in declaration order. Hand-listed rather than macro-derived
    /// because these variants carry payloads: [`crate::vocab::wire_enum`] declares
    /// unit vocabularies, and the guard that keeps this list honest is
    /// [`Self::name`]'s exhaustive match plus the test below that walks one value of
    /// every variant through it.
    pub const NAMES: [&'static str; 7] = [
        "roots_changed",
        "root_sync_changed",
        "task_started",
        "task_finished",
        "snapshot",
        "resync",
        "log",
    ];
}

/// One line off the daemon's log ring — v1's `LogBuffer` entry, given a wire shape.
///
/// The whitelist is the point. A `tracing` event carries arbitrary fields of
/// arbitrary types, and a stream that forwarded all of them would publish whatever a
/// future `tracing::info!` happened to attach — including a URL with credentials in
/// it. So the ring records a fixed shape: level, target, message, and the handful of
/// fields grove itself keys on.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LogLine {
    /// Milliseconds since the Unix epoch, read through the clock seam.
    pub at_ms: u64,
    pub level: LogLevel,
    /// The emitting module path (`grove_daemon::engine`).
    pub target: String,
    pub message: String,
    /// The whitelisted structured fields, rendered as strings. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<LogField>,
}

/// One whitelisted field on a [`LogLine`]. A list of pairs rather than a map: it
/// preserves the order the macro wrote them in, which is the order an operator reads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LogField {
    pub name: String,
    pub value: String,
}

wire_enum! {
    /// A log line's severity — `tracing`'s five levels, in the order a filter reads
    /// them (least verbose first).
    pub enum LogLevel {
        Error => "error",
        Warn => "warn",
        Info => "info",
        Debug => "debug",
        Trace => "trace",
    }
}

wire_enum! {
    /// Which background op a task event is about.
    ///
    /// Exactly the engine's background slot: v1's `bg_kind` (`:reconcile | :sync |
    /// :fill`), in the driver's own priority order. Foreground work — a promote, a
    /// remove, a doctor converge — is the caller's own latency and answers on its
    /// own request, so it never becomes a task event.
    pub enum TaskKind {
        /// `root.reconcile` + the declared pool target, on the root's lane.
        Reconcile => "reconcile",
        /// `root.sync` — fetch, fast-forward the trunk, recycle stranded slots.
        Sync => "sync",
        /// `pool.fill` — one warm slot toward the declared target.
        Fill => "fill",
    }
}

wire_enum! {
    /// How a background op ended.
    ///
    /// Deliberately coarse. The engine's own dispositions (terminal vs transient, a
    /// `diverged` trunk vs a clean one) drive *its* next move; a consumer of this
    /// stream re-reads the root's status and sync note rather than reconstructing
    /// them from an outcome token, so a finer vocabulary here would be a second,
    /// drift-prone copy of state that is already published.
    pub enum TaskOutcome {
        /// The op ran and the engine accepted its result.
        Ok => "ok",
        /// The op failed, or its task never produced a result.
        Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, LogLevel, LogLine, TaskKind, TaskOutcome};
    use crate::routes::Snapshot;
    use serde_json::json;

    fn log_line() -> LogLine {
        LogLine {
            at_ms: 1_577_836_800_000,
            level: LogLevel::Info,
            target: "grove_daemon::engine".into(),
            message: "engine reconciled".into(),
            fields: Vec::new(),
        }
    }

    fn one_of_each() -> [Event; 7] {
        [
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
            Event::Snapshot(Snapshot::default()),
            Event::Resync(Snapshot::default()),
            Event::Log(log_line()),
        ]
    }

    /// The wire shapes, byte for byte — the tag inline with the payload, for the
    /// struct variants and for the three that wrap a payload type.
    #[test]
    fn events_serialize_tagged_on_the_event_field() {
        let [roots, sync, started, finished, snapshot, resync, log] = one_of_each();
        assert_eq!(
            serde_json::to_value(&snapshot).unwrap(),
            json!({"event": "snapshot", "roots": [], "logs": []}),
            "a newtype variant flattens its payload beside the tag"
        );
        assert_eq!(
            serde_json::to_value(&resync).unwrap()["event"],
            json!("resync")
        );
        assert_eq!(
            serde_json::to_value(&log).unwrap(),
            json!({
                "event": "log", "at_ms": 1_577_836_800_000u64, "level": "info",
                "target": "grove_daemon::engine", "message": "engine reconciled"
            }),
            "an empty field list is omitted rather than rendered as []"
        );
        assert_eq!(
            serde_json::to_value(&roots).unwrap(),
            json!({"event": "roots_changed", "roots": ["o/r"]})
        );
        assert_eq!(
            serde_json::to_value(&sync).unwrap(),
            json!({"event": "root_sync_changed", "slug": "o/r"})
        );
        assert_eq!(
            serde_json::to_value(&started).unwrap(),
            json!({"event": "task_started", "slug": "o/r", "kind": "reconcile"})
        );
        assert_eq!(
            serde_json::to_value(&finished).unwrap(),
            json!({
                "event": "task_finished", "slug": "o/r",
                "kind": "fill", "outcome": "failed"
            })
        );
    }

    #[test]
    fn events_round_trip() {
        for event in one_of_each() {
            let bytes = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&bytes).unwrap(), event);
        }
    }

    /// [`Event::name`] and the serde tag are two renderings of one vocabulary, and
    /// `NAMES` is a third. This walks one value of every variant through all three —
    /// the check that keeps the hand-written list from omitting a variant the way a
    /// payload-carrying enum cannot force at compile time.
    #[test]
    fn every_variant_names_itself_and_is_listed() {
        let events = one_of_each();
        assert_eq!(events.len(), Event::NAMES.len(), "a variant is unlisted");
        for event in &events {
            let tag = serde_json::to_value(event).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(tag, event.name(), "name() disagrees with the serde tag");
            assert!(
                Event::NAMES.contains(&event.name()),
                "{} missing from NAMES",
                event.name()
            );
        }
    }

    /// Which frames the event bus may carry. The three synthesized at the stream's
    /// edge share this enum but never that channel — see the module doc.
    #[test]
    fn only_the_four_state_events_belong_on_the_bus() {
        let states: Vec<&'static str> = one_of_each()
            .iter()
            .filter(|e| e.is_state())
            .map(Event::name)
            .collect();
        assert_eq!(
            states,
            [
                "roots_changed",
                "root_sync_changed",
                "task_started",
                "task_finished"
            ]
        );
    }

    #[test]
    fn task_vocabularies_match_their_serde_spelling() {
        for kind in TaskKind::ALL {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(kind.as_str().into())
            );
        }
        for outcome in TaskOutcome::ALL {
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                serde_json::Value::String(outcome.as_str().into())
            );
        }
    }

    /// Strict decoding, as everywhere else in this crate: an event name no producer
    /// in this binary emits is drift, not a value to skip.
    #[test]
    fn an_unknown_event_fails_to_decode() {
        assert!(serde_json::from_str::<Event>(r#"{"event":"log_line"}"#).is_err());
        assert!(serde_json::from_str::<TaskKind>("\"promote\"").is_err());
    }
}
