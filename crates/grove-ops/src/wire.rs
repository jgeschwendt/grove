//! The wire vocabulary — the one Rust source for every status *value* that leaves
//! this crate for the HTTP API and, through it, a UI.
//!
//! Each vocabulary is a serde enum carrying its own wire spelling via
//! `#[serde(rename_all)]`, so the type *is* the single source: a status field holds
//! a variant, never a hand-written string, and a mismatched spelling is a compile
//! error rather than a badge that quietly stops matching. `git::FastForward` and
//! `pool::ColdReason` are the same pattern declared beside their producers; they
//! belong to this vocabulary too.
//!
//! The one holdout is [`promotion`], still `pub const`s: no route publishes a
//! promotion. A promote is an internal step of realizing a declared worktree —
//! `worktrees::create`/`reconcile` claim a warm slot and answer in the realizer's own
//! vocabulary — so the outcome reaches a consumer only as the worktree that appeared,
//! and this vocabulary reaches the wire only through the fixture. It becomes an enum
//! on `pool::Promotion` the moment something serializes one.
//!
//! `grove-api`'s `tests/wire_vocab.rs` snapshots the whole vocabulary (these enums'
//! serialized forms, `Error::code()`, and grove-api's own HTTP/root-status
//! vocabularies) to `contracts/wire-vocab.json`. A rename here becomes a failing Rust
//! snapshot test, so it cannot land unnoticed on this side; nothing outside this repo
//! reads the fixture, so downstream it is still a silently-blank badge. Re-bless with
//! `BLESS_WIRE=1 cargo test -p grove-api`.

use serde::{Deserialize, Serialize};

/// Reconcile outcome — [`crate::roots::Applied::status`] from `reconcile_one`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReconcileStatus {
    /// A cold clone just landed the bare repo.
    Cloned,
    /// The bare repo was already present on disk.
    Present,
    /// The clone/guard failed — the root is unusable.
    Failed,
}

/// Adopt outcome — [`crate::roots::Applied::status`] from `adopt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdoptStatus {
    /// An undeclared on-disk bare was written into the manifest.
    Adopted,
    /// A candidate directory was passed over (not an adoptable bare).
    Skipped,
}

/// Worktree reconcile outcome — [`crate::worktrees::WorktreeOutcome::status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeOutcomeStatus {
    /// A declared-but-missing checkout was re-realized.
    Recreated,
    /// An in-git-but-undeclared worktree was written into the manifest.
    Adopted,
    /// The realize/adopt failed for one worktree (others still processed).
    Failed,
}

/// Promote result — the `pool.promote` dispatch `status`.
pub mod promotion {
    /// A warm slot was claimed as the requested worktree.
    pub const PROMOTED: &str = "promoted";
    /// No promote happened; the caller falls back to a cold create. The `reason`
    /// field then carries a `pool::ColdReason`.
    pub const COLD: &str = "cold";
}
