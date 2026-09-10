//! Snapshot envelope reader.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use crate::compression::decompress_zstd_bounded;
use crate::section::{
    MAX_SECTION_PAYLOAD_BYTES, SECTION_TABLE_ROW_LEN, SectionEntry, read_section_table,
    read_section_table_from_bytes, section_table_bytes,
};
use crate::snapshot_file_header::SNAPSHOT_FILE_HEADER_LEN;
use crate::{PersistError, PersistResult, SnapshotFileHeader};

const BODY_HASH_READ_BUFFER_BYTES: usize = 64 * 1024;

/// Reader for one snapshot file.
pub struct SnapshotReader {
    file: File,
    header: SnapshotFileHeader,
    sections: Arc<[SectionEntry]>,
}

impl SnapshotReader {
    /// Open and validate the snapshot header and section table.
    ///
    /// Body-hash verification is explicit through [`Self::verify_body_hash`].
    ///
    /// # Errors
    ///
    /// Returns I/O, header, section-table, or cap validation errors.
    pub fn open(path: &Path) -> PersistResult<Self> {
        let (dir, name) = crate::StoreDirectory::for_file(path)?;
        Self::open_in(&dir, &name)
    }

    /// Open a snapshot relative to a retained capability.
    ///
    /// # Errors
    /// Returns protocol, file-open, header, section-table, or layout errors.
    pub fn open_in(dir: &crate::StoreDirectory, name: &Path) -> PersistResult<Self> {
        dir.require_legacy()?;
        let mut file = dir.open_read(name)?;
        let header = SnapshotFileHeader::read_from(&mut file)?;
        let sections = read_section_table(&mut file, usize::from(header.section_count))?;
        validate_unique_tags(&sections)?;
        let file_len = file.metadata()?.len();
        validate_section_offsets(&sections, file_len)?;
        Ok(Self {
            file,
            header,
            sections: Arc::from(sections),
        })
    }

    /// Decode and validate a snapshot **envelope** — the fixed file header, the
    /// section table, and the section layout — from an in-memory byte slice,
    /// without reading any section payload.
    ///
    /// This is the pure-slice twin of the header/table/validation prefix of
    /// [`Self::open`]: it runs the identical [`SnapshotFileHeader`] decode,
    /// section-table decode, duplicate-tag check, and offset/length layout
    /// validation, using `bytes.len()` as the file length. Section *payloads*
    /// (and their downstream rkyv decode, which lives in `selene-graph` behind a
    /// `CheckBytes` bound — never in this crate) are intentionally out of scope:
    /// this validates the envelope an attacker could craft, which is the surface
    /// a decoder fuzz target drives. For valid input it returns the same header
    /// and section table [`Self::open`] would expose.
    ///
    /// # Errors
    ///
    /// Returns the same header, section-table, duplicate-tag, and layout errors
    /// as [`Self::open`] (e.g. [`PersistError::TruncatedSnapshotHeader`],
    /// [`PersistError::TruncatedSectionTable`], [`PersistError::SectionTooLarge`],
    /// [`PersistError::DuplicateSection`], [`PersistError::MalformedSectionLayout`]).
    pub fn decode_envelope(bytes: &[u8]) -> PersistResult<(SnapshotFileHeader, Vec<SectionEntry>)> {
        let (header, offset) = SnapshotFileHeader::from_bytes(bytes)?;
        let sections =
            read_section_table_from_bytes(bytes, offset, usize::from(header.section_count))?;
        validate_unique_tags(&sections)?;
        validate_section_offsets(&sections, bytes.len() as u64)?;
        Ok((header, sections))
    }

    /// Return the snapshot file header.
    #[must_use]
    pub const fn header(&self) -> &SnapshotFileHeader {
        &self.header
    }

    /// Return the decoded section table.
    #[must_use]
    pub fn sections(&self) -> &[SectionEntry] {
        &self.sections
    }

