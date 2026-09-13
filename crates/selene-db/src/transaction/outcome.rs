//! Durability-independent outcomes at the outer authority cut-line.

use crate::{Error, Result};

/// Result of the in-memory authority cut-line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "non-committed outcomes are also used by test failpoints"
)]
pub(crate) enum AuthorityOutcome {
    /// Publication was canceled before the outer state store.
    Canceled,
    /// The complete state was stored and acknowledged.
    Committed,
    /// The complete state was stored, but acknowledgement was uncertain.
    Indeterminate,
}

pub(crate) fn require_committed(outcome: AuthorityOutcome) -> Result<()> {
    match outcome {
        AuthorityOutcome::Committed => Ok(()),
        AuthorityOutcome::Canceled => Err(Error::mutation_canceled()),
        AuthorityOutcome::Indeterminate => Err(Error::mutation_indeterminate()),
    }
}
