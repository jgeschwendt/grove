//! The two status vocabularies the daemon publishes: per-root engine status, and
//! the daemon's own boot state.
//!
//! Both crossed the wire in v1 and neither was pinned on either end — the CLI read
//! root status as a bare `String` and rendered whatever arrived, so a rename blanked
//! a badge instead of failing a build. `root_status` joins the fixture here;
//! `BootStatus` is not a fixture group because it never appears as a bare token —
//! it is always the object below.

use grove_ops::git::FastForward;
use serde::{Deserialize, Serialize};

use crate::vocab::wire_enum;

wire_enum! {
    /// One root's engine status, as `POST /api/doctor` reports it per slug.
    ///
    /// Server-only in the general case: `cloning`/`degraded` are facts about in-flight
    /// work that no disk read can recover, which is why the offline CLI can synthesize
    /// only `ready`/`missing` from the filesystem.
    ///
    /// Declared in the order the engine's lifecycle visits them. The declaration is
    /// also the `ALL` list `contracts/wire-vocab.json` pins as `root_status` and the
    /// `as_str` match — see [`crate::vocab`].
    pub enum RootStatus {
        /// Bare and trunk checkout on disk, no failure recorded.
        Ready => "ready",
        /// A clone is in flight.
        Cloning => "cloning",
        /// A terminal failure stopped the engine; it waits for an operator or a change.
        Degraded => "degraded",
        /// Declared, nothing on disk yet.
        Missing => "missing",
        /// No engine has run for this root yet — the cache has nothing to say.
        Unknown => "unknown",
        /// A *reader's* verdict, never a state the engine enters: the engine did not
        /// answer inside the reader's budget, or the root's lane would not run the
        /// reads the row needs.
        Unavailable => "unavailable",
    }
}

wire_enum! {
    /// What a root's last sync left behind, when it left anything.
    ///
    /// Carried law 9: a trunk carrying local commits or dirty tracked files is
    /// **reported, never forced**. This is the report — the engine records it, the
    /// snapshot publishes it beside `syncing`, and the next clean sync clears it.
    ///
    /// `failed` is v1's synthetic member: a sync that errored or whose task never
    /// returned leaves it, which is why this vocabulary is not simply
    /// `fast_forward` minus its successes.
    pub enum SyncNote {
        /// The trunk has commits the remote does not; a fast-forward would rewrite work.
        Diverged => "diverged",
        /// The trunk has dirty tracked files.
        Dirty => "dirty",
        /// The sync itself failed, or produced no result at all.
        Failed => "failed",
    }
}

impl SyncNote {
    /// The note a fast-forward outcome leaves, if any — exactly
    /// [`crate::policy::is_fast_forward_note`], with the outcome carried rather than
    /// discarded. Total over `FastForward`, so a new outcome there stops this
    /// compiling until it is classified.
    #[must_use]
    pub const fn of(outcome: FastForward) -> Option<Self> {
        match outcome {
            FastForward::Diverged => Some(Self::Diverged),
            FastForward::Dirty => Some(Self::Dirty),
            FastForward::Updated | FastForward::AlreadyCurrent => None,
        }
    }
}

/// The daemon's lifecycle state. Serializes to the object the health route puts in
/// `error.data`: `{"status":"booting"}`, `{"status":"degraded","reason":"…"}`.
///
/// `degraded` is **sticky** — it carries the reason that degraded the boot and is not
/// cleared by a later ready signal, because the self-update health gate depends on a
/// degraded daemon staying visibly degraded until it is replaced.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase", tag = "status")]
pub enum BootStatus {
    /// Starting up; the readiness gate 503s everything but the whitelist.
    Booting,
    /// Serving.
    Ready,
    /// Draining after a shutdown request; the whitelist still answers.
    Stopping,
    /// A fault that survives a `mark_ready` — reported with its reason.
    Degraded { reason: String },
}

impl BootStatus {
    /// The bare state token, without the reason — what `error.data.status` carries.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Booting => "booting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::Degraded { .. } => "degraded",
        }
    }

    /// Only `ready` serves non-whitelisted routes.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::{BootStatus, RootStatus, SyncNote};
    use crate::policy::is_fast_forward_note;
    use grove_ops::git::FastForward;
    use serde_json::json;

    /// The note vocabulary and the shared curation are one decision: every outcome
    /// the policy calls a note produces one here, and every outcome it does not
    /// produces none.
    #[test]
    fn sync_notes_agree_with_the_fast_forward_curation() {
        for outcome in [
            FastForward::Updated,
            FastForward::AlreadyCurrent,
            FastForward::Diverged,
            FastForward::Dirty,
        ] {
            assert_eq!(
                SyncNote::of(outcome).is_some(),
                is_fast_forward_note(outcome),
                "{outcome:?}"
            );
        }
        assert_eq!(
            SyncNote::of(FastForward::Diverged),
            Some(SyncNote::Diverged)
        );
        // The synthetic member: no fast-forward outcome produces it, and the engine
        // records it on a sync that never got that far.
        assert_eq!(SyncNote::Failed.as_str(), "failed");
        assert_eq!(SyncNote::ALL.len(), 3);
    }

    #[test]
    fn root_status_as_str_matches_the_serde_spelling() {
        for status in RootStatus::ALL {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(status.as_str().into()),
                "{status:?}"
            );
        }
    }

    /// The health route's non-ready bodies, byte for byte: a plain state is `{status}`
    /// and a degrade carries its reason beside it.
    #[test]
    fn boot_status_serializes_as_the_health_error_data() {
        assert_eq!(
            serde_json::to_value(BootStatus::Booting).unwrap(),
            json!({"status": "booting"})
        );
        assert_eq!(
            serde_json::to_value(BootStatus::Stopping).unwrap(),
            json!({"status": "stopping"})
        );
        assert_eq!(
            serde_json::to_value(BootStatus::Degraded {
                reason: "ops_incompatible".into()
            })
            .unwrap(),
            json!({"status": "degraded", "reason": "ops_incompatible"})
        );
    }

    #[test]
    fn boot_status_round_trips() {
        for status in [
            BootStatus::Booting,
            BootStatus::Ready,
            BootStatus::Stopping,
            BootStatus::Degraded {
                reason: "ops_incompatible".into(),
            },
        ] {
            let bytes = serde_json::to_string(&status).unwrap();
            assert_eq!(
                serde_json::from_str::<BootStatus>(&bytes).unwrap(),
                status,
                "{bytes}"
            );
            assert!(bytes.contains(status.as_str()));
        }
        assert!(BootStatus::Ready.is_ready());
        assert!(!BootStatus::Booting.is_ready());
    }
}