    /// Return a cheap shared handle to the decoded section table.
    ///
    /// Cloning the returned [`Arc`] is a pointer bump, so a caller that needs to
    /// iterate the sections while also re-borrowing `&mut self` for payload reads
    /// can do so without deep-copying the section Vec.
    #[must_use]
    pub fn sections_arc(&self) -> Arc<[SectionEntry]> {
        Arc::clone(&self.sections)
    }

    /// Read a section by provider/sub tag.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::SectionMissing`] when the tag is absent, or I/O,
    /// decompression, and size-cap errors while reading the payload.
    pub fn read_section(&mut self, provider: [u8; 4], sub: [u8; 4]) -> PersistResult<Vec<u8>> {
        let index = self
            .sections
            .iter()
            .position(|entry| entry.provider == provider && entry.sub == sub)
            .ok_or(PersistError::SectionMissing { provider, sub })?;
        self.read_section_at(index)
    }

    /// Read a section by section-table index.
    ///
    /// # Errors
    ///
    /// Returns I/O errors for an out-of-bounds index and for file reads, plus
    /// decompression or size-cap errors for compressed sections.
    pub fn read_section_at(&mut self, index: usize) -> PersistResult<Vec<u8>> {
        let entry = *self.sections.get(index).ok_or_else(|| {
            PersistError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "snapshot section index out of bounds",
            ))
        })?;
        let payload = self.read_payload(&entry)?;
        if self.header.is_section_compressed() {
            decompress_zstd_bounded(&payload, MAX_SECTION_PAYLOAD_BYTES, |len, max| {
                PersistError::SectionTooLarge { len, max }
            })
        } else {
            Ok(payload)
        }
    }

    /// Recompute and validate the snapshot body hash.
    ///
    /// Streams payload bytes through the hasher in 64 KiB chunks rather than
    /// materializing every section in memory, so peak memory stays bounded
    /// regardless of total snapshot size.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::BodyHashMismatch`] if the stored and recomputed
    /// hashes differ, or I/O errors while reading payload bytes.
    pub fn verify_body_hash(&mut self) -> PersistResult<()> {
        let table = section_table_bytes(&self.sections)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&table);
        let mut buf = [0_u8; BODY_HASH_READ_BUFFER_BYTES];
        // Cheap Arc pointer-bump (not a deep Vec clone) sidesteps the `&self`
        // borrow conflict between `self.sections` and the `&mut self.file` reads.
        let entries = Arc::clone(&self.sections);
        for entry in entries.iter() {
            self.file.seek(SeekFrom::Start(entry.payload_offset))?;
            let mut remaining = entry.payload_len;
            while remaining > 0 {
                let want = remaining.min(buf.len() as u64) as usize;
                self.file.read_exact(&mut buf[..want])?;
                hasher.update(&buf[..want]);
                remaining -= want as u64;
            }
        }
        let digest = hasher.finalize();
        let mut observed = [0_u8; 16];
        observed.copy_from_slice(&digest.as_bytes()[..16]);
        if observed != self.header.body_hash {
            return Err(PersistError::BodyHashMismatch {
                expected: self.header.body_hash,
                observed,
            });
        }
        Ok(())
    }

    fn read_payload(&mut self, entry: &SectionEntry) -> PersistResult<Vec<u8>> {
        let len =
            usize::try_from(entry.payload_len).map_err(|_| PersistError::SectionTooLarge {
                len: usize::MAX,
                max: MAX_SECTION_PAYLOAD_BYTES,
            })?;
        let mut payload = vec![0_u8; len];
        self.file.seek(SeekFrom::Start(entry.payload_offset))?;
        self.file.read_exact(&mut payload)?;
        Ok(payload)
    }
}

fn validate_unique_tags(sections: &[SectionEntry]) -> PersistResult<()> {
    let mut seen = HashSet::with_capacity(sections.len());
    for entry in sections {
        if !seen.insert(entry.tag_pair()) {
            return Err(PersistError::DuplicateSection {
                provider: entry.provider,
                sub: entry.sub,
            });
        }
    }
    Ok(())
}

