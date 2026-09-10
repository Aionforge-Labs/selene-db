//! Read-only WAL iteration.

use std::fs::File;
use std::io::{BufReader, Cursor, Read};
use std::path::{Path, PathBuf};

use selene_core::Change;

use crate::entry_header::{ensure_payload_len, read_entry_header};
use crate::file_header::{WAL_FILE_HEADER_LEN, WalFileHeader};
use crate::payload::{decode_changes, verify_checksum};
use crate::{PersistError, PersistResult, WalEntryHeader};

/// WAL reader for a single WAL file.
pub struct WalReader {
    directory: crate::StoreDirectory,
    path: PathBuf,
    snapshot_seq: u64,
}

impl WalReader {
    /// Open and validate a WAL file for read-only iteration.
    ///
    /// # Errors
    ///
    /// Returns I/O or file-header validation errors.
    pub fn open(path: &Path) -> PersistResult<Self> {
        let (dir, name) = crate::StoreDirectory::for_file(path)?;
        Self::open_in(&dir, &name)
    }

    /// Read a WAL through a retained directory capability.
    ///
    /// # Errors
    /// Returns protocol, file-open, or header validation errors.
    pub fn open_in(dir: &crate::StoreDirectory, name: &Path) -> PersistResult<Self> {
        dir.require_legacy()?;
        let mut file = dir.open_read(name)?;
        let header = WalFileHeader::read_from(&mut file)?;
        Ok(Self {
            path: name.to_path_buf(),
            directory: dir.clone(),
            snapshot_seq: header.snapshot_seq,
        })
    }

    /// Highest WAL sequence covered by the snapshot this file extends.
    #[must_use]
    pub const fn snapshot_seq(&self) -> u64 {
        self.snapshot_seq
    }

    /// Iterate entries whose headers match `filter`.
    ///
    /// The iterator reads payload bytes to advance the stream, but postcard
    /// body decoding is deferred until [`WalEntryView::body`] is called.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from opening a fresh read handle.
    pub fn iterate<F>(&self, filter: F) -> PersistResult<WalEntryStream<BufReader<File>>>
    where
        F: Fn(&WalEntryHeader) -> bool + 'static,
    {
        let file = self.directory.open_read(&self.path)?;
        let mut file = BufReader::with_capacity(64 * 1024, file);
        WalFileHeader::read_from(&mut file)?;
        let file_len = file.get_ref().metadata()?.len();
        Ok(WalEntryStream::from_reader(
            file,
            file_len,
            Box::new(filter),
        ))
    }

    /// Iterate WAL entries directly over an in-memory byte slice.
    ///
    /// Validates the WAL file header at the front of `bytes`, then streams every
    /// entry over a [`Cursor`] exactly as [`Self::iterate`] streams over a file
    /// handle (`file_len` is taken from the slice length, so the per-entry
    /// payload-vs-remaining bound is the slice bound). Unlike [`Self::iterate`]
    /// there is no header filtering — every entry is yielded — which is the most
    /// aggressive form for exercising the full decode path on untrusted bytes.
    ///
    /// # Errors
    ///
    /// Returns the same file-header validation errors as [`Self::open`].
    pub fn from_bytes(bytes: &[u8]) -> PersistResult<WalEntryStream<Cursor<&[u8]>>> {
        let mut cursor = Cursor::new(bytes);
        WalFileHeader::read_from(&mut cursor)?;
        let file_len = bytes.len() as u64;
        Ok(WalEntryStream::from_reader(
            cursor,
            file_len,
            Box::new(|_| true),
        ))
    }
}

/// Streaming WAL entry iterator over a reader `R` (a buffered file for
/// [`WalReader::iterate`], an in-memory [`Cursor`] for [`WalReader::from_bytes`]).
pub struct WalEntryStream<R = BufReader<File>> {
    file: R,
    file_len: u64,
    next_offset: u64,
    previous_sequence: u64,
    filter: Box<dyn Fn(&WalEntryHeader) -> bool>,
    stopped: bool,
}

