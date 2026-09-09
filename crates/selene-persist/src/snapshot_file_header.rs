//! Fixed snapshot file header.

use std::io::{Read, Write};

use crate::section::MAX_SECTION_COUNT;
use crate::{PersistArtifact, PersistError, PersistResult};

/// Snapshot file magic.
pub const SNAPSHOT_MAGIC: [u8; 4] = *b"SLSN";
/// Snapshot major format version.
pub const SNAPSHOT_VERSION_MAJOR: u16 = 1;
/// Snapshot minor format version.
///
/// Bumped `0 -> 1` by BRIEF-Item-4a STEP 9: the `CORE/NODE` / `CORE/EDGE`
/// sections now persist the explicit external `NodeId` / `EdgeId` per row
/// instead of synthesizing `row + 1`, and recovery places rows positionally so
/// a future 4b-compacted snapshot (ids != row+1) round-trips. The version gate
/// (`SnapshotFileHeader::read_from`) rejects any mismatch, so pre-STEP-9
/// (minor 0) snapshots are cleanly rejected with [`crate::PersistError::UnsupportedVersion`]
/// — a clean break, not a dual decoder (deferred to 4c per the D14 amendment).
///
/// Bumped `1 -> 2` when the serialized `Value` enum layout changed. Snapshot
/// rows embed property values as postcard `PropertyMap` blobs inside the rkyv
/// `CORE/NODE` / `CORE/EDGE` sections. Older snapshots are rejected by the same
/// exact-match gate rather than mis-decoding shifted variant discriminants —
/// another clean greenfield break.
///
/// Bumped `2 -> 3` by first-class IVF config: `CORE/VIDX` rows now persist
/// optional IVF construction parameters beside HNSW construction parameters.
/// The exact-match gate rejects older vector-index schema rows rather than
/// decoding them against the wrong rkyv shape.
///
/// Bumped `3 -> 4` by the typed-descriptor stream: the rkyv `PropertyTypeDef`
/// rows archived inside `CORE/GTYP` gained mid-struct `decimal_type` /
/// `character_string_type` / `byte_string_type` descriptor fields, and the
/// `PropertyElementType` / `RecordFieldType` enums gained descriptor variants
/// ahead of existing ones. The exact-match gate rejects pre-descriptor
/// snapshots rather than decoding them against the wrong archived shape.
///
/// Bumped `4 -> 5` when `CORE/SCMA` added an entity discriminator so the same
/// section can persist both node-property and edge-property index
/// registrations. The exact-match gate rejects pre-edge-index schema rows
/// rather than decoding them against the wrong archived key shape.
/// F01-PR03 adds intrinsic directionality to edge reconstruction inputs.
/// This guards the current archive shape, not the future F02 byte format.
pub const SNAPSHOT_VERSION_MINOR: u16 = 6;
/// Fixed snapshot file-header length.
pub const SNAPSHOT_FILE_HEADER_LEN: usize = 32;
/// Whole-body compression flag, reserved in v1.0.
pub const FLAG_BODY_COMPRESSED: u16 = 0b0000_0000_0000_0001;
/// Per-section compression flag.
pub const FLAG_SECTION_COMPRESSED: u16 = 0b0000_0000_0000_0010;

const RESERVED_START_OFFSET: u64 = 12;
const SUPPORTED_FLAGS: u16 = FLAG_SECTION_COMPRESSED;

/// Fixed-size snapshot envelope header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotFileHeader {
    /// Major format version.
    pub version_major: u16,
    /// Minor format version.
    pub version_minor: u16,
    /// Snapshot flags.
    pub flags: u16,
    /// Number of section-table rows.
    pub section_count: u16,
    /// Low 128 bits of blake3 over section table plus payload bytes.
    pub body_hash: [u8; 16],
}

impl SnapshotFileHeader {
    /// Construct a validated v1.0 snapshot file header.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::TooManySections`] if `section_count` does not
    /// fit the u16 header field or [`PersistError::UnsupportedFlag`] if an
    /// unsupported flag bit is set.
    pub fn new(flags: u16, section_count: usize, body_hash: [u8; 16]) -> PersistResult<Self> {
        validate_flags(flags)?;
        if section_count > MAX_SECTION_COUNT {
            return Err(PersistError::TooManySections {
                count: section_count,
                max: MAX_SECTION_COUNT,
            });
        }
        Ok(Self {
            version_major: SNAPSHOT_VERSION_MAJOR,
            version_minor: SNAPSHOT_VERSION_MINOR,
            flags,
            section_count: section_count as u16,
            body_hash,
        })
    }

