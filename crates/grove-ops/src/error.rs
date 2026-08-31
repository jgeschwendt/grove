//! `grove_ops::Error` — a categorized failure carrying a stable `snake_case` wire code.
//!
//! Every public op returns this. The category is set at the point the failure is
//! detected (a not-declared root vs. a git subprocess vs. a network fetch); the
//! wrapped `String` is the already-formatted, user-facing message — byte-identical to
//! what the pre-typed `anyhow` `{e:#}` boundary produced, so output is unchanged.
//! Two consumers read the category: the HTTP error envelope surfaces [`Error::code`]
//! so the daemon's engine can tell a terminal failure from a transient one, and the CLI
//! maps it to a process exit code (`impl From<Error> for CliError`, in the `grove`
//! crate). Internals stay on `anyhow` for context building; conversion happens at each
//! public-function boundary via the `map_err` constructors below.

use thiserror::Error;

/// A categorized grove-ops failure. The variant is the category; the `String` is the
/// preserved, already-formatted message.
#[derive(Debug, Error)]
pub enum Error {
    /// A root/slug not present in the manifest — nothing declared to act on.
    #[error("{0}")]
    NotDeclared(String),
    /// Declared but not yet realized/ready on disk (a missing bare or `.trunk`).
    #[error("{0}")]
    NotReady(String),
    /// Bad or missing caller input (malformed slug/name, missing param, unknown op).
    #[error("{0}")]
    InvalidInput(String),
    /// Already-exists / in-progress / would-clobber.
    #[error("{0}")]
    Conflict(String),
    /// Remote / network / fetch failure.
    #[error("{0}")]
    Network(String),
    /// A `git` subprocess failure.
    #[error("{0}")]
    Git(String),
    /// Filesystem / IO / TOML read-write / serialization failure.
    #[error("{0}")]
    Io(String),
}

/// A grove-ops result whose error carries a category + wire code.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The stable `snake_case` wire code for this category — the error envelope's
    /// `code` field, read by the daemon's engine to tell terminal from transient.
    // stele:landmark wire-error-codes
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Error::NotDeclared(_) => "not_declared",
            Error::NotReady(_) => "not_ready",
            Error::InvalidInput(_) => "invalid_input",
            Error::Conflict(_) => "conflict",
            Error::Network(_) => "network",
            Error::Git(_) => "git",
            Error::Io(_) => "io",
        }
    }
}

/// Category constructors from an internal `anyhow::Error`: format the full chain
/// (`{:#}`) into the variant, preserving the exact text the pre-typed `{e:#}` boundary
/// emitted. Used as `.map_err(Error::git)` at each public-function seam. (The
/// not-declared / not-ready / conflict categories are set inline at their detection
/// sites, where the message is a literal, so they need no `anyhow` constructor here.)
#[allow(
    clippy::needless_pass_by_value,
    reason = "used point-free as `.map_err(Error::git)`; map_err hands over an owned error"
)]
impl Error {
    pub(crate) fn git(e: anyhow::Error) -> Self {
        Self::Git(format!("{e:#}"))
    }
    pub(crate) fn io(e: anyhow::Error) -> Self {
        Self::Io(format!("{e:#}"))
    }
    pub(crate) fn invalid_input(e: anyhow::Error) -> Self {
        Self::InvalidInput(format!("{e:#}"))
    }
    pub(crate) fn network(e: anyhow::Error) -> Self {
        Self::Network(format!("{e:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_stable_snake_case_per_variant() {
        let m = || "x".to_string();
        assert_eq!(Error::NotDeclared(m()).code(), "not_declared");
        assert_eq!(Error::NotReady(m()).code(), "not_ready");
        assert_eq!(Error::InvalidInput(m()).code(), "invalid_input");
        assert_eq!(Error::Conflict(m()).code(), "conflict");
        assert_eq!(Error::Network(m()).code(), "network");
        assert_eq!(Error::Git(m()).code(), "git");
        assert_eq!(Error::Io(m()).code(), "io");
    }

    #[test]
    fn display_is_the_bare_message() {
        assert_eq!(
            Error::Git("git worktree add failed: boom".into()).to_string(),
            "git worktree add failed: boom"
        );
    }

    #[test]
    fn anyhow_constructors_preserve_the_formatted_chain() {
        let e = anyhow::anyhow!("root cause").context("outer");
        // `{e:#}` joins the chain — the same text the pre-typed boundary produced.
        assert_eq!(Error::git(e).to_string(), "outer: root cause");
    }
}
