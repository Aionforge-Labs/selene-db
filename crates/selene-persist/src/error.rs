//! Error types for persistence formats.

use std::path::PathBuf;

use crate::provider::RecoveryError;

/// Result alias for persistence operations.
pub type PersistResult<T> = Result<T, PersistError>;

/// A separately versioned on-disk artifact within a persistence directory.
///
/// Each carries its own magic and format version and can be rejected
/// independently, so an error that does not name one leaves an operator
/// guessing which file to recreate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PersistArtifact {
    /// The write-ahead log (`wal.log`) or one of its archives.
    Wal,
    /// A snapshot file (`snapshot.N.snap`).
    Snapshot,
    /// The `MANIFEST` epoch record.
    Manifest,
    /// The audit log (`audit.log`).
    AuditLog,
}

impl std::fmt::Display for PersistArtifact {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Wal => "wal",
            Self::Snapshot => "snapshot",
            Self::Manifest => "MANIFEST",
            Self::AuditLog => "audit log",
        };
        out.write_str(name)
    }
}

/// Error type for WAL persistence operations.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum PersistError {
    /// Selected artifact context; the name is diagnostic, never path authority.
    #[error("selected artifact {name:?}: {source}")]
    Artifact {
        /// Managed artifact basename supplied by the owning selection code.
        name: String,
        /// Original typed cause, including native I/O errors.
        source: Box<PersistError>,
    },
    /// Retained directory authority or platform contract failure.
    #[error(transparent)]
    Directory(#[from] crate::DirectoryError),
    /// Empty-store control identity, encoding, or publication failure.
    #[error(transparent)]
    Control(#[from] crate::ControlError),
    /// I/O failure on a persistence file.
    #[error("persistence io: {0}")]
    #[diagnostic(code(SLENE_P_001))]
    Io(#[from] std::io::Error),

    /// WAL entry-header encode/decode failure.
    #[error("wal header codec: {0}")]
    #[diagnostic(code(SLENE_P_002))]
    HeaderCodec(String),

    /// Postcard encode/decode failure for the payload `Vec<Change>`.
    #[error("wal payload codec: {0}")]
    #[diagnostic(code(SLENE_P_003))]
    PayloadCodec(String),

    /// Compression or decompression failure.
    #[error("wal payload compression: {0}")]
    #[diagnostic(code(SLENE_P_004))]
    Compression(String),

    /// Payload exceeds the WAL entry cap.
    #[error("wal payload too large: {len} bytes (max {max})")]
    #[diagnostic(code(SLENE_P_005))]
    PayloadTooLarge {
        /// Encoded payload length.
        len: usize,
        /// Maximum encoded payload length.
        max: usize,
    },

    /// Principal slot exceeds the D12 cap.
    #[error("wal principal too large: {len} bytes (max {max})")]
    #[diagnostic(code(SLENE_P_006))]
    PrincipalTooLarge {
        /// Principal byte length.
        len: usize,
        /// Maximum principal byte length.
        max: usize,
    },

    /// Magic bytes do not match the expected persistence format.
    #[error("persistence magic mismatch: got {observed:?}")]
    #[diagnostic(code(SLENE_P_007))]
    MagicMismatch {
        /// Observed four-byte file magic.
        observed: [u8; 4],
    },

    /// File-header version is unsupported.
    ///
    /// A store holds four independently versioned artifacts, so the message
    /// names which one is at fault. Without that, the only actionable part of
    /// "recreate the store from source" — knowing what to look at — is missing.
    #[error("{artifact} version unsupported: {major}.{minor}")]
    #[diagnostic(code(SLENE_P_008))]
    UnsupportedVersion {
        /// Artifact whose header carried the version.
        artifact: PersistArtifact,
        /// Major version read from disk.
        major: u16,
        /// Minor version read from disk.
        minor: u16,
    },

    /// Entry checksum does not match the payload.
    #[error("wal entry checksum mismatch at sequence {sequence}")]
    #[diagnostic(code(SLENE_P_009))]
    ChecksumMismatch {
        /// WAL sequence number.
        sequence: u64,
    },

    /// Sequence numbers are not strictly monotonic.
    #[error("wal entry sequence non-monotonic: previous {previous}, current {current}")]
    #[diagnostic(code(SLENE_P_010))]
    NonMonotonicSequence {
        /// Previous valid sequence number.
        previous: u64,
        /// Current sequence number.
        current: u64,
    },

    /// File header missing or short read at open.
    #[error("wal file truncated before completing the file header")]
    #[diagnostic(code(SLENE_P_011))]
    TruncatedFileHeader,

    /// Last entry was partially written.
    #[error("wal entry truncated at offset {offset}; iterator stopped")]
    #[diagnostic(code(SLENE_P_012))]
    TruncatedEntry {
        /// File offset where the truncated entry started.
        offset: u64,
    },

    /// Another writer holds the exclusive WAL lock.
    #[error("wal writer lock is held by another process or handle")]
    #[diagnostic(code(SLENE_P_013))]
    WriterLockHeld,

    /// Snapshot header missing or short read at open.
    #[error("snapshot file truncated before completing the file header")]
    #[diagnostic(code(SLENE_P_014))]
    TruncatedSnapshotHeader,

    /// Snapshot section table missing or short read.
    #[error("snapshot section table truncated at offset {offset}")]
    #[diagnostic(code(SLENE_P_015))]
    TruncatedSectionTable {
        /// File offset where the truncated table row started.
        offset: u64,
    },

    /// Snapshot contains more sections than the u16 section-count field can hold.
    #[error("snapshot has too many sections: {count} (max {max})")]
    #[diagnostic(code(SLENE_P_016))]
    TooManySections {
        /// Requested section count.
        count: usize,
        /// Maximum section count.
        max: usize,
    },

    /// Snapshot section payload exceeds the hard per-section cap.
    #[error("snapshot section too large: {len} bytes (max {max})")]
    #[diagnostic(code(SLENE_P_017))]
    SectionTooLarge {
        /// Encoded or decoded section length.
        len: usize,
        /// Maximum section byte length.
        max: usize,
    },

    /// Snapshot builder saw the same provider/sub tag twice.
    #[error("duplicate snapshot section tag: provider {provider:?}, sub {sub:?}")]
    #[diagnostic(code(SLENE_P_018))]
    DuplicateSection {
        /// Provider tag.
        provider: [u8; 4],
        /// Provider-owned sub-tag.
        sub: [u8; 4],
    },

    /// Snapshot body hash does not match the section table and payload bytes.
    #[error("snapshot body hash mismatch: expected {expected:?}, observed {observed:?}")]
    #[diagnostic(code(SLENE_P_019))]
    BodyHashMismatch {
        /// Hash stored in the snapshot header.
        expected: [u8; 16],
        /// Hash recomputed by the reader.
        observed: [u8; 16],
    },

    /// Snapshot header set a flag this implementation does not support.
    #[error("snapshot flag unsupported: {flag:#06x}")]
    #[diagnostic(code(SLENE_P_020))]
    UnsupportedFlag {
        /// Unsupported flag bit or mask.
        flag: u16,
    },

    /// Snapshot header reserved bytes were not zero.
    #[error("snapshot reserved bytes were nonzero at offset {offset}")]
    #[diagnostic(code(SLENE_P_021))]
    ReservedBytesNonZero {
        /// File offset of the first nonzero reserved byte.
        offset: u64,
    },

    /// A WAL entry's framing bytes failed their integrity check.
    ///
    /// The prefix checksum authenticates the framing fields — including the
    /// payload length and the payload's own checksum — so this fires before any
    /// of them is used as a length or a file offset. It is never a torn write:
    /// the writer emits the prefix and its checksum in one buffer.
    #[error("wal entry header checksum mismatch at offset {offset}")]
    #[diagnostic(code(SLENE_P_044))]
    WalHeaderChecksumMismatch {
        /// File offset where the failing entry started.
        offset: u64,
    },

    /// The 24-byte WAL file header failed its integrity check.
    #[error("wal file header checksum mismatch")]
    #[diagnostic(code(SLENE_P_045))]
    WalFileHeaderChecksumMismatch,

    /// A WAL frame before the tail failed validation.
    ///
    /// Distinguished from a torn tail: the failing frame is followed by more
    /// file content, or its bytes are not the zero fill a short write leaves.
    /// Truncating here would discard committed frames, so recovery refuses.
    ///
    /// The wrapper exists so the outcome is legible without inspecting the
    /// file. The underlying `source` says what failed; only this variant says
    /// the log was left intact, which is the distinction that was missing when
    /// corruption anywhere was silently repaired as a torn tail.
    #[error(
        "wal corruption at offset {offset} is not a torn tail \
         ({trailing_bytes} bytes follow it); the log was left intact"
    )]
    #[diagnostic(code(SLENE_P_046))]
    WalMidLogCorruption {
        /// File offset where the failing entry started.
        offset: u64,
        /// Bytes between the failing frame's start and end of file.
        trailing_bytes: u64,
        /// The validation failure that triggered the refusal.
        source: Box<PersistError>,
    },

    /// An audit record failed validation with records after it.
    ///
    /// The audit twin of [`Self::WalMidLogCorruption`], and it exists for the
    /// same reason: a record that fails validation is only a torn tail if
    /// nothing follows it. Truncating at the first bad record discarded every
    /// acknowledged record after it, on every recovery, and returned `Ok`.
    #[error(
        "audit corruption at offset {offset} is not a torn tail \
         ({trailing_bytes} bytes follow it); the log was left intact"
    )]
    #[diagnostic(code(SLENE_P_047))]
    AuditMidLogCorruption {
        /// File offset where the failing record started.
        offset: u64,
        /// Bytes between the failing record's start and end of file.
        trailing_bytes: u64,
        /// The validation failure that triggered the refusal.
        source: Box<PersistError>,
    },

    /// An audit record header failed its own integrity check.
    ///
    /// Raised before `payload_len` is used as a length or an offset, so a
    /// flipped length cannot steer the scan to a bogus record boundary.
    #[error("audit record header checksum mismatch at offset {offset}")]
    #[diagnostic(code(SLENE_P_048))]
    AuditHeaderChecksumMismatch {
        /// File offset where the failing record started.
        offset: u64,
    },

    /// An audit record's payload does not match its checksum.
    ///
    /// Distinct from [`Self::ChecksumMismatch`], which names a WAL sequence;
    /// audit records have no sequence, so this names the file offset instead.
    #[error("audit record payload checksum mismatch at offset {offset}")]
    #[diagnostic(code(SLENE_P_049))]
    AuditRecordChecksumMismatch {
        /// File offset where the failing record started.
        offset: u64,
    },

    /// Snapshot filename could not be parsed.
    #[error("malformed snapshot filename")]
    #[diagnostic(code(SLENE_P_022))]
    MalformedSnapshotFilename,

    /// Requested snapshot section tag is not present.
    #[error("snapshot section missing: provider {provider:?}, sub {sub:?}")]
    #[diagnostic(code(SLENE_P_023))]
    SectionMissing {
        /// Provider tag.
        provider: [u8; 4],
        /// Provider-owned sub-tag.
        sub: [u8; 4],
    },

    /// Snapshot section table claims a payload range that overlaps the
    /// header/table region, runs past end-of-file, or overflows `u64`.
    #[error("malformed snapshot section layout: {reason}")]
    #[diagnostic(code(SLENE_P_024))]
    MalformedSectionLayout {
        /// Human-readable reason for the layout rejection.
        reason: &'static str,
    },

    /// Recovery provider registry saw the same provider tag twice.
    #[error("duplicate recovery provider tag: {tag:?}")]
    #[diagnostic(code(SLENE_P_025))]
    DuplicateProviderTag {
        /// Duplicate provider tag.
        tag: [u8; 4],
    },

    /// Snapshot section table referenced an unregistered provider.
    #[error("unknown recovery provider: provider {provider:?}, sub {sub:?}")]
    #[diagnostic(code(SLENE_P_026))]
    UnknownProvider {
        /// Provider tag from the snapshot section row.
        provider: [u8; 4],
        /// Provider-owned sub-tag from the snapshot section row.
        sub: [u8; 4],
    },

    /// Recovery provider returned an error while applying bytes or changes.
    #[error("recovery provider {provider:?} failed for sub {sub:?}: {source}")]
    #[diagnostic(code(SLENE_P_027))]
    ProviderFailed {
        /// Provider tag whose callback failed.
        provider: [u8; 4],
        /// Section sub-tag for snapshot failures, or `None` for WAL changes.
        sub: Option<[u8; 4]>,
        /// Provider-owned source error.
        #[source]
        source: RecoveryError,
    },

    /// WAL file extends a different snapshot epoch than the snapshot applied.
    #[error(
        "wal snapshot sequence {wal_snapshot_seq} does not match applied snapshot {snapshot_seq}"
    )]
    #[diagnostic(code(SLENE_P_028))]
    WalSnapshotMismatch {
        /// Snapshot sequence recorded in the WAL header.
        wal_snapshot_seq: u64,
        /// Snapshot sequence selected by recovery.
        snapshot_seq: u64,
    },

    /// A same-sequence persistence artifact differs from the bytes this
    /// rotation intended to publish.
    #[error("existing persistence artifact does not match the intended bytes: {path}")]
    #[diagnostic(code(SLENE_P_029))]
    ArtifactIdentityMismatch {
        /// Existing snapshot or WAL archive path that failed identity checking.
        path: PathBuf,
    },

    /// WAL rotation committed the new epoch but could not finish replacing the active WAL.
    #[error("wal rotation incomplete: archive {archived_path:?}, active {new_path}")]
    #[diagnostic(code(SLENE_P_030))]
    WalRotationIncomplete {
        /// Path containing the archived pre-rotation WAL, or `None` when
        /// retention had already removed that archive before reset retry.
        archived_path: Option<PathBuf>,
        /// Active WAL path whose replacement did not complete.
        new_path: PathBuf,
    },

    /// WAL rotation was asked to rotate at a snapshot sequence that does not
    /// match the writer's current high-water mark.
    #[error(
        "wal rotation snapshot sequence {snapshot_seq} does not match current sequence {last_sequence}"
    )]
    #[diagnostic(code(SLENE_P_031))]
    WalRotationSequenceMismatch {
        /// Sequence covered by the caller's finalized snapshot.
        snapshot_seq: u64,
        /// Current WAL high-water mark.
        last_sequence: u64,
    },

    /// A MANIFEST or live writer named an active WAL file other than the one
    /// this build recovers from. The active-WAL name is a de-facto fixed
    /// contract (always [`crate::DEFAULT_WAL_FILE_NAME`]).
    #[error("unexpected active wal {observed:?}, expected {expected:?}")]
    #[diagnostic(code(SLENE_P_032))]
    UnexpectedActiveWal {
        /// Active WAL name read from the MANIFEST or writer path.
        observed: String,
        /// Active WAL name this build recovers from.
        expected: &'static str,
    },

    /// WAL rotation requires a positive durable high-water sequence.
    #[error("wal rotation requires a nonzero snapshot sequence")]
    #[diagnostic(code(SLENE_P_033))]
    WalRotationZeroSequence,

    /// Snapshot builder and active WAL target different directories.
    #[error(
        "wal rotation snapshot directory {snapshot_dir} does not match active wal directory {wal_dir}"
    )]
    #[diagnostic(code(SLENE_P_034))]
    WalRotationDirectoryMismatch {
        /// Directory configured on the snapshot builder.
        snapshot_dir: PathBuf,
        /// Stable parent directory of the active WAL.
        wal_dir: PathBuf,
    },

    /// Persistence-state divergence or a committed-but-incomplete rotation
    /// made this writer unsafe to reuse.
    #[error("wal writer is poisoned by persistence-state divergence; reopen it before use")]
    #[diagnostic(code(SLENE_P_035))]
    WalWriterPoisoned,

    /// The committed MANIFEST is newer than the requested rotation sequence.
    #[error(
        "manifest snapshot sequence {manifest_sequence} is ahead of requested rotation {snapshot_sequence}"
    )]
    #[diagnostic(code(SLENE_P_036))]
    WalRotationManifestAhead {
        /// Snapshot sequence named by the committed MANIFEST.
        manifest_sequence: u64,
        /// Snapshot sequence requested by the caller.
        snapshot_sequence: u64,
    },

    /// A MANIFEST-committed live snapshot is missing or not a regular file.
    #[error("committed snapshot is unavailable as a regular file: {path}")]
    #[diagnostic(code(SLENE_P_037))]
    CommittedSnapshotUnavailable {
        /// Snapshot path named by the committed epoch.
        path: PathBuf,
    },

    /// A MANIFEST-committed snapshot differs from the state supplied for the
    /// same checkpoint epoch.
    #[error("committed snapshot does not match the requested checkpoint state: {path}")]
    #[diagnostic(code(SLENE_P_038))]
    CommittedSnapshotIdentityMismatch {
        /// Committed snapshot path that differs from the requested bytes.
        path: PathBuf,
    },

    /// A WAL archive retained by the committed MANIFEST is unavailable or
    /// fails structural validation.
    #[error("committed WAL archive is unavailable or invalid: {path}")]
    #[diagnostic(code(SLENE_P_039))]
    CommittedArchiveInvalid {
        /// Retained archive path that could not be validated.
        path: PathBuf,
    },

    /// The active WAL path names a symlink or another non-regular entry.
    #[error("active WAL path is not a regular file: {path}")]
    #[diagnostic(code(SLENE_P_040))]
    WalPathNotRegular {
        /// Anchored active WAL path that failed the regular-file check.
        path: PathBuf,
    },

    /// A checkpoint snapshot did not target the marker sequence immediately
    /// after the writer's current high-water mark.
    #[error(
        "checkpoint snapshot sequence {snapshot_seq} does not match expected watermark sequence {expected_sequence}"
    )]
    #[diagnostic(code(SLENE_P_041))]
    WalCheckpointSequenceMismatch {
        /// Sequence configured on the checkpoint snapshot builder.
        snapshot_seq: u64,
        /// Exact next sequence the checkpoint marker would receive.
        expected_sequence: u64,
    },

    /// The WAL sequence counter cannot advance beyond `u64::MAX`.
    #[error("wal sequence is exhausted at {last_sequence}")]
    #[diagnostic(code(SLENE_P_042))]
    WalSequenceExhausted {
        /// Current high-water sequence that cannot be incremented.
        last_sequence: u64,
    },

    /// A writer takeover's WAL tip disagrees with the sequence reached by its
    /// guarded recovery pass.
    #[error(
        "wal writer sequence {writer_sequence} does not match recovered sequence {recovered_sequence}"
    )]
    #[diagnostic(code(SLENE_P_043))]
    WalRecoverySequenceMismatch {
        /// High-water sequence retained by the reopened writer.
        writer_sequence: u64,
        /// High-water sequence observed by guarded recovery.
        recovered_sequence: u64,
    },
}