    /// Write this fixed 32-byte header.
    pub(crate) fn write_to(&self, writer: &mut impl Write) -> PersistResult<()> {
        writer.write_all(&SNAPSHOT_MAGIC)?;
        writer.write_all(&self.version_major.to_le_bytes())?;
        writer.write_all(&self.version_minor.to_le_bytes())?;
        writer.write_all(&self.flags.to_le_bytes())?;
        writer.write_all(&self.section_count.to_le_bytes())?;
        writer.write_all(&[0_u8; 4])?;
        writer.write_all(&self.body_hash)?;
        Ok(())
    }

    /// Read and validate a fixed 32-byte snapshot header from a reader.
    pub(crate) fn read_from(reader: &mut impl Read) -> PersistResult<Self> {
        let mut bytes = [0_u8; SNAPSHOT_FILE_HEADER_LEN];
        reader
            .read_exact(&mut bytes)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::UnexpectedEof => PersistError::TruncatedSnapshotHeader,
                _ => PersistError::Io(error),
            })?;
        Self::from_bytes(&bytes).map(|(header, _)| header)
    }

    /// Decode and validate the fixed 32-byte header from the front of `bytes`,
    /// returning the header and the offset just past it (always
    /// [`SNAPSHOT_FILE_HEADER_LEN`]). The pure-slice core of [`Self::read_from`].
    ///
    /// # Errors
    ///
    /// [`PersistError::TruncatedSnapshotHeader`] for a slice shorter than the
    /// 32-byte header, plus the same magic/version/flag/reserved errors as
    /// [`Self::read_from`].
    pub(crate) fn from_bytes(bytes: &[u8]) -> PersistResult<(Self, usize)> {
        let header = bytes
            .get(..SNAPSHOT_FILE_HEADER_LEN)
            .ok_or(PersistError::TruncatedSnapshotHeader)?;
        let observed = [header[0], header[1], header[2], header[3]];
        if observed != SNAPSHOT_MAGIC {
            return Err(PersistError::MagicMismatch { observed });
        }
        let version_major = u16::from_le_bytes([header[4], header[5]]);
        let version_minor = u16::from_le_bytes([header[6], header[7]]);
        if version_major != SNAPSHOT_VERSION_MAJOR || version_minor != SNAPSHOT_VERSION_MINOR {
            return Err(PersistError::UnsupportedVersion {
                artifact: PersistArtifact::Snapshot,
                major: version_major,
                minor: version_minor,
            });
        }
        let flags = u16::from_le_bytes([header[8], header[9]]);
        validate_flags(flags)?;
        for (index, byte) in header[12..16].iter().enumerate() {
            if *byte != 0 {
                return Err(PersistError::ReservedBytesNonZero {
                    offset: RESERVED_START_OFFSET + index as u64,
                });
            }
        }
        let mut body_hash = [0_u8; 16];
        body_hash.copy_from_slice(&header[16..32]);
        Ok((
            Self {
                version_major,
                version_minor,
                flags,
                section_count: u16::from_le_bytes([header[10], header[11]]),
                body_hash,
            },
            SNAPSHOT_FILE_HEADER_LEN,
        ))
    }

    /// Return true if the reserved whole-body compression flag is set.
    #[must_use]
    pub const fn is_body_compressed(&self) -> bool {
        self.flags & FLAG_BODY_COMPRESSED != 0
    }

    /// Return true if section payloads are individually compressed.
    #[must_use]
    pub const fn is_section_compressed(&self) -> bool {
        self.flags & FLAG_SECTION_COMPRESSED != 0
    }
}

