//! CLI error type with stable process exit codes.
//!
//! Codes are a contract — scripts and the installer branch on them, so the
//! mapping stays fixed.

use std::fmt;

#[derive(Debug)]
pub enum CliError {
    /// General failure or API error. Exit 1.
    Api(String),
    /// Requested resource not found. Exit 3.
    NotFound(String),
    /// The daemon is unreachable or failed. Exit 4.
    Daemon(String),
    /// Conflicting state — already exists / in progress. Exit 5.
    Conflict(String),
    /// Health check failed. Exit 6.
    Unhealthy(String),
    /// Self-update failed. Exit 7.
    Update(String),
}

impl CliError {
    /// Process exit code for this error.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Api(_) => 1,
            Self::NotFound(_) => 3,
            Self::Daemon(_) => 4,
            Self::Conflict(_) => 5,
            Self::Unhealthy(_) => 6,
            Self::Update(_) => 7,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Self::Api(m)
        | Self::NotFound(m)
        | Self::Daemon(m)
        | Self::Conflict(m)
        | Self::Unhealthy(m)
        | Self::Update(m)) = self;
        f.write_str(m)
    }
}

impl std::error::Error for CliError {}

/// Map a categorized `grove_ops::Error` onto the CLI's exit-code taxonomy, carrying
/// the message through unchanged. This is what makes the offline-ops exit codes
/// reachable: `?` on a `grove_ops` call at a CLI dispatch site routes the category to
/// its code (a not-declared root → `NotFound(3)`, a not-ready/conflict → `Conflict(5)`)
/// instead of the old blanket `Api(1)`.
impl From<grove_ops::Error> for CliError {
    fn from(e: grove_ops::Error) -> Self {
        use grove_ops::Error::{Conflict, Git, InvalidInput, Io, Network, NotDeclared, NotReady};
        let msg = e.to_string();
        match e {
            NotDeclared(_) => Self::NotFound(msg),
            NotReady(_) | Conflict(_) => Self::Conflict(msg),
            InvalidInput(_) | Network(_) | Git(_) | Io(_) => Self::Api(msg),
        }
    }
}

/// An error envelope from the server reaches the operator as an `Api(1)` failure
/// carrying `code: message` — the `ErrorCode` travels into what gets printed, so
/// what an operator sees can be matched against the contract table — **plus
/// `error.data`'s detail when the route sent any**.
///
/// The detail is not decoration. `doctor_failed` and `remove_failed` carry
/// deliberately generic messages whose whole content is `data.reason`, so dropping it
/// printed `doctor_failed: doctor failed` at the moment the daemon knew the manifest's
/// TOML parse error down to the line and column — and `remove_failed: remove failed`
/// when the removal was refused precisely to protect the operator's uncommitted work.
/// v2 carries `error.data` on the wire; this is where it reaches a human.
///
/// The HTTP taxonomy is *not* remapped onto the exit codes here: the two partitions
/// are unrelated (a 404 `not_found` from the API is a declared-state failure, not the
/// `grove_ops::Error::NotDeclared` that exit 3 names), and v1 pinned this collapse
/// with `remove_tree_error_envelope_maps_to_api`.
impl From<grove_api::ApiError> for CliError {
    fn from(e: grove_api::ApiError) -> Self {
        Self::Api(match e.detail() {
            Some(detail) => format!("{e}: {detail}"),
            None => e.to_string(),
        })
    }
}

/// The daemon's own startup failures — a refused bind, a held port — reach the
/// operator through `grove serve` as a daemon failure (exit 4).
impl From<grove_daemon::Error> for CliError {
    fn from(e: grove_daemon::Error) -> Self {
        Self::Daemon(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::CliError;
    use grove_api::{ApiError, ErrorCode};

    #[test]
    fn exit_codes_are_stable() {
        let m = || "x".to_string();
        assert_eq!(CliError::Api(m()).exit_code(), 1);
        assert_eq!(CliError::NotFound(m()).exit_code(), 3);
        assert_eq!(CliError::Daemon(m()).exit_code(), 4);
        assert_eq!(CliError::Conflict(m()).exit_code(), 5);
        assert_eq!(CliError::Unhealthy(m()).exit_code(), 6);
        assert_eq!(CliError::Update(m()).exit_code(), 7);
    }

    #[test]
    fn a_grove_ops_error_keeps_its_category_and_message() {
        let err = CliError::from(grove_ops::Error::NotDeclared("root o/r".into()));
        assert_eq!(err.exit_code(), 3);
        assert_eq!(err.to_string(), "root o/r");
        assert_eq!(
            CliError::from(grove_ops::Error::Conflict("busy".into())).exit_code(),
            5
        );
        assert_eq!(
            CliError::from(grove_ops::Error::Network("down".into())).exit_code(),
            1
        );
    }

    /// v1's `remove_tree_error_envelope_maps_to_api`: an error envelope at any HTTP
    /// status becomes exit 1, and the code string reaches the operator's message.
    #[test]
    fn an_error_envelope_maps_to_api_and_carries_the_code() {
        let err = CliError::from(ApiError::new(ErrorCode::NotFound, "root not declared"));
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.to_string(), "not_found: root not declared");
    }

    /// …and `error.data` reaches the operator, which is the whole of what the two
    /// generic codes have to say. Without this, the diagnostic command prints
    /// `doctor_failed: doctor failed` over a manifest whose parse error the daemon
    /// knows exactly, and the destructive command prints `remove_failed: remove
    /// failed` over work it just refused to destroy.
    #[test]
    fn an_error_envelope_carries_its_data_detail_to_the_operator() {
        let err = CliError::from(
            ApiError::new(ErrorCode::DoctorFailed, "doctor failed").with_data(
                serde_json::json!({"reason": "parse manifest: TOML parse error at line 24, column 6"}),
            ),
        );
        assert_eq!(
            err.to_string(),
            "doctor_failed: doctor failed: parse manifest: TOML parse error at line 24, column 6"
        );

        let refused = CliError::from(
            ApiError::new(ErrorCode::RemoveFailed, "remove failed")
                .with_data(serde_json::json!({"reason": "o/r has unsaved work in feat"})),
        );
        assert!(
            refused.to_string().contains("unsaved work in feat"),
            "{refused}"
        );

        // A payload with no `reason` still names what it is about.
        let missing = CliError::from(
            ApiError::new(ErrorCode::NotFound, "worktree not declared")
                .with_data(serde_json::json!({"slug": "o/r", "name": "feat"})),
        );
        assert!(missing.to_string().contains("name=feat"), "{missing}");
        assert!(missing.to_string().contains("slug=o/r"), "{missing}");
    }
}
