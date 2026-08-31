//! Engine policy — the curations v1 kept in Elixir's `Grove.Wire`, given a typed home.
//!
//! These are *policy*, not vocabulary: which reconcile outcomes mean a healthy root,
//! which fast-forward outcomes are a note rather than a failure, which error
//! categories stop an engine instead of leaving it to retry. v1 expressed each as a
//! list of strings curated beside the vocabulary, with a compile-time check that the
//! members still existed and a runtime test that the reconcile lists *partitioned*
//! the vocabulary — because a status falling through both lists hit a catch-all and
//! was silently treated as healthy.
//!
//! Here each is a total match on a typed enum, so the partition is a compile-time
//! property: add a variant to `ReconcileStatus`, `FastForward`, or `grove_ops::Error`
//! and this file stops compiling until the new case is classified. The list constants
//! remain as the enumerated form the fixture test checks against.

use grove_ops::Error;
use grove_ops::git::FastForward;
use grove_ops::wire::ReconcileStatus;

/// What a reconcile outcome means for the root's engine state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootDisposition {
    /// The root is realized; the engine may sync, fill, and promote.
    Ready,
    /// The root is not usable; the engine stops until something changes.
    Degraded,
}

/// v1's `reconcile_ready`.
pub const RECONCILE_READY: [ReconcileStatus; 2] =
    [ReconcileStatus::Cloned, ReconcileStatus::Present];

/// v1's `reconcile_degraded`.
pub const RECONCILE_DEGRADED: [ReconcileStatus; 1] = [ReconcileStatus::Failed];

/// The reconcile partition, total by construction: every outcome is ready or
/// degraded, and there is no third answer for one to fall through into.
#[must_use]
pub const fn classify_reconcile(status: ReconcileStatus) -> RootDisposition {
    match status {
        ReconcileStatus::Cloned | ReconcileStatus::Present => RootDisposition::Ready,
        ReconcileStatus::Failed => RootDisposition::Degraded,
    }
}

/// v1's `fast_forward_note`.
pub const FAST_FORWARD_NOTE: [FastForward; 2] = [FastForward::Diverged, FastForward::Dirty];

/// Is this fast-forward outcome a *note* — a trunk sync deliberately declined,
/// reported and left alone (carried law 9) — rather than an ordinary success?
#[must_use]
pub const fn is_fast_forward_note(outcome: FastForward) -> bool {
    match outcome {
        FastForward::Diverged | FastForward::Dirty => true,
        FastForward::Updated | FastForward::AlreadyCurrent => false,
    }
}

/// What an ops failure means for the engine's next move.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorDisposition {
    /// Retrying cannot help: degrade the root and stop until something changes.
    Terminal,
    /// A failure the next event may clear; re-derive from disk and wait.
    Transient,
}

/// v1's `terminal_error_codes`, applied to a real failure rather than its spelling.
///
/// Note the asymmetry this preserves: `invalid_input`/`conflict` are the engine's
/// terminal pair, while the CLI's exit-code map groups `invalid_input` with
/// `network`/`git`/`io`. Two unrelated partitions over one taxonomy; both are
/// contract.
#[must_use]
pub const fn classify_error(err: &Error) -> ErrorDisposition {
    match err {
        Error::Conflict(_) | Error::InvalidInput(_) => ErrorDisposition::Terminal,
        Error::NotDeclared(_)
        | Error::NotReady(_)
        | Error::Network(_)
        | Error::Git(_)
        | Error::Io(_) => ErrorDisposition::Transient,
    }
}

/// The terminal codes as wire strings, read off real `Error` values so a rename in
/// `Error::code` travels here rather than leaving a stale literal behind.
#[must_use]
pub fn terminal_error_codes() -> [&'static str; 2] {
    [
        Error::Conflict(String::new()).code(),
        Error::InvalidInput(String::new()).code(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        ErrorDisposition, FAST_FORWARD_NOTE, RECONCILE_DEGRADED, RECONCILE_READY, RootDisposition,
        classify_error, classify_reconcile, is_fast_forward_note, terminal_error_codes,
    };
    use grove_ops::Error;
    use grove_ops::git::FastForward;

    /// The port of the Elixir suite's `reconcile partition is total`: the two lists
    /// agree with the classifier and neither claims a member of the other. Totality
    /// itself is the compiler's job — [`classify_reconcile`] has no catch-all — so
    /// this pins the *lists* against the function they document.
    #[test]
    fn the_reconcile_lists_agree_with_the_classifier() {
        for status in RECONCILE_READY {
            assert_eq!(
                classify_reconcile(status),
                RootDisposition::Ready,
                "{status:?}"
            );
        }
        for status in RECONCILE_DEGRADED {
            assert_eq!(
                classify_reconcile(status),
                RootDisposition::Degraded,
                "{status:?}"
            );
        }
        assert!(
            !RECONCILE_READY
                .iter()
                .any(|r| RECONCILE_DEGRADED.contains(r)),
            "the two halves overlap"
        );
    }

    #[test]
    fn the_fast_forward_note_list_agrees_with_the_predicate() {
        for outcome in FAST_FORWARD_NOTE {
            assert!(is_fast_forward_note(outcome), "{outcome:?}");
        }
        assert!(!is_fast_forward_note(FastForward::Updated));
        assert!(!is_fast_forward_note(FastForward::AlreadyCurrent));
    }

    /// One test per terminal code, as v1 generated: the code is terminal and every
    /// other category is transient.
    #[test]
    fn terminal_codes_are_exactly_conflict_and_invalid_input() {
        let m = String::new;
        let terminal = terminal_error_codes();
        assert_eq!(terminal, ["conflict", "invalid_input"]);

        for err in [Error::Conflict(m()), Error::InvalidInput(m())] {
            assert!(terminal.contains(&err.code()), "{}", err.code());
            assert_eq!(classify_error(&err), ErrorDisposition::Terminal);
        }
        for err in [
            Error::NotDeclared(m()),
            Error::NotReady(m()),
            Error::Network(m()),
            Error::Git(m()),
            Error::Io(m()),
        ] {
            assert!(!terminal.contains(&err.code()), "{}", err.code());
            assert_eq!(classify_error(&err), ErrorDisposition::Transient);
        }
    }
}
