//! grove-api — the shared HTTP contract: envelope, request/response types, error-code
//! vocabularies. The daemon serves it and the CLI client decodes it from these types,
//! so neither side can drift from the other's spelling.
//!
//! Three properties this crate exists to hold:
//!
//! 1. **One serializer.** [`Envelope`] is the only thing that writes `{ok, …}`. v1
//!    hand-wrote the shape in four places and nothing pinned them to each other.
//! 2. **Discrimination on `ok`.** The envelope is tagged on its `ok` boolean, not on
//!    which of `data`/`error` happens to be present — v1's untagged decoder would read
//!    `{"ok":true,"error":{…}}` as a failure.
//! 3. **Typed vocabularies.** Error codes, root status, boot status and the engine's
//!    policy curations are enums, snapshotted into `contracts/wire-vocab.json` by
//!    `tests/wire_vocab.rs`, so a rename breaks a build or a test rather than a badge.

pub mod codes;
pub mod envelope;
pub mod events;
pub mod policy;
pub mod routes;
pub mod status;
mod vocab;

pub use codes::ErrorCode;
pub use envelope::{ApiError, Envelope};
pub use events::{Event, LogField, LogLevel, LogLine, TaskKind, TaskOutcome};
pub use policy::{ErrorDisposition, RootDisposition};
pub use routes::{
    DoctorData, DoctorRequest, HealthData, HealthStatus, PoolView, ReconcileAck, ReconcileData,
    RemoveRootData, RemoveRootRequest, RemoveWorktreeData, RemoveWorktreeRequest, RootStatusEntry,
    RootView, ShutdownData, Snapshot, SyncAck, SyncData, SyncRequest, VersionData, WorktreeView,
};
pub use status::{BootStatus, RootStatus, SyncNote};