fn validate_section_offsets(sections: &[SectionEntry], file_len: u64) -> PersistResult<()> {
    let payloads_start = SNAPSHOT_FILE_HEADER_LEN as u64
        + (sections.len() as u64).saturating_mul(SECTION_TABLE_ROW_LEN as u64);
    for entry in sections {
        if entry.payload_offset < payloads_start {
            return Err(PersistError::MalformedSectionLayout {
                reason: "section payload offset overlaps header or table",
            });
        }
        let end = entry.payload_offset.checked_add(entry.payload_len).ok_or(
            PersistError::MalformedSectionLayout {
                reason: "section offset+length overflows u64",
            },
        )?;
        if end > file_len {
            return Err(PersistError::MalformedSectionLayout {
                reason: "section payload extends past end of file",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    use proptest::prelude::*;

    use super::*;
    use crate::section::{SECTION_TABLE_ROW_LEN, section_table_bytes};
    use crate::{
        FLAG_BODY_COMPRESSED, FLAG_SECTION_COMPRESSED, SNAPSHOT_FILE_HEADER_LEN, SnapshotBuilder,
        SnapshotConfig, snapshot_path,
    };

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "selene-snapshot-reader-{name}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        dir
    }

    fn write_snapshot(
        dir: &Path,
        sequence: u64,
        compression: crate::SectionCompression,
        sections: &[([u8; 4], [u8; 4], Vec<u8>)],
    ) -> std::path::PathBuf {
        let mut builder = SnapshotBuilder::new(SnapshotConfig {
            dir: dir.to_path_buf(),
            sequence,
            compression,
            fsync: true,
        })
        .unwrap();
        for (provider, sub, payload) in sections {
            builder
                .add_section(*provider, *sub, payload.clone())
                .unwrap();
        }
        let outcome = builder.finalize().unwrap();
        assert_eq!(outcome.snapshot_seq, sequence);
        crate::snapshot_path(dir, sequence)
    }

    #[test]
    fn compressed_and_uncompressed_round_trip() {
        for (idx, compression) in [
            crate::SectionCompression::None,
            crate::SectionCompression::PerSection { level: 1 },
        ]
        .into_iter()
        .enumerate()
        {
            let dir = temp_dir("round");
            let path = write_snapshot(
                &dir,
                idx as u64,
                compression,
                &[(*b"CORE", *b"META", vec![idx as u8; 512])],
            );
            let mut reader = SnapshotReader::open(&path).unwrap();
            reader.verify_body_hash().unwrap();
            assert_eq!(
                reader.read_section(*b"CORE", *b"META").unwrap(),
                vec![idx as u8; 512]
            );
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn read_section_missing_tag_reports_section_missing() {
        let dir = temp_dir("missing");
        let path = write_snapshot(&dir, 1, crate::SectionCompression::None, &[]);
        let mut reader = SnapshotReader::open(&path).unwrap();
        assert!(matches!(
            reader.read_section(*b"CORE", *b"META"),
            Err(PersistError::SectionMissing { provider, sub })
                if provider == *b"CORE" && sub == *b"META"
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn body_hash_mismatch_is_reported() {
        let dir = temp_dir("hash");
        let path = write_snapshot(
            &dir,
            2,
            crate::SectionCompression::None,
            &[(*b"CORE", *b"META", b"payload".to_vec())],
        );
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0xFF;
        fs::write(&path, bytes).unwrap();
        let mut reader = SnapshotReader::open(&path).unwrap();
        assert!(matches!(
            reader.verify_body_hash(),
            Err(PersistError::BodyHashMismatch { .. })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unsupported_flag_at_open_is_rejected() {
        let dir = temp_dir("flag");
        let path = snapshot_path(&dir, 3);
        let header = SnapshotFileHeader {
            version_major: crate::SNAPSHOT_VERSION_MAJOR,
            version_minor: crate::SNAPSHOT_VERSION_MINOR,
            flags: FLAG_BODY_COMPRESSED,
            section_count: 0,
            body_hash: [0; 16],
        };
        let mut file = File::create(&path).unwrap();
        header.write_to(&mut file).unwrap();
        assert!(matches!(
            SnapshotReader::open(&path),
            Err(PersistError::UnsupportedFlag {
                flag: FLAG_BODY_COMPRESSED
            })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reserved_bytes_nonzero_at_open_is_rejected() {
        let dir = temp_dir("reserved");
        let path = write_snapshot(&dir, 4, crate::SectionCompression::None, &[]);
        let mut bytes = fs::read(&path).unwrap();
        bytes[12] = 1;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            SnapshotReader::open(&path),
            Err(PersistError::ReservedBytesNonZero { offset: 12 })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_section_payload_len_at_open_is_rejected() {
        let dir = temp_dir("oversize-row");
        let path = snapshot_path(&dir, 5);
        let entry = SectionEntry {
            provider: *b"CORE",
            sub: *b"META",
            payload_offset: SNAPSHOT_FILE_HEADER_LEN as u64 + SECTION_TABLE_ROW_LEN as u64,
            payload_len: crate::MAX_SECTION_PAYLOAD_BYTES as u64 + 1,
        };
        let table = section_table_bytes(&[entry]).unwrap();
        let hash = crate::section::body_hash(&table, std::iter::empty::<&[u8]>());
        SnapshotFileHeader::new(0, 1, hash)
            .unwrap()
            .write_to(&mut File::create(&path).unwrap())
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&table)
            .unwrap();
        assert!(matches!(
            SnapshotReader::open(&path),
            Err(PersistError::SectionTooLarge { .. })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn truncated_section_table_is_reported() {
        let dir = temp_dir("truncated-table");
        let path = snapshot_path(&dir, 6);
        let header = SnapshotFileHeader::new(0, 1, [0; 16]).unwrap();
        let mut file = File::create(&path).unwrap();
        header.write_to(&mut file).unwrap();
        file.write_all(&[0_u8; 2]).unwrap();
        assert!(matches!(
            SnapshotReader::open(&path),
            Err(PersistError::TruncatedSectionTable { offset })
                if offset == SNAPSHOT_FILE_HEADER_LEN as u64
        ));
        let _ = fs::remove_dir_all(dir);
    }

    proptest! {
        #[test]
        fn snapshot_round_trips(
            sections in proptest::collection::vec(
                (any::<[u8; 4]>(), any::<[u8; 4]>(), proptest::collection::vec(any::<u8>(), 0..8192)),
                0..16,
            ).prop_filter("duplicate tags are rejected by the builder", |sections| {
                let mut seen = std::collections::HashSet::new();
                sections.iter().all(|(provider, sub, _)| seen.insert((*provider, *sub)))
            })
        ) {
            for (sequence, compression) in [
                (7, crate::SectionCompression::None),
                (8, crate::SectionCompression::PerSection { level: 1 }),
            ] {
                let dir = temp_dir("prop-round");
                let path = write_snapshot(&dir, sequence, compression, &sections);
                let mut reader = SnapshotReader::open(&path).unwrap();
                reader.verify_body_hash().unwrap();
                for (idx, (provider, sub, payload)) in sections.iter().enumerate() {
                    let by_tag = reader.read_section(*provider, *sub).unwrap();
                    prop_assert_eq!(by_tag.as_slice(), payload.as_slice());
                    let by_index = reader.read_section_at(idx).unwrap();
                    prop_assert_eq!(by_index.as_slice(), payload.as_slice());
                }
                let _ = fs::remove_dir_all(dir);
            }
        }

        #[test]
        fn body_hash_detects_single_byte_corruption(payload in proptest::collection::vec(any::<u8>(), 1..8192)) {
            let dir = temp_dir("prop-corrupt");
            let path = write_snapshot(
                &dir,
                8,
                crate::SectionCompression::None,
                &[(*b"CORE", *b"META", payload)],
            );
            let mut bytes = fs::read(&path).unwrap();
            *bytes.last_mut().unwrap() ^= 0x01;
            fs::write(&path, bytes).unwrap();
            let mut reader = SnapshotReader::open(&path).unwrap();
            let mismatch = matches!(reader.verify_body_hash(), Err(PersistError::BodyHashMismatch { .. }));
            prop_assert!(mismatch);
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn section_compressed_flag_is_exposed() {
        let dir = temp_dir("section-flag");
        let path = write_snapshot(
            &dir,
            9,
            crate::SectionCompression::PerSection { level: 1 },
            &[(*b"CORE", *b"META", b"payload".to_vec())],
        );
        let reader = SnapshotReader::open(&path).unwrap();
        assert_eq!(reader.header().flags, FLAG_SECTION_COMPRESSED);
        let _ = fs::remove_dir_all(dir);
    }

    /// Hand-write a snapshot file with the supplied entries directly, bypassing
    /// the builder so we can exercise reader-side validation against malformed
    /// inputs the builder would otherwise reject.
    fn write_raw_snapshot(path: &Path, entries: &[SectionEntry], extra_payload: &[u8]) {
        let table = section_table_bytes(entries).unwrap();
        let hash = crate::section::body_hash(&table, [extra_payload]);
        let header = SnapshotFileHeader::new(0, entries.len(), hash).unwrap();
        let mut file = File::create(path).unwrap();
        header.write_to(&mut file).unwrap();
        file.write_all(&table).unwrap();
        file.write_all(extra_payload).unwrap();
    }

    #[test]
    fn duplicate_section_tag_at_open_is_rejected() {
        let dir = temp_dir("dup-open");
        let path = snapshot_path(&dir, 10);
        let payload_offset = SNAPSHOT_FILE_HEADER_LEN as u64 + 2 * SECTION_TABLE_ROW_LEN as u64;
        let entries = vec![
            SectionEntry {
                provider: *b"CORE",
                sub: *b"META",
                payload_offset,
                payload_len: 0,
            },
            SectionEntry {
                provider: *b"CORE",
                sub: *b"META",
                payload_offset,
                payload_len: 0,
            },
        ];
        write_raw_snapshot(&path, &entries, &[]);
        assert!(matches!(
            SnapshotReader::open(&path),
            Err(PersistError::DuplicateSection { provider, sub })
                if provider == *b"CORE" && sub == *b"META"
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn section_offset_inside_header_or_table_is_rejected() {
        let dir = temp_dir("overlap");
        let path = snapshot_path(&dir, 11);
        let entries = vec![SectionEntry {
            provider: *b"CORE",
            sub: *b"META",
            payload_offset: 8, // inside the 32-byte header
            payload_len: 0,
        }];
        write_raw_snapshot(&path, &entries, &[]);
        assert!(matches!(
            SnapshotReader::open(&path),
            Err(PersistError::MalformedSectionLayout { reason })
                if reason.contains("header or table")
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn section_payload_past_eof_is_rejected() {
        let dir = temp_dir("past-eof");
        let path = snapshot_path(&dir, 12);
        let payload_offset = SNAPSHOT_FILE_HEADER_LEN as u64 + SECTION_TABLE_ROW_LEN as u64;
        let entries = vec![SectionEntry {
            provider: *b"CORE",
            sub: *b"META",
            payload_offset,
            payload_len: 4096, // file is only 56 bytes
        }];
        write_raw_snapshot(&path, &entries, &[]);
        assert!(matches!(
            SnapshotReader::open(&path),
            Err(PersistError::MalformedSectionLayout { reason })
                if reason.contains("end of file")
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn section_offset_overflow_is_rejected() {
        let dir = temp_dir("overflow");
        let path = snapshot_path(&dir, 13);
        let entries = vec![SectionEntry {
            provider: *b"CORE",
            sub: *b"META",
            payload_offset: u64::MAX,
            payload_len: 1,
        }];
        write_raw_snapshot(&path, &entries, &[]);
        assert!(matches!(
            SnapshotReader::open(&path),
            Err(PersistError::MalformedSectionLayout { reason })
                if reason.contains("overflows")
        ));
        let _ = fs::remove_dir_all(dir);
    }
}