impl<R: Read> WalEntryStream<R> {
    /// Build a stream over a reader already positioned just past the validated
    /// WAL file header. `file_len` bounds the per-entry payload reads.
    fn from_reader(file: R, file_len: u64, filter: Box<dyn Fn(&WalEntryHeader) -> bool>) -> Self {
        Self {
            file,
            file_len,
            next_offset: WAL_FILE_HEADER_LEN as u64,
            previous_sequence: 0,
            filter,
            stopped: false,
        }
    }
}

impl<R: Read> Iterator for WalEntryStream<R> {
    type Item = PersistResult<WalEntryView>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.stopped {
            return None;
        }
        loop {
            if self.next_offset >= self.file_len {
                self.stopped = true;
                return None;
            }

            let offset = self.next_offset;
            let (header, bytes_consumed) = match read_entry_header(&mut self.file, offset) {
                Ok(header) => header,
                Err(error) => {
                    self.stopped = true;
                    return Some(Err(error));
                }
            };
            if header.sequence <= self.previous_sequence {
                self.stopped = true;
                return Some(Err(PersistError::NonMonotonicSequence {
                    previous: self.previous_sequence,
                    current: header.sequence,
                }));
            }

            let payload_len = header.payload_len as usize;
            if let Err(error) = ensure_payload_len(payload_len) {
                self.stopped = true;
                return Some(Err(error));
            }
            let payload_start = offset.saturating_add(bytes_consumed as u64);
            let payload_end = payload_start.saturating_add(u64::from(header.payload_len));
            if payload_end > self.file_len {
                self.stopped = true;
                return Some(Err(PersistError::TruncatedEntry { offset }));
            }
            let mut payload = vec![0_u8; payload_len];
            if let Err(error) = self.file.read_exact(&mut payload) {
                self.stopped = true;
                return Some(Err(error.into()));
            }

            self.previous_sequence = header.sequence;
            self.next_offset = payload_end;
            if !(self.filter)(&header) {
                continue;
            }
            return Some(Ok(WalEntryView { header, payload }));
        }
    }
}

/// Header-filtered WAL entry view with lazily decoded body.
///
/// Owns its payload bytes outright, so it carries no borrow of the underlying
/// reader and outlives the [`WalEntryStream`] that produced it.
pub struct WalEntryView {
    /// Decoded entry header.
    pub header: WalEntryHeader,
    payload: Vec<u8>,
}

impl WalEntryView {
    /// Validate the checksum and decode the body as `Vec<Change>`.
    ///
    /// # Errors
    ///
    /// Returns checksum, compression, postcard payload decode, or malformed
    /// checkpoint-watermark errors.
    pub fn body(&self) -> PersistResult<Vec<Change>> {
        verify_checksum(&self.header, &self.payload)?;
        let changes = decode_changes(&self.payload, self.header.is_payload_compressed())?;
        validate_checkpoint_watermark(&self.header, &changes)?;
        Ok(changes)
    }

    /// Consume this view and return a fully decoded WAL entry.
    ///
    /// # Errors
    ///
    /// Returns checksum, compression, postcard payload decode, or malformed
    /// checkpoint-watermark errors.
    pub fn into_entry(self) -> PersistResult<WalEntry> {
        let changes = self.body()?;
        Ok(WalEntry {
            header: self.header,
            changes,
        })
    }
}

fn validate_checkpoint_watermark(header: &WalEntryHeader, changes: &[Change]) -> PersistResult<()> {
    if header.is_checkpoint_watermark()
        && (!matches!(header.origin, selene_core::Origin::Local)
            || header.principal.is_some()
            || !changes.is_empty())
    {
        return Err(PersistError::HeaderCodec(format!(
            "checkpoint watermark at sequence {} must be local, principal-free, and empty",
            header.sequence
        )));
    }
    Ok(())
}

/// Fully decoded WAL entry.
#[derive(Clone, Debug, PartialEq)]
pub struct WalEntry {
    /// Decoded entry header.
    pub header: WalEntryHeader,
    /// Decoded `Vec<Change>` body.
    pub changes: Vec<Change>,
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::Arc;

    use selene_core::{
        Change, HlcTimestamp, LabelSet, NodeId, Origin, PropertyMap, Value, db_string,
    };