fn validate_flags(flags: u16) -> PersistResult<()> {
    if flags & FLAG_BODY_COMPRESSED != 0 {
        return Err(PersistError::UnsupportedFlag {
            flag: FLAG_BODY_COMPRESSED,
        });
    }
    let unsupported = flags & !SUPPORTED_FLAGS;
    if unsupported != 0 {
        return Err(PersistError::UnsupportedFlag { flag: unsupported });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_header_round_trips() {
        let header = SnapshotFileHeader::new(FLAG_SECTION_COMPRESSED, 2, [7_u8; 16]).unwrap();
        let mut bytes = Vec::new();
        header.write_to(&mut bytes).unwrap();
        assert_eq!(bytes.len(), SNAPSHOT_FILE_HEADER_LEN);
        assert_eq!(
            SnapshotFileHeader::read_from(&mut bytes.as_slice()).unwrap(),
            header
        );
        assert!(header.is_section_compressed());
        assert!(!header.is_body_compressed());
    }

    #[test]
    fn magic_mismatch_is_reported() {
        let mut bytes = Vec::new();
        SnapshotFileHeader::new(0, 0, [0; 16])
            .unwrap()
            .write_to(&mut bytes)
            .unwrap();
        bytes[0..4].copy_from_slice(b"NOPE");
        assert!(matches!(
            SnapshotFileHeader::read_from(&mut bytes.as_slice()),
            Err(PersistError::MagicMismatch { observed }) if observed == *b"NOPE"
        ));
    }

    #[test]
    fn unsupported_version_is_reported() {
        let mut bytes = Vec::new();
        SnapshotFileHeader::new(0, 0, [0; 16])
            .unwrap()
            .write_to(&mut bytes)
            .unwrap();
        bytes[4..6].copy_from_slice(&2_u16.to_le_bytes());
        // `new()` writes the current minor; patching only the major byte leaves
        // minor at its written value.
        assert!(matches!(
            SnapshotFileHeader::read_from(&mut bytes.as_slice()),
            Err(PersistError::UnsupportedVersion {
                artifact: PersistArtifact::Snapshot,
                major: 2,
                minor: 6
            })
        ));
    }

    #[test]
    fn pre_step9_minor_zero_is_rejected() {
        // BRIEF-Item-4a STEP 9 clean break: a snapshot written at the previous
        // minor version (0) must fail the gate, since the CORE/NODE & CORE/EDGE
        // section semantics changed. Patch a freshly written (minor 1) header's
        // minor bytes [6..8] back to 0 and confirm a clean UnsupportedVersion
        // rather than a silent mis-decode of the body.
        let mut bytes = Vec::new();
        SnapshotFileHeader::new(0, 0, [0; 16])
            .unwrap()
            .write_to(&mut bytes)
            .unwrap();
        bytes[6..8].copy_from_slice(&0_u16.to_le_bytes());
        assert!(matches!(
            SnapshotFileHeader::read_from(&mut bytes.as_slice()),
            Err(PersistError::UnsupportedVersion {
                artifact: PersistArtifact::Snapshot,
                major: 1,
                minor: 0
            })
        ));
    }

    #[test]
    fn pre_descriptor_minor_three_is_rejected() {
        // Typed-descriptor clean break: a snapshot written at the previous minor
        // version (3) carries `CORE/GTYP` `PropertyTypeDef` archives WITHOUT the
        // descriptor fields, so it must fail the exact-match gate with a clean
        // UnsupportedVersion rather than mis-decoding the shifted rkyv layout.
        let mut bytes = Vec::new();
        SnapshotFileHeader::new(0, 0, [0; 16])
            .unwrap()
            .write_to(&mut bytes)
            .unwrap();
        bytes[6..8].copy_from_slice(&3_u16.to_le_bytes());
        assert!(matches!(
            SnapshotFileHeader::read_from(&mut bytes.as_slice()),
            Err(PersistError::UnsupportedVersion {
                artifact: PersistArtifact::Snapshot,
                major: 1,
                minor: 3
            })
        ));
    }

    #[test]
    fn reserved_bytes_nonzero_is_reported() {
        let mut bytes = Vec::new();
        SnapshotFileHeader::new(0, 0, [0; 16])
            .unwrap()
            .write_to(&mut bytes)
            .unwrap();
        bytes[13] = 1;
        assert!(matches!(
            SnapshotFileHeader::read_from(&mut bytes.as_slice()),
            Err(PersistError::ReservedBytesNonZero { offset: 13 })
        ));
    }

    #[test]
    fn truncated_header_is_reported() {
        let bytes = [0_u8; SNAPSHOT_FILE_HEADER_LEN - 1];
        assert!(matches!(
            SnapshotFileHeader::read_from(&mut bytes.as_slice()),
            Err(PersistError::TruncatedSnapshotHeader)
        ));
    }

    #[test]
    fn body_compressed_flag_is_rejected() {
        assert!(matches!(
            SnapshotFileHeader::new(FLAG_BODY_COMPRESSED, 0, [0; 16]),
            Err(PersistError::UnsupportedFlag {
                flag: FLAG_BODY_COMPRESSED
            })
        ));
    }

    #[test]
    fn too_many_sections_is_rejected() {
        assert!(matches!(
            SnapshotFileHeader::new(0, MAX_SECTION_COUNT + 1, [0; 16]),
            Err(PersistError::TooManySections {
                count,
                max: MAX_SECTION_COUNT
            }) if count == MAX_SECTION_COUNT + 1
        ));
    }
}
