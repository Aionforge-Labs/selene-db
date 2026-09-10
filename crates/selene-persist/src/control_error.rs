//! Typed failures of the empty-store control protocol.

/// A control envelope, lineage, compatibility, or publication failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ControlError {
    /// No complete CURRENT selector has been published. Orphans are not selected.
    #[error("store control is not initialized")]
    NotInitialized,
    /// Exclusive creation would replace an existing control state.
    #[error("store control already exists")]
    AlreadyInitialized,
    /// Existing unselected control files cannot be adopted as a fresh store.
    #[error("unpublished control artifacts require explicit offline reconciliation")]
    UnpublishedArtifacts,
    /// Empty-store control cannot coexist with data or unknown directory entries.
    #[error("empty-store control encountered an unsupported artifact: {0}")]
    MixedArtifacts(std::path::PathBuf),
    /// An envelope exceeded the fixed allocation/encoding limit.
    #[error("control record exceeds the bounded envelope limit")]
    TooLarge,
    /// Truncation, invalid serialization, magic, or trailing bytes.
    #[error("invalid control envelope: {0}")]
    Envelope(&'static str),
    /// Only the explicitly supported control encoding and empty format are accepted.
    #[error("unsupported control encoding or storage format version")]
    UnsupportedVersion,
    /// The envelope or selected manifest bytes do not match their digest.
    #[error("control checksum mismatch")]
    Checksum,
    /// IDs, epochs, generations, selector names, or structural parent provenance disagree.
    #[error("control identity or lineage mismatch")]
    Lineage,
    /// The caller's expected profile, Unicode, or collation identity differs.
    #[error("store compatibility identity mismatch")]
    Compatibility,
    /// A handle's prior selector is no longer current.
    #[error("stale control publication handle")]
    Stale,
    /// Manifest generation cannot advance without overflowing.
    #[error("control manifest generation exhausted")]
    GenerationExhausted,
    /// Publication may have taken effect; no further publication is allowed on this handle.
    #[error("control publication is uncertain; reopen before further use: {source}")]
    PublicationUncertain {
        /// The failure after CURRENT became externally observable.
        source: Box<crate::PersistError>,
    },
    /// A prior ambiguous publication fenced this handle.
    #[error("control handle requires reopen after uncertain publication")]
    RequiresReopen,
}