    use super::*;
    use crate::entry_header::encode_entry_header;
    use crate::file_header::WalFileHeader;
    use crate::payload::{checksum_lo, encode_changes};
    use crate::{WalConfig, WalWriter};

    fn temp_path(_name: &str) -> selene_testing::PersistenceTestPath {
        selene_testing::PersistenceTestPath::new()
    }

    fn changes(id: u64) -> Vec<Change> {
        vec![Change::NodeCreated {
            id: NodeId::new(id),
            labels: LabelSet::single(db_string("reader.node").unwrap()),
            properties: PropertyMap::from_pairs([(db_string("reader.p").unwrap(), Value::Int(1))])
                .unwrap(),
        }]
    }

    fn large_changes(seed: u32, len: usize) -> Vec<Change> {
        let mut state = seed;
        let mut payload = Vec::with_capacity(len);
        for _ in 0..len {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            payload.push((state >> 24) as u8);
        }
        vec![Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(db_string("reader.boundary").unwrap()),
            properties: PropertyMap::from_pairs([(
                db_string("reader.payload").unwrap(),
                Value::Bytes(Arc::from(payload)),
            )])
            .unwrap(),
        }]
    }

    fn append(path: &Path, principal: Option<Arc<[u8]>>, id: u64) {
        let mut writer = WalWriter::open(path, WalConfig::default()).unwrap();
        writer
            .append(
                HlcTimestamp::new(id, 0),
                Origin::Local,
                principal,
                &changes(id),
            )
            .unwrap();
    }

    #[test]
    fn empty_wal_iterates_no_entries() {
        let path = temp_path("empty");
        {
            let _writer = WalWriter::open(&path, WalConfig::default()).unwrap();
        }
        let reader = WalReader::open(&path).unwrap();
        assert_eq!(reader.snapshot_seq(), 0);
        assert!(reader.iterate(|_| true).unwrap().next().is_none());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn entries_round_trip() {
        let path = temp_path("round");
        append(&path, None, 1);
        append(&path, None, 2);
        let reader = WalReader::open(&path).unwrap();
        let entries: Vec<_> = reader
            .iterate(|_| true)
            .unwrap()
            .map(|result| result.unwrap().into_entry().unwrap())
            .collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].changes, changes(1));
        assert_eq!(entries[1].changes, changes(2));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn malformed_checkpoint_watermarks_are_rejected() {
        use crate::FLAG_CHECKPOINT_WATERMARK;

        let cases = [
            ("changes", Origin::Local, None, changes(1)),
            (
                "replicated",
                Origin::Replicated {
                    source_node_id: NodeId::new(7),
                    source_seq: 1,
                },
                None,
                Vec::new(),
            ),
            (
                "principal",
                Origin::Local,
                Some(Arc::from(b"actor".as_slice())),
                Vec::new(),
            ),
        ];
        for (name, origin, principal, body) in cases {
            let path = temp_path(name);
            let payload = encode_changes(&body).unwrap();
            let header = WalEntryHeader::new(
                payload.bytes.len(),
                payload.checksum_lo,
                1,
                HlcTimestamp::new(1, 0),
                origin,
                payload.flags | FLAG_CHECKPOINT_WATERMARK,
                principal,
            )
            .unwrap();
            let mut file = fs::File::create(&path).unwrap();
            WalFileHeader::new(0).write_to(&mut file).unwrap();
            file.write_all(&encode_entry_header(&header).unwrap())
                .unwrap();
            file.write_all(&payload.bytes).unwrap();
            drop(file);

            let view = WalReader::open(&path)
                .unwrap()
                .iterate(|_| true)
                .unwrap()
                .next()
                .unwrap()
                .unwrap();
            assert!(matches!(
                view.body(),
                Err(PersistError::HeaderCodec(reason))
                    if reason.contains("checkpoint watermark")
            ));
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn header_filter_skips_unmatched_bodies() {
        let path = temp_path("filter");
        append(&path, Some(Arc::from(b"alice".as_slice())), 1);
        append(&path, Some(Arc::from(b"bob".as_slice())), 2);
        let reader = WalReader::open(&path).unwrap();
        let entries: Vec<_> = reader
            .iterate(|header| header.principal.as_deref() == Some(b"alice".as_slice()))
            .unwrap()
            .map(|result| result.unwrap().into_entry().unwrap())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].header.principal.as_deref(),
            Some(b"alice".as_slice())
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn bufreader_handles_partial_buffer_boundary() {
        let path = temp_path("buffer-boundary");
        let first = large_changes(0x1234_5678, 70 * 1024);
        let second = large_changes(0x8765_4321, 70 * 1024);
        {
            let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
            writer
                .append(HlcTimestamp::new(1, 0), Origin::Local, None, &first)
                .unwrap();
            writer
                .append(HlcTimestamp::new(1, 1), Origin::Local, None, &second)
                .unwrap();
        }
        let reader = WalReader::open(&path).unwrap();
        let entries: Vec<_> = reader
            .iterate(|_| true)
            .unwrap()
            .map(|result| result.unwrap().body().unwrap())
            .collect();
        assert_eq!(entries, vec![first, second]);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn checksum_corruption_is_reported_by_body() {
        let path = temp_path("checksum");
        append(&path, None, 1);
        {
            let mut bytes = fs::read(&path).unwrap();
            let last = bytes.last_mut().unwrap();
            *last ^= 0xFF;
            fs::write(&path, bytes).unwrap();
        }
        let reader = WalReader::open(&path).unwrap();
        let view = reader.iterate(|_| true).unwrap().next().unwrap().unwrap();
        assert!(matches!(
            view.body(),
            Err(PersistError::ChecksumMismatch { sequence: 1 })
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn truncated_entry_stops_iterator_with_error() {
        let path = temp_path("trunc");
        append(&path, None, 1);
        let len = fs::metadata(&path).unwrap().len();
        {
            let file = OpenOptions::new().write(true).open(&path).unwrap();
            file.set_len(len - 1).unwrap();
        }
        let reader = WalReader::open(&path).unwrap();
        assert!(matches!(
            reader.iterate(|_| true).unwrap().next().unwrap(),
            Err(PersistError::TruncatedEntry { .. })
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn from_bytes_matches_file_path_iteration() {
        let path = temp_path("from-bytes-eq");
        append(&path, None, 1);
        append(&path, Some(Arc::from(b"alice".as_slice())), 2);
        let via_file: Vec<_> = WalReader::open(&path)
            .unwrap()
            .iterate(|_| true)
            .unwrap()
            .map(|r| r.unwrap().into_entry().unwrap())
            .collect();
        let bytes = fs::read(&path).unwrap();
        let via_slice: Vec<_> = WalReader::from_bytes(&bytes)
            .unwrap()
            .map(|r| r.unwrap().into_entry().unwrap())
            .collect();
        assert_eq!(via_file, via_slice);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn from_bytes_payload_len_past_remaining_yields_truncated_entry() {
        // A WAL file header + one entry header whose payload_len is large (but
        // under the 256 MiB cap, so ensure_payload_len passes) with only a few
        // payload bytes supplied. The slice reader must yield TruncatedEntry
        // WITHOUT allocating the claimed payload (the payload_end > file_len
        // bound runs before the `vec![0; payload_len]` allocation).
        let header = WalEntryHeader::new(
            64 * 1024 * 1024,
            0,
            1,
            HlcTimestamp::zero(),
            Origin::Local,
            0,
            None,
        )
        .unwrap();
        let mut bytes = Vec::new();
        WalFileHeader::new(0).write_to(&mut bytes).unwrap();
        bytes.extend_from_slice(&encode_entry_header(&header).unwrap());
        bytes.extend_from_slice(&[0_u8; 8]);
        let mut stream = WalReader::from_bytes(&bytes).unwrap();
        assert!(matches!(
            stream.next().unwrap(),
            Err(PersistError::TruncatedEntry { .. })
        ));
    }

    #[test]
    fn from_bytes_rejects_bad_file_header() {
        assert!(matches!(
            WalReader::from_bytes(b"NOPExxxxxxxxxxxx"),
            Err(PersistError::MagicMismatch { .. })
        ));
        assert!(matches!(
            WalReader::from_bytes(&[0_u8; 4]),
            Err(PersistError::TruncatedFileHeader)
        ));
    }

    #[test]
    fn non_monotonic_sequence_is_reported() {
        let path = temp_path("nonmono");
        let payload = encode_changes(&changes(1)).unwrap();
        let header1 = WalEntryHeader::new(
            payload.bytes.len(),
            payload.checksum_lo,
            1,
            HlcTimestamp::zero(),
            Origin::Local,
            payload.flags,
            None,
        )
        .unwrap();
        let header2 = WalEntryHeader::new(
            payload.bytes.len(),
            payload.checksum_lo,
            1,
            HlcTimestamp::zero(),
            Origin::Local,
            payload.flags,
            None,
        )
        .unwrap();
        {
            let mut file = File::create(&path).unwrap();
            WalFileHeader::new(0).write_to(&mut file).unwrap();
            file.write_all(&encode_entry_header(&header1).unwrap())
                .unwrap();
            file.write_all(&payload.bytes).unwrap();
            file.write_all(&encode_entry_header(&header2).unwrap())
                .unwrap();
            file.write_all(&payload.bytes).unwrap();
        }
        let reader = WalReader::open(&path).unwrap();
        let mut stream = reader.iterate(|_| true).unwrap();
        assert!(stream.next().unwrap().unwrap().body().is_ok());
        assert!(matches!(
            stream.next().unwrap(),
            Err(PersistError::NonMonotonicSequence {
                previous: 1,
                current: 1
            })
        ));
        let _ = fs::remove_file(path);
    }

    /// A released v1.2.0-v1.4.0 store, byte for byte, opened by a v3 reader.
    /// Expressed as the literal 16-byte v2 header rather than as
    /// `WAL_VERSION_MAJOR - 1`, because the point is the exact on-disk shape a
    /// real consumer has.
    #[test]
    fn released_v2_store_is_rejected_by_version() {
        let path = temp_path("version");
        {
            let mut file = File::create(&path).unwrap();
            file.write_all(b"SLDB").unwrap();
            file.write_all(&2_u16.to_le_bytes()).unwrap();
            file.write_all(&2_u16.to_le_bytes()).unwrap();
            file.write_all(&0_u64.to_le_bytes()).unwrap();
        }
        assert!(matches!(
            WalReader::open(&path),
            Err(PersistError::UnsupportedVersion {
                artifact: crate::PersistArtifact::Wal,
                major: 2,
                minor: 2
            })
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unsupported_v1_file_is_rejected() {
        let path = temp_path("version-v1");
        {
            let mut file = File::create(&path).unwrap();
            file.write_all(b"SLDB").unwrap();
            file.write_all(&1_u16.to_le_bytes()).unwrap();
            file.write_all(&0_u16.to_le_bytes()).unwrap();
            file.write_all(&0_u64.to_le_bytes()).unwrap();
        }
        assert!(matches!(
            WalReader::open(&path),
            Err(PersistError::UnsupportedVersion {
                artifact: crate::PersistArtifact::Wal,
                major: 1,
                minor: 0
            })
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn malformed_payload_reports_payload_codec() {
        let path = temp_path("payload-codec");
        let payload = vec![0xFF, 0xFF, 0xFF];
        let header = WalEntryHeader::new(
            payload.len(),
            checksum_lo(&payload),
            1,
            HlcTimestamp::zero(),
            Origin::Local,
            0,
            None,
        )
        .unwrap();
        {
            let mut file = File::create(&path).unwrap();
            WalFileHeader::new(0).write_to(&mut file).unwrap();
            file.write_all(&encode_entry_header(&header).unwrap())
                .unwrap();
            file.write_all(&payload).unwrap();
        }
        let reader = WalReader::open(&path).unwrap();
        let view = reader.iterate(|_| true).unwrap().next().unwrap().unwrap();
        assert!(matches!(view.body(), Err(PersistError::PayloadCodec(_))));
        let _ = fs::remove_file(path);
    }
}
