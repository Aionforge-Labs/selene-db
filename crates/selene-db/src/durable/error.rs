use selene_persist::{ControlError, DirectoryError, PersistError, logical_stream::StreamError};
use std::{error::Error as StdError, fmt};

/// Phase of public durable lifecycle failure, separate from transaction outcomes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoragePhase {
    /// Initial directory anchoring.
    Anchor,
    /// Strict initial creation.
    Create,
    /// Authoritative selection and ownership.
    Select,
    /// Complete snapshot validation.
    Snapshot,
    /// Whole-transaction suffix replay and prefix verification.
    Replay,
    /// Eager all-index and runtime reconstruction.
    Rebuild,
    /// Establishing synchronized append ownership.
    Synchronize,
    /// Immutable checkpoint encoding/publication.
    Checkpoint,
    /// Explicit retention validation and cleanup.
    Prune,
}
/// Actionable failure categories; no failed open returns a Database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StorageErrorKind {
    /// Native I/O failure.
    Io,
    /// Native filesystem mode is unavailable.
    UnsupportedPlatform,
    /// Unsupported format/version or foreign artifacts.
    UnsupportedFormat,
    /// Existing store or unpublished bootstrap artifacts prevent strict creation.
    AlreadyInitialized,
    /// No initialized format-2 database exists.
    NotInitialized,
    /// Another owning database/session retains LOCK.
    Contention,
    /// Profile, Unicode or collation identity differs.
    Compatibility,
    /// A required selected artifact is missing (not permission denied).
    MissingArtifact,
    /// A managed artifact name, file type or link violates the directory capability contract.
    InvalidArtifact,
    /// Common header or complete authoritative checksum failed.
    Integrity,
    /// Noncanonical or structurally invalid authoritative bytes.
    Corruption,
    /// Control generation, origin or checkpoint-boundary lineage is inconsistent.
    Lineage,
    /// Durable StoreId differs from the trusted selected context.
    ForeignStore,
    /// Durable epoch differs from the trusted selected context.
    ForeignEpoch,
    /// Segment anchor differs from the trusted selected context.
    ForeignSegment,
    /// Previous-record or origin digest differs from the trusted boundary.
    DigestLineage,
    /// Exact next record sequence is greater than expected.
    SequenceGap,
    /// Record sequence repeats or precedes the exact expected sequence.
    SequenceOverlap,
    /// Specifically incomplete captured final unsealed tail; no repair is authorized.
    IncompleteTail,
    /// Incomplete required snapshot, sealed frame or interior frame.
    IncompleteRequired,
    /// Logical catalog, graph, type, value or constraint validation rejected the image.
    Semantic,
    /// Native declarations or eager all-index reconstruction rejected readiness.
    NativeAdmission,
    /// Invalid lifecycle use or other state admission failure.
    InvalidState,
    /// An enforced aggregate resource ceiling was exhausted.
    ResourceLimit,
    /// Requires dropping all owning handles and non-destructive reopen.
    Fenced,
    /// CURRENT may select the new complete snapshot; never retry blindly.
    CheckpointUncertain,
    /// A memory-only database has no durable checkpoint authority.
    InMemory,
}
/// Facade-owned lifecycle diagnostic with its concrete causal chain retained privately.
pub struct StorageError {
    /// Last attempted lifecycle phase.
    pub phase: StoragePhase,
    /// Actionable failure category.
    pub kind: StorageErrorKind,
    /// Bounded escaped artifact basename, where known; never filesystem authority.
    pub artifact: Option<String>,
    /// Validated byte cursor within that artifact, where known.
    pub offset: Option<u64>,
    /// Trusted sequence expected at that cursor, where known.
    pub expected_sequence: Option<u64>,
    /// Observed sequence only after header integrity passed, where applicable.
    pub observed_sequence: Option<u64>,
    source: Box<dyn StdError + Send + Sync>,
}
impl StorageError {
    pub(crate) fn new(
        phase: StoragePhase,
        kind: StorageErrorKind,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            phase,
            kind,
            artifact: None,
            offset: None,
            expected_sequence: None,
            observed_sequence: None,
            source: Box::new(source),
        }
    }
    pub(crate) fn invalid(phase: StoragePhase, message: &'static str) -> Self {
        Self::new(
            phase,
            StorageErrorKind::InvalidState,
            std::io::Error::other(message),
        )
    }
    pub(crate) fn codec(phase: StoragePhase, source: selene_core::logical::CodecError) -> Self {
        let kind = codec_kind(phase, source);
        Self::new(phase, kind, source)
    }
    pub(crate) fn persist(phase: StoragePhase, source: PersistError) -> Self {
        if let PersistError::Artifact { name, source } = source {
            return Self::persist(phase, *source).at(&name, None, None);
        }
        use StorageErrorKind as K;
        let kind = match &source {
            PersistError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => K::MissingArtifact,
            PersistError::Io(_) => K::Io,
            PersistError::WriterLockHeld => K::Contention,
            PersistError::UnsupportedVersion { .. } => K::UnsupportedFormat,
            PersistError::Directory(DirectoryError::UnsupportedPlatform) => K::UnsupportedPlatform,
            PersistError::Directory(
                DirectoryError::NotRegular(_) | DirectoryError::InvalidName(_),
            ) => K::InvalidArtifact,
            PersistError::Control(
                ControlError::AlreadyInitialized | ControlError::UnpublishedArtifacts,
            ) => K::AlreadyInitialized,
            PersistError::Control(ControlError::NotInitialized) => K::NotInitialized,
            PersistError::Control(
                ControlError::UnsupportedVersion | ControlError::MixedArtifacts(_),
            ) => K::UnsupportedFormat,
            PersistError::Control(ControlError::Compatibility) => K::Compatibility,
            PersistError::Control(ControlError::Checksum) => K::Integrity,
            PersistError::Control(ControlError::Lineage | ControlError::Stale) => K::Lineage,
            PersistError::Control(ControlError::Envelope(_)) => K::Corruption,
            PersistError::Control(ControlError::TooLarge | ControlError::GenerationExhausted) => {
                K::ResourceLimit
            }
            PersistError::Control(ControlError::PublicationUncertain { .. }) => {
                K::CheckpointUncertain
            }
            PersistError::Control(ControlError::RequiresReopen) => K::Fenced,
            _ => K::InvalidState,
        };
        Self::new(phase, kind, source)
    }
    pub(crate) fn stream(phase: StoragePhase, source: StreamError) -> Self {
        match source {
            StreamError::Artifact {
                name,
                offset,
                expected_sequence,
                source,
            } => Self::stream(phase, *source).at(&name, offset, expected_sequence),
            StreamError::Persist(error) => Self::persist(phase, error),
            StreamError::Preparation(error) => {
                if let Some(frame) =
                    error.downcast_ref::<selene_persist::logical_frame::FrameError>()
                {
                    return Self::frame(phase, *frame);
                }
                let kind = error
                    .downcast_ref::<selene_core::logical::CodecError>()
                    .map_or(StorageErrorKind::InvalidState, |e| codec_kind(phase, *e));
                let mut result = Self::invalid(phase, "preparation failed");
                result.kind = kind;
                result.source = error;
                result
            }
            StreamError::Frame(error) => Self::frame(phase, error),
            other => {
                let kind = match &other {
                    StreamError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        StorageErrorKind::MissingArtifact
                    }
                    StreamError::Io(_) => StorageErrorKind::Io,
                    StreamError::MissingSnapshot => StorageErrorKind::NotInitialized,
                    StreamError::IncompleteTail => StorageErrorKind::IncompleteTail,
                    _ => StorageErrorKind::InvalidState,
                };
                Self::new(phase, kind, other)
            }
        }
    }

    pub(crate) fn at(mut self, name: &str, offset: Option<u64>, expected: Option<u64>) -> Self {
        // Accept only a basename as diagnostic text, never grant it path authority.
        // Escape controls and cap even caller-supplied names; do not include locators.
        let base = std::path::Path::new(name)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        self.artifact = Some(
            base.chars()
                .take(128)
                .flat_map(char::escape_default)
                .take(256)
                .collect(),
        );
        self.offset = offset.or(self.offset);
        self.expected_sequence = expected.or(self.expected_sequence);
        self
    }

    fn frame(phase: StoragePhase, error: selene_persist::logical_frame::FrameError) -> Self {
        use StorageErrorKind as K;
        use selene_persist::logical_frame::FrameError as F;
        let kind = match error {
            F::Limit => K::ResourceLimit,
            F::Unsupported(_) => K::UnsupportedFormat,
            F::Integrity(_) => K::Integrity,
            F::Store => K::ForeignStore,
            F::Epoch => K::ForeignEpoch,
            F::Segment => K::ForeignSegment,
            F::Origin => K::DigestLineage,
            F::Sequence { expected, observed } if observed > expected => K::SequenceGap,
            F::Sequence { .. } => K::SequenceOverlap,
            F::SnapshotBoundary | F::Context => K::Lineage,
            F::CorruptIncomplete => K::IncompleteRequired,
            F::Payload(e) => codec_kind(phase, e),
            F::Invalid(_) | F::Compression => K::Corruption,
        };
        let mut result = Self::new(phase, kind, error);
        if let F::Sequence { expected, observed } = error {
            result.expected_sequence = Some(expected);
            result.observed_sequence = Some(observed);
        }
        result
    }
}
fn codec_kind(phase: StoragePhase, error: selene_core::logical::CodecError) -> StorageErrorKind {
    use StorageErrorKind as K;
    use selene_core::logical::CodecError as E;
    match error {
        E::Limit => K::ResourceLimit,
        E::Unsupported(_) => K::UnsupportedFormat,
        E::Incomplete => K::IncompleteRequired,
        E::Invalid(_) => K::Corruption,
        E::Semantic | E::Admission(_) if phase == StoragePhase::Rebuild => K::NativeAdmission,
        E::Semantic | E::Admission(_) => K::Semantic,
    }
}
impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {:?}", self.phase, self.kind)?;
        if let Some(name) = &self.artifact {
            write!(
                f,
                " artifact={name:?} offset={:?} expected_sequence={:?}",
                self.offset, self.expected_sequence
            )?;
        }
        Ok(())
    }
}
impl fmt::Debug for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The original source is explicitly available via Error::source, but its
        // caller-supplied strings/locators must not leak through routine diagnostics.
        f.debug_struct("StorageError")
            .field("phase", &self.phase)
            .field("kind", &self.kind)
            .field("artifact", &self.artifact)
            .field("offset", &self.offset)
            .field("expected_sequence", &self.expected_sequence)
            .field("observed_sequence", &self.observed_sequence)
            .finish_non_exhaustive()
    }
}
impl StdError for StorageError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.source.as_ref())
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