impl PersistError {
    /// Map this error to its 5-character ISO GQLSTATUS code.
    #[must_use]
    pub const fn gqlstatus(&self) -> &'static str {
        match self {
            Self::Artifact { source, .. } => source.gqlstatus(),
            Self::PrincipalTooLarge { .. } => "22G03",
            Self::PayloadTooLarge { .. }
            | Self::TooManySections { .. }
            | Self::SectionTooLarge { .. } => "5GQL1",
            Self::DuplicateSection { .. } | Self::DuplicateProviderTag { .. } => "22G03",
            Self::UnsupportedVersion { .. } => "08000",
            Self::Control(_)
            | Self::Directory(_)
            | Self::Io(_)
            | Self::HeaderCodec(_)
            | Self::PayloadCodec(_)
            | Self::Compression(_)
            | Self::MagicMismatch { .. }
            | Self::ChecksumMismatch { .. }
            | Self::NonMonotonicSequence { .. }
            | Self::TruncatedFileHeader
            | Self::TruncatedEntry { .. }
            | Self::WriterLockHeld
            | Self::TruncatedSnapshotHeader
            | Self::TruncatedSectionTable { .. }
            | Self::BodyHashMismatch { .. }
            | Self::UnsupportedFlag { .. }
            | Self::ReservedBytesNonZero { .. }
            | Self::WalHeaderChecksumMismatch { .. }
            | Self::WalFileHeaderChecksumMismatch
            | Self::WalMidLogCorruption { .. }
            | Self::AuditMidLogCorruption { .. }
            | Self::AuditHeaderChecksumMismatch { .. }
            | Self::AuditRecordChecksumMismatch { .. }
            | Self::MalformedSnapshotFilename
            | Self::SectionMissing { .. }
            | Self::MalformedSectionLayout { .. }
            | Self::UnknownProvider { .. }
            | Self::ProviderFailed { .. }
            | Self::WalSnapshotMismatch { .. }
            | Self::ArtifactIdentityMismatch { .. }
            | Self::WalRotationIncomplete { .. }
            | Self::WalRotationSequenceMismatch { .. }
            | Self::UnexpectedActiveWal { .. }
            | Self::WalRotationZeroSequence
            | Self::WalRotationDirectoryMismatch { .. }
            | Self::WalWriterPoisoned
            | Self::WalRotationManifestAhead { .. }
            | Self::CommittedSnapshotUnavailable { .. }
            | Self::CommittedSnapshotIdentityMismatch { .. }
            | Self::CommittedArchiveInvalid { .. }
            | Self::WalPathNotRegular { .. }
            | Self::WalCheckpointSequenceMismatch { .. }
            | Self::WalSequenceExhausted { .. }
            | Self::WalRecoverySequenceMismatch { .. } => "5GQL0",
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(PersistError::HeaderCodec("bad".to_owned()), "5GQL0")]
    #[case(PersistError::PayloadCodec("bad".to_owned()), "5GQL0")]
    #[case(PersistError::Compression("bad".to_owned()), "5GQL0")]
    #[case(PersistError::PayloadTooLarge { len: 1, max: 0 }, "5GQL1")]
    #[case(PersistError::PrincipalTooLarge { len: 1, max: 0 }, "22G03")]
    #[case(PersistError::MagicMismatch { observed: *b"NOPE" }, "5GQL0")]
    #[case(PersistError::UnsupportedVersion { artifact: PersistArtifact::Wal, major: 2, minor: 0 }, "08000")]
    #[case(PersistError::AuditMidLogCorruption { offset: 8, trailing_bytes: 40, source: Box::new(PersistError::TruncatedFileHeader) }, "5GQL0")]
    #[case(PersistError::AuditHeaderChecksumMismatch { offset: 8 }, "5GQL0")]
    #[case(PersistError::AuditRecordChecksumMismatch { offset: 8 }, "5GQL0")]
    #[case(PersistError::ChecksumMismatch { sequence: 7 }, "5GQL0")]
    #[case(PersistError::NonMonotonicSequence { previous: 7, current: 7 }, "5GQL0")]
    #[case(PersistError::TruncatedFileHeader, "5GQL0")]
    #[case(PersistError::TruncatedEntry { offset: 16 }, "5GQL0")]
    #[case(PersistError::WriterLockHeld, "5GQL0")]
    #[case(PersistError::TruncatedSnapshotHeader, "5GQL0")]
    #[case(PersistError::TruncatedSectionTable { offset: 32 }, "5GQL0")]
    #[case(PersistError::TooManySections { count: 2, max: 1 }, "5GQL1")]
    #[case(PersistError::SectionTooLarge { len: 2, max: 1 }, "5GQL1")]
    #[case(PersistError::DuplicateSection { provider: *b"CORE", sub: *b"META" }, "22G03")]
    #[case(PersistError::BodyHashMismatch { expected: [1; 16], observed: [2; 16] }, "5GQL0")]
    #[case(PersistError::UnsupportedFlag { flag: 1 }, "5GQL0")]
    #[case(PersistError::ReservedBytesNonZero { offset: 12 }, "5GQL0")]
    #[case(PersistError::MalformedSnapshotFilename, "5GQL0")]
    #[case(PersistError::SectionMissing { provider: *b"CORE", sub: *b"META" }, "5GQL0")]
    #[case(PersistError::MalformedSectionLayout { reason: "test" }, "5GQL0")]
    #[case(PersistError::DuplicateProviderTag { tag: *b"CORE" }, "22G03")]
    #[case(PersistError::UnknownProvider { provider: *b"CORE", sub: *b"META" }, "5GQL0")]
    #[case(PersistError::ProviderFailed {
        provider: *b"CORE",
        sub: Some(*b"META"),
        source: Box::new(std::io::Error::other("provider failed")),
    }, "5GQL0")]
    #[case(PersistError::WalSnapshotMismatch { wal_snapshot_seq: 2, snapshot_seq: 1 }, "5GQL0")]
    #[case(PersistError::ArtifactIdentityMismatch { path: "wal.1.archive".into() }, "5GQL0")]
    #[case(PersistError::WalRotationIncomplete {
        archived_path: Some("wal.1.archive".into()),
        new_path: "wal.log".into(),
    }, "5GQL0")]
    #[case(PersistError::WalRotationSequenceMismatch {
        snapshot_seq: 1,
        last_sequence: 2,
    }, "5GQL0")]
    #[case(PersistError::UnexpectedActiveWal {
        observed: "custom.log".to_owned(),
        expected: crate::DEFAULT_WAL_FILE_NAME,
    }, "5GQL0")]
    #[case(PersistError::WalRotationZeroSequence, "5GQL0")]
    #[case(PersistError::WalRotationDirectoryMismatch {
        snapshot_dir: "snapshots".into(),
        wal_dir: "wal".into(),
    }, "5GQL0")]
    #[case(PersistError::WalWriterPoisoned, "5GQL0")]
    #[case(PersistError::WalRotationManifestAhead {
        manifest_sequence: 3,
        snapshot_sequence: 2,
    }, "5GQL0")]
    #[case(PersistError::CommittedSnapshotUnavailable {
        path: "snapshot.3.snap".into(),
    }, "5GQL0")]
    #[case(PersistError::CommittedSnapshotIdentityMismatch {
        path: "snapshot.3.snap".into(),
    }, "5GQL0")]
    #[case(PersistError::CommittedArchiveInvalid {
        path: "wal.3.archive".into(),
    }, "5GQL0")]
    #[case(PersistError::WalPathNotRegular {
        path: "wal.log".into(),
    }, "5GQL0")]
    #[case(PersistError::WalCheckpointSequenceMismatch {
        snapshot_seq: 4,
        expected_sequence: 5,
    }, "5GQL0")]
    #[case(PersistError::WalSequenceExhausted {
        last_sequence: u64::MAX,
    }, "5GQL0")]
    #[case(PersistError::WalRecoverySequenceMismatch {
        writer_sequence: 4,
        recovered_sequence: 5,
    }, "5GQL0")]
    fn gqlstatus_for_each_variant(#[case] error: PersistError, #[case] status: &str) {
        assert_eq!(error.gqlstatus(), status);
        assert!(
            selene_core::gqlstatus_name(status).is_some(),
            "GQLSTATUS code {status} emitted by PersistError but not in ALL_GQLSTATUS_NAMES"
        );
    }
}
