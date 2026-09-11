//! Separate durable outcome contract; never changes MutationIndeterminate's promise.

use super::*;
use selene_persist::logical_stream as lower;

/// Proven recovery outcome, independent of whether this process published it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableCommitState {
    /// No append or successfully synchronized whole-transaction rollback.
    Canceled,
    /// Persistence could not be proved or disproved. Do not retry blindly.
    Uncertain,
    /// Entire transaction synchronized, but the call was not acknowledged.
    CommittedUnacknowledged,
}

/// Last attempted durable commit phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableCommitPhase {
    /// Preparation/admission before append.
    Prepare,
    /// Authoritative append.
    Append,
    /// Explicit file synchronization.
    Synchronize,
    /// Single outer catalog-and-graph publication.
    Publish,
    /// Acknowledgment after publication, including observer interruption.
    Acknowledge,
}

/// Exact WAL context for diagnostics, not a retention lease or retry token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableCommitPosition {
    /// Durable UUID bytes, unrelated to process-local DatabaseId.
    pub store: [u8; 16],
    /// Durable epoch.
    pub epoch: u64,
    /// Selected segment anchor; an offset alone has no meaning.
    pub segment: [u8; 32],
    /// Complete-record sequence.
    pub sequence: u64,
    /// End offset within precisely this segment.
    pub offset: u64,
}

/// Failure evidence returned by the format-2 commit path. `MutationIndeterminate` remains
/// the separate, already-published in-memory outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableCommitOutcome {
    /// Recovery evidence; uncertainty is not a rollback guarantee.
    pub state: DurableCommitState,
    /// Last attempted phase.
    pub phase: DurableCommitPhase,
    /// Whether this candidate was published live by this database instance.
    pub published: bool,
    /// Candidate boundary, when encoding completed.
    pub candidate: Option<DurableCommitPosition>,
    /// Last completely written record (not necessarily durable).
    pub written: DurableCommitPosition,
    /// Last successfully synchronized record.
    pub synchronized: DurableCommitPosition,
    /// Last acknowledged prefix, not the failed candidate.
    pub acknowledged: Option<DurableCommitPosition>,
}

impl From<lower::Position> for DurableCommitPosition {
    fn from(p: lower::Position) -> Self {
        Self {
            store: *p.store.as_bytes(),
            epoch: p.epoch.get(),
            segment: p.segment,
            sequence: p.sequence,
            offset: p.offset,
        }
    }
}

impl Error {
    pub(crate) fn local_commit_rollback(source: Error) -> Self {
        let mut error = Self::with_source(
            ErrorKind::TransactionRollback,
            "commit validation prevented commitment; the whole transaction was canceled",
            source,
        );
        error.status = Some(GqlStatus::DURABLE_COMMIT_ROLLBACK);
        error
    }
    pub(crate) fn named_type_violation(
        source: selene_graph::type_validator::TypeViolation,
    ) -> Self {
        let mut error = Self::with_source(ErrorKind::Execution, source.to_string(), source);
        error.status = Some(GqlStatus::GRAPH_TYPE_VIOLATION);
        error
    }
    /// Inspect durable recovery/visibility evidence. `None` preserves the existing
    /// in-memory/error contract and makes no durability statement. The source chain
    /// retains the primary I/O error; its commit failure also retains cleanup errors.
    #[must_use]
    pub fn durable_commit_outcome(&self) -> Option<&DurableCommitOutcome> {
        self.durable.as_deref()
    }

    pub(crate) fn durable_failure(source: Box<lower::CommitFailure>) -> Self {
        let (state, kind) = match source.durability {
            lower::Durability::Canceled => (
                DurableCommitState::Canceled,
                ErrorKind::DurableCommitCanceled,
            ),
            lower::Durability::Uncertain => (
                DurableCommitState::Uncertain,
                ErrorKind::DurableCommitUncertain,
            ),
            lower::Durability::Committed => (
                DurableCommitState::CommittedUnacknowledged,
                ErrorKind::DurableCommitUnacknowledged,
            ),
        };
        let phase = match source.phase {
            lower::CommitPhase::Prepare => DurableCommitPhase::Prepare,
            lower::CommitPhase::Append => DurableCommitPhase::Append,
            lower::CommitPhase::Synchronize => DurableCommitPhase::Synchronize,
            lower::CommitPhase::Publish => DurableCommitPhase::Publish,
            lower::CommitPhase::Acknowledge => DurableCommitPhase::Acknowledge,
        };
        let outcome = DurableCommitOutcome {
            state,
            phase,
            published: source.candidate.is_some() && source.progress.published == source.candidate,
            candidate: source.candidate.map(Into::into),
            written: source.progress.written.into(),
            synchronized: source.progress.synchronized.into(),
            acknowledged: source.progress.acknowledged.map(Into::into),
        };
        let mut error = Self::facade(
            kind,
            format!("{source}; do not retry without resolving this outcome"),
        );
        error.source = Some(source);
        error.durable = Some(Box::new(outcome));
        error
    }
}
