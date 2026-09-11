//! Phase information independent of a graph runtime or GQL diagnostics.

use crate::{
    PersistError,
    control::{StoreEpoch, StoreId},
    logical_frame::FrameError,
};

/// Exact complete-record boundary or selected segment base. Offsets are meaningful
/// only within this identity; rotation never transfers offsets between files.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Position {
    /// Durable store identity.
    pub store: StoreId,
    /// Durable store epoch.
    pub epoch: StoreEpoch,
    /// Selected segment lineage anchor.
    pub segment: [u8; 32],
    /// Global complete-record sequence; a rotated empty base may be nonzero.
    pub sequence: u64,
    /// Byte offset after the record in this segment only, zero at its declared base.
    pub offset: u64,
    /// Complete record digest, or the independently selected segment origin at its base.
    pub digest: [u8; 32],
}

/// Independently established boundaries; synchronization advances only on success.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    /// Last completely written record or selected base, not a durability promise.
    pub written: Position,
    /// Last successfully synchronized record or selected base.
    pub synchronized: Position,
    /// Last outer publication reported by the sole facade authority.
    pub published: Option<Position>,
    /// Last successful acknowledgment returned by the authority.
    pub acknowledged: Option<Position>,
}

/// Phase at which a commit stopped. Cleanup failure is preserved separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitPhase {
    /// No authoritative append has started.
    Prepare,
    /// Some candidate bytes may have been written.
    Append,
    /// Synchronizing a complete candidate group.
    Synchronize,
    /// Synchronized state awaits the outer publication.
    Publish,
    /// Published state awaits successful acknowledgment.
    Acknowledge,
}

/// What the evidence proves about recovery, independent of live visibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Durability {
    /// No append, or rollback was explicitly synchronized; candidate is absent.
    Canceled,
    /// Rollback could not be proved durable; recovery may include candidate records.
    Uncertain,
    /// The complete candidate group synchronized successfully and must not be undone.
    Committed,
}

/// Framing, control, I/O, or protocol error, retaining concrete causal errors.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// Validated artifact and bounded cursor context supplied by the reader.
    #[error("format-2 artifact {name:?} at {offset:?}, next {expected_sequence:?}: {source}")]
    Artifact {
        /// Managed basename, not I/O authority.
        name: String,
        /// Byte cursor within this artifact, where known.
        offset: Option<u64>,
        /// Trusted next sequence at the cursor, where known.
        expected_sequence: Option<u64>,
        /// Original typed physical failure.
        source: Box<StreamError>,
    },
    /// The selected database lacks an initial full snapshot.
    #[error("missing initial database snapshot")]
    MissingSnapshot,
    /// Specifically incomplete captured unsealed suffix; no truncation is authorized.
    #[error("incomplete authoritative WAL tail; no repair")]
    IncompleteTail,
    /// A prior failure permanently terminated this reader.
    #[error("format-2 reader terminated by failure")]
    Terminated,
    /// Retained-directory/control failure.
    #[error(transparent)]
    Persist(#[from] PersistError),
    /// File operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Frame integrity, context, or bounds failure.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// Invalid use, fenced ownership, or a bounded resource ceiling.
    #[error("format-2 stream: {0}")]
    Protocol(&'static str),
    /// A synchronous operation unwound; panic payloads are not retained.
    #[error("format-2 commit operation panicked")]
    Panicked,
    /// Owning runtime preparation failed before any append.
    #[error("commit preparation failed: {0}")]
    Preparation(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl StreamError {
    pub(super) fn at(
        self,
        name: impl Into<String>,
        offset: Option<u64>,
        expected_sequence: Option<u64>,
    ) -> Self {
        Self::Artifact {
            name: name.into(),
            offset,
            expected_sequence,
            source: Box::new(self),
        }
    }
}

/// Failed commit with phase, recovery evidence, live progress and causal cleanup failure.
#[derive(Debug, thiserror::Error)]
#[error("format-2 commit stopped in {phase:?}: {durability:?}: {source}")]
pub struct CommitFailure {
    /// Last attempted commit phase.
    pub phase: CommitPhase,
    /// Proven recovery outcome.
    pub durability: Durability,
    /// Established boundaries at return, distinct from the candidate.
    pub progress: Progress,
    /// Complete candidate group's intended last boundary, if prepared.
    pub candidate: Option<Position>,
    /// Primary cause of failure.
    #[source]
    pub source: StreamError,
    /// Additional failure proving that rollback cannot be claimed.
    pub cleanup: Option<StreamError>,
}
