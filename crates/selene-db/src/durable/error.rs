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
    /// Integrity, lineage, semantic inventory or runtime admission failed.
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
#[derive(Debug)]
pub struct StorageError {
    /// Last attempted lifecycle phase.
    pub phase: StoragePhase,
    /// Actionable failure category.
    pub kind: StorageErrorKind,
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
        let kind = if source == selene_core::logical::CodecError::Limit {
            StorageErrorKind::ResourceLimit
        } else {
            StorageErrorKind::InvalidState
        };
        Self::new(phase, kind, source)
    }
    pub(crate) fn persist(phase: StoragePhase, source: PersistError) -> Self {
        use StorageErrorKind as K;
        let kind = match &source {
            PersistError::Io(_) => K::Io,
            PersistError::WriterLockHeld => K::Contention,
            PersistError::Directory(DirectoryError::UnsupportedPlatform) => K::UnsupportedPlatform,
            PersistError::Control(
                ControlError::AlreadyInitialized | ControlError::UnpublishedArtifacts,
            ) => K::AlreadyInitialized,
            PersistError::Control(ControlError::NotInitialized) => K::NotInitialized,
            PersistError::Control(
                ControlError::UnsupportedVersion | ControlError::MixedArtifacts(_),
            ) => K::UnsupportedFormat,
            PersistError::Control(ControlError::Compatibility) => K::Compatibility,
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
            StreamError::Persist(error) => Self::persist(phase, error),
            StreamError::Preparation(error) => {
                let kind = if error.downcast_ref::<selene_core::logical::CodecError>()
                    == Some(&selene_core::logical::CodecError::Limit)
                {
                    StorageErrorKind::ResourceLimit
                } else {
                    StorageErrorKind::InvalidState
                };
                Self {
                    phase,
                    kind,
                    source: error,
                }
            }
            other => {
                let kind = match &other {
                    StreamError::Io(_) => StorageErrorKind::Io,
                    StreamError::Frame(selene_persist::logical_frame::FrameError::Limit) => {
                        StorageErrorKind::ResourceLimit
                    }
                    StreamError::Protocol(
                        "store is not initialized" | "missing initial database snapshot",
                    ) => StorageErrorKind::NotInitialized,
                    _ => StorageErrorKind::InvalidState,
                };
                Self::new(phase, kind, other)
            }
        }
    }
}
impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {:?}: {}", self.phase, self.kind, self.source)
    }
}
impl StdError for StorageError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.source.as_ref())
    }
}
