//! The HTTP `error.code` vocabulary.
//!
//! In v1 these were Elixir atoms written inline at each controller call site — the
//! one vocabulary crossing the API that nothing pinned, on either end. Here they are
//! a closed enum, snapshotted into `contracts/wire-vocab.json` as `http_error_codes`.
//!
//! Decoding is **strict**: an unrecognized code is a decode failure, not a silent
//! fallback onto [`ErrorCode::Error`]. The vocabulary is closed (one binary serves
//! and consumes it, and the self-update gate refuses a version mismatch on the port),
//! so an unknown code means genuine drift, and the whole point of this crate is that
//! drift is loud.

use crate::vocab::wire_enum;

wire_enum! {
    /// An `error.code` on the HTTP surface. Distinct from `grove_ops::Error::code` —
    /// that taxonomy classifies an *operation*'s failure and drives the engine's
    /// terminal/transient split; this one classifies a *request*'s and drives the HTTP
    /// status. The two are unrelated partitions and both are contract.
    ///
    /// Declared in the order the status table declares them: readiness, guard, then
    /// per-route. The declaration is also the `ALL` list and the `as_str` match — see
    /// [`crate::vocab`] for why it is a macro rather than three hand-synced copies.
    pub enum ErrorCode {
        /// Health, while boot state is `degraded` — 503, with `data.reason`.
        Degraded => "degraded",
        /// Draining, booting, or degraded behind the readiness gate — 503.
        Unavailable => "unavailable",
        /// The mutation guard refused a cross-origin state-changing request — 403.
        Forbidden => "forbidden",
        /// The slug or worktree named is not declared — 404, with `data.slug`.
        NotFound => "not_found",
        /// A required body parameter is missing or malformed — 422.
        InvalidRequest => "invalid_request",
        /// A sync the daemon could not even decide on — 422, with `data.reason`.
        /// Distinct from a sync that ran and reported a diverged trunk, which is not
        /// a failure at all: the engine records the note and the snapshot publishes
        /// it (carried law 9 — reported, never forced).
        SyncFailed => "sync_failed",
        /// A remove that reached the ops layer and failed there — 422, `data.reason`.
        RemoveFailed => "remove_failed",
        /// A doctor run that failed — 422, with `data.reason`.
        DoctorFailed => "doctor_failed",
        /// The fallback: a framework-level 404/500 with no route-specific code.
        Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::ErrorCode;

    /// [`ErrorCode::as_str`] and the serde spelling are two renderings of one
    /// vocabulary; the macro emits both from a single literal, and this is the
    /// regression guard on that wiring.
    #[test]
    fn as_str_matches_the_serde_spelling() {
        for code in ErrorCode::ALL {
            assert_eq!(
                serde_json::to_value(code).unwrap(),
                serde_json::Value::String(code.as_str().into()),
                "{code:?}"
            );
            assert_eq!(
                serde_json::from_str::<ErrorCode>(&format!("\"{}\"", code.as_str())).unwrap(),
                code
            );
        }
    }

    #[test]
    fn all_is_free_of_duplicates() {
        let mut seen: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        seen.sort_unstable();
        let len = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), len, "a code is listed twice in ALL");
    }

    /// The strictness is deliberate (see the module doc), so it gets a test rather
    /// than being an accident of the derive.
    #[test]
    fn an_unknown_code_fails_to_decode() {
        assert!(serde_json::from_str::<ErrorCode>("\"teapot\"").is_err());
    }
}
