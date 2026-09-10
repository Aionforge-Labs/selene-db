//! Append-only audit log (`audit.log`, `SLAU`): durable engine-event record
//! with retention independent of the WAL + snapshot lineage.
//!
//! Per the 2026-05-26 deletion+reclamation audit (Item 7 / Seam D), engine
//! events that were written into the WAL change stream got moved by
//! `WalWriter::rotate` into `wal.{N}.archive` files, which nothing reads — so an
//! embedder that pruned archives (Item 5) silently lost that history. The audit
//! log fixes this by recording engine-owned events in a dedicated file with its
//! own [`AuditRetentionPolicy`], decoupled from WAL-archive pruning. It is the
//! durable "events" surface in the snapshot=state / WAL=changes / audit=events
//! split, available as a forward framework for user-action audit events.
//!
//! # Layering: clock- and semantics-agnostic
//!
//! This module is below `selene-graph` and never interprets event payloads. A
//! record is an opaque `kind`-tagged byte blob plus a caller-supplied wall-clock
//! stamp ([`AuditRecord::recorded_at_unix_nanos`]). The caller (the graph
//! mutation funnel) stamps the time and serializes the typed event into the
//! payload, so `audit.rs` stays deterministic in tests and free of any clock or
//! lifecycle dependency. `kind` tags are reserved here (e.g.
//! [`AUDIT_KIND_RESERVED_0`]); the meaning of the payload lives one layer up.
//!
//! # Format (`format_version = 2`)
//!
//! ```text
//! file header (hand-rolled LE, 8 bytes):
//!   [0..4) magic = b"SLAU"
//!   [4..6) format_version: u16 LE = 2
//!   [6..8) reserved:       u16 LE = 0  (zero-checked on read)
//! then zero or more append-only records, each:
//!   fixed record header (LE, 24 bytes):
//!     [0..4)   header_checksum:        u32 LE (low 32 of xxh3_64 over [4..24))
//!     [4..12)  recorded_at_unix_nanos: u64 LE
//!     [12..14) kind:                   u16 LE
//!     [14..16) reserved:               u16 LE = 0
//!     [16..20) payload_len:            u32 LE (<= MAX_AUDIT_PAYLOAD_BYTES)
//!     [20..24) payload_checksum_lo:    u32 LE (low 32 of xxh3_64(payload))
//!   payload: payload_len opaque bytes
//! ```
//!
//! The header checksum sits at offset 0 so its coverage is a suffix: no field
//! has to be zeroed and no scratch copy is made to verify it. This mirrors the
//! WAL v3 prefix checksum, for the same reason.
//!
//! # Crash safety
//!
//! Append is the only growth operation, so a power loss can only tear the
//! *final* record. [`AuditLog::open`] scans from the file header and repairs a
//! torn tail, but **refuses** a record that fails validation with records after
//! it, leaving the file untouched
//! ([`PersistError::AuditMidLogCorruption`]). v1 could not tell those apart —
//! it truncated at the first bad record and fsynced the shortened file,
//! discarding acknowledged records on every recovery — because `payload_len`
//! was the only field saying where a record ended and nothing protected it.
//!
//! [`AuditLog::prune`] rewrites the whole log via the write-tmp → fsync →
//! atomic-rename → dir-fsync idiom
//! ([`crate::manifest::Manifest::write_atomic`]'s pattern), so a crash
//! mid-prune leaves the prior `audit.log` fully intact.
//!
//! # Compatibility
//!
//! v1 logs are rejected at open with [`PersistError::UnsupportedVersion`]
//! naming [`PersistArtifact::AuditLog`]. There is no dual decoder: the audit
//! log is an events surface with its own retention, so the recovery path for a
//! v1 file is to archive or discard it, not to migrate it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{PersistArtifact, PersistError, PersistResult};

/// Audit-log file magic.
pub const AUDIT_MAGIC: [u8; 4] = *b"SLAU";
/// Audit-log format version understood by this build.
pub const AUDIT_FORMAT_VERSION: u16 = 2;
/// Conventional audit-log file name used by embedders.
pub const DEFAULT_AUDIT_FILE_NAME: &str = "audit.log";
/// Maximum opaque payload bytes per audit record (1 MiB).
pub const MAX_AUDIT_PAYLOAD_BYTES: usize = 1 << 20;
/// Reserved `kind` tag (value `1`). No engine event currently produces it; it
/// is held reserved so the `kind` space stays stable for the forward
/// user-action / first-party engine audit framework. The payload meaning lives
/// in the layer that writes it; `audit.rs` only stores the tag.
pub const AUDIT_KIND_RESERVED_0: u16 = 1;

const AUDIT_FILE_HEADER_LEN: usize = 8;
const AUDIT_RECORD_HEADER_LEN: usize = 24;
/// Bytes the record-header checksum covers: everything after it.
const HEADER_CHECKSUM_COVERAGE: std::ops::Range<usize> = 4..AUDIT_RECORD_HEADER_LEN;

/// A decoded record header. The checksum is verified before this is produced,
/// so every field here came from bytes the checksum covered.
pub(crate) struct RecordHeader {
    pub(crate) recorded_at_unix_nanos: u64,
    pub(crate) kind: u16,
    pub(crate) payload_len: usize,
    pub(crate) payload_checksum_lo: u32,
}

fn header_checksum(bytes: &[u8; AUDIT_RECORD_HEADER_LEN]) -> u32 {
    xxhash_rust::xxh3::xxh3_64(&bytes[HEADER_CHECKSUM_COVERAGE]) as u32
}

/// Verify the record header's own integrity, before any field is used as a
/// length or an offset.
pub(crate) fn verify_prefix_checksum(bytes: &[u8; AUDIT_RECORD_HEADER_LEN]) -> bool {
    let stored = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
    stored == header_checksum(bytes)
}

/// Decode an already-authenticated record header.
pub(crate) fn decode_record_header(bytes: &[u8; AUDIT_RECORD_HEADER_LEN]) -> RecordHeader {
    RecordHeader {
        recorded_at_unix_nanos: u64::from_le_bytes(bytes[4..12].try_into().expect("8 bytes")),
        kind: u16::from_le_bytes([bytes[12], bytes[13]]),
        // bytes[14..16) reserved — covered by the checksum, ignored on read.
        payload_len: u32::from_le_bytes(bytes[16..20].try_into().expect("4 bytes")) as usize,
        payload_checksum_lo: u32::from_le_bytes(bytes[20..24].try_into().expect("4 bytes")),
    }
}

/// One durable audit record.
///
/// `recorded_at_unix_nanos` is a caller-supplied wall-clock stamp (nanoseconds
/// since the Unix epoch) used for age-based retention and ordering; this crate
/// never reads the system clock itself. `kind` tags the payload's schema (see
/// [`AUDIT_KIND_RESERVED_0`]); `payload` is opaque to this layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditRecord {
    /// Wall-clock append time, nanoseconds since the Unix epoch (caller-stamped).
    pub recorded_at_unix_nanos: u64,
    /// Payload schema tag.
    pub kind: u16,
    /// Opaque event bytes.
    pub payload: Vec<u8>,
}

/// Typed retention policy for the audit log.
///
/// Both constraints are **conjunctive**: a record is retained only if it is
/// among the newest [`keep_n_events`](Self::keep_n_events) *and* not older than
/// [`max_age`](Self::max_age). `None` disables a constraint. The default retains
/// everything — engine audit events are sparse, so unbounded growth is the
/// safe default and the embedder opts into trimming.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AuditRetentionPolicy {
    /// Maximum records to retain (newest kept). `None` = unbounded.
    pub keep_n_events: Option<u64>,
    /// Maximum record age relative to the prune's `now`. `None` = no age limit.
    pub max_age: Option<Duration>,
}

/// What an [`AuditLog::prune`] retained and removed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuditPruneOutcome {
    /// Number of records retained.
    pub retained: u64,
    /// Number of records removed.
    pub removed: u64,
    /// Bytes reclaimed by the rewrite.
    pub bytes_reclaimed: u64,
}

/// Append-only audit-log writer.
///
/// Holds an exclusive append handle positioned at the durable tail. Construct
/// with [`AuditLog::open`]; append with [`AuditLog::append`]; read the full
/// history with [`AuditLog::read_all`]; trim with [`AuditLog::prune`].
#[derive(Debug)]
pub struct AuditLog {
    file: File,
    path: PathBuf,
    authority: crate::StoreWriter,
    fenced: bool,
}

impl AuditLog {
    /// Existing writer lease for deliberate composition with a WAL.
    #[must_use]
    pub fn authority(&self) -> &crate::StoreWriter {
        &self.authority
    }

    /// Open (creating if absent) the audit log at `path`, repairing a torn
    /// final record and positioning for append.
    ///
    /// # Errors
    ///
    /// Returns I/O errors, [`PersistError::MagicMismatch`] /
    /// [`PersistError::UnsupportedVersion`] / [`PersistError::ReservedBytesNonZero`]
    /// for a corrupt file header, and
    /// [`PersistError::AuditMidLogCorruption`] when a record fails validation
    /// with records after it. The refusal leaves the file untouched, so the
    /// surviving records and the evidence needed to recover them by other means
    /// are still there on the next open.
    pub fn open(path: &Path) -> PersistResult<Self> {
        let (dir, name) = crate::StoreDirectory::for_file(path)?;
        crate::store_directory::validate_data_name(&name)?;
        let authority = crate::StoreWriter::acquire(&dir)?;
        Self::open_with_authority(&authority, &name)
    }

    /// Attach audit to an explicitly shared store-writer lease (for example a WAL's).
    ///
    /// # Errors
    /// Returns protocol, managed-name/file, contention, I/O, or decoding errors.
    pub fn open_with_authority(authority: &crate::StoreWriter, name: &Path) -> PersistResult<Self> {
        crate::store_directory::validate_data_name(name)?;
        let dir = authority.directory();
        dir.require_legacy()?;
        let mut file = dir.open_or_create(name)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(PersistError::WriterLockHeld),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }

        let file_len = file.metadata()?.len();
        if file_len == 0 {
            write_file_header(&mut file)?;
            file.sync_all()?;
            dir.sync()?;
        } else {
            verify_file_header(&mut file)?;
            let durable_end = scan_durable_end(&mut file, file_len)?;
            if durable_end < file_len {
                // Truncating the torn tail is idempotent (a re-open re-scans and
                // re-truncates), but fsync it so the reclaimed length is durable
                // immediately rather than relying on the next append's flush.
                file.set_len(durable_end)?;
                file.sync_all()?;
            }
        }
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            path: name.to_path_buf(),
            authority: authority.clone(),
            fenced: false,
        })
    }

    /// Append `record` and fsync it durable.
    ///
    /// Lifecycle events are infrequent, so each append fsyncs (data + size) —
    /// the audit trail is durable the instant the call returns. Append is the
    /// only growth path, so a crash can tear at most this final record, which
    /// [`Self::open`] discards on the next start.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::PayloadTooLarge`] if the payload exceeds
    /// [`MAX_AUDIT_PAYLOAD_BYTES`], or I/O errors from the write / fsync.
    pub fn append(&mut self, record: &AuditRecord) -> PersistResult<()> {
        self.ensure_usable()?;
        let bytes = encode_record(record)?;
        self.file.write_all(&bytes)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Read every durable record in `path`, oldest first.
    ///
    /// A static reader (does not require an open [`AuditLog`]); stops at the
    /// durable tail exactly as [`Self::open`] would, so a torn trailing record
    /// is never surfaced. Returns an empty vector for an absent or
    /// header-only file.
    ///
    /// # Errors
    ///
    /// Returns I/O / header-validation errors, and
    /// [`PersistError::AuditMidLogCorruption`] for interior damage. A reader
    /// must not quietly return the prefix before the damage — that is the
    /// shape that made the truncation on open look successful.
    pub fn read_all(path: &Path) -> PersistResult<Vec<AuditRecord>> {
        let (dir, name) = crate::StoreDirectory::for_file(path)?;
        Self::read_all_in(&dir, &name)
    }

    /// Read audit through retained directory authority without writer ownership.
    ///
    /// # Errors
    /// Returns managed-file, I/O, or audit validation errors.
    pub fn read_all_in(
        dir: &crate::StoreDirectory,
        name: &Path,
    ) -> PersistResult<Vec<AuditRecord>> {
        dir.require_legacy()?;
        // Streams record-by-record over the file handle rather than slurping the
        // whole log into memory first: a long-lived log under unbounded retention
        // can be large, and peak memory should stay at the decoded records plus a
        // single payload (the slice twin [`Self::decode_all`] is for fuzzing the
        // decoder on an already-in-memory buffer, not for reading real files).
        let mut file = match dir.open_read(name) {
            Ok(file) => file,
            Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let file_len = file.metadata()?.len();
        if file_len == 0 {
            return Ok(Vec::new());
        }
        verify_file_header(&mut file)?;
        read_records(&mut file, file_len)
    }

    /// Decode every durable record from an in-memory audit-log byte slice,
    /// oldest first — the pure-slice twin of [`Self::read_all`] used by the
    /// decoder fuzz target (it feeds bytes directly, no file handle).
    ///
    /// Validates the 8-byte file header, then scans records, stopping at the
    /// durable tail exactly as [`Self::read_all`] / [`Self::open`] would: a
    /// short, over-cap, or checksum-failed trailing record is treated as a torn
    /// tail and silently truncated, never surfaced and never a panic. An empty
    /// slice (an absent or zero-length file) decodes to an empty vector.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::MagicMismatch`] / [`PersistError::UnsupportedVersion`]
    /// / [`PersistError::ReservedBytesNonZero`] for a corrupt file header.
    pub fn decode_all(bytes: &[u8]) -> PersistResult<Vec<AuditRecord>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        verify_file_header_bytes(bytes)?;
        read_records_bytes(bytes)
    }

    /// Prune the log per `policy`, where `now_unix_nanos` is the reference time
    /// for [`AuditRetentionPolicy::max_age`].
    ///
    /// Reads all durable records, retains those satisfying both constraints, and
    /// atomically rewrites the log (write tmp → fsync → rename → dir fsync) so a
    /// crash mid-prune leaves the prior log intact. Records are sparse, so a full
    /// rewrite is cheap. The caller supplies `now_unix_nanos` (this crate is
    /// clock-free); pass the same epoch base used when stamping
    /// [`AuditRecord::recorded_at_unix_nanos`].
    ///
    /// # Errors
    ///
    /// Returns I/O / header errors from the read or the atomic rewrite.
    pub fn prune(
        &mut self,
        policy: &AuditRetentionPolicy,
        now_unix_nanos: u64,
    ) -> PersistResult<AuditPruneOutcome> {
        self.ensure_usable()?;
        let epoch = crate::manifest_lock::ManifestEpochGuard::acquire(&self.authority)?;
        self.file.sync_data()?;
        let dir = self.authority.directory();
        let all = Self::read_all_in(dir, &self.path)?;
        let total = all.len() as u64;
        let retained = select_retained(all, policy, now_unix_nanos);
        let retained_len = retained.len() as u64;
        if retained_len == total {
            return Ok(AuditPruneOutcome {
                retained: retained_len,
                removed: 0,
                bytes_reclaimed: 0,
            });
        }

        let size_before = self.file.metadata()?.len();
        let mut file = match rewrite_atomic(&epoch, &self.path, &retained) {
            Ok(file) => file,
            Err(error) => {
                if matches!(
                    error,
                    PersistError::Directory(crate::DirectoryError::PublicationUncertain { .. })
                ) {
                    self.fenced = true;
                }
                return Err(error);
            }
        };
        file.seek(SeekFrom::End(0))?;
        let size_after = file.metadata()?.len();
        self.file = file;
        Ok(AuditPruneOutcome {
            retained: retained_len,
            removed: total - retained_len,
            bytes_reclaimed: size_before.saturating_sub(size_after),
        })
    }

    fn ensure_usable(&self) -> PersistResult<()> {
        if self.fenced {
            return Err(crate::DirectoryError::RequiresReopen.into());
        }
        Ok(())
    }
}

/// Retain records satisfying both conjunctive constraints (newest-`keep_n`,
/// not-older-than-`max_age`), preserving oldest-first order.
fn select_retained(
    mut records: Vec<AuditRecord>,
    policy: &AuditRetentionPolicy,
    now_unix_nanos: u64,
) -> Vec<AuditRecord> {
    if let Some(max_age) = policy.max_age {
        let cutoff = now_unix_nanos.saturating_sub(nanos_from_duration(max_age));
        records.retain(|r| r.recorded_at_unix_nanos >= cutoff);
    }
    if let Some(keep_n) = policy.keep_n_events {
        let keep_n = keep_n as usize;
        if records.len() > keep_n {
            // Drop the oldest overflow (records are oldest-first).
            records.drain(0..records.len() - keep_n);
        }
    }
    records
}

/// Saturating conversion of a `Duration` to whole nanoseconds.
fn nanos_from_duration(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn write_file_header(file: &mut File) -> PersistResult<()> {
    let mut header = [0_u8; AUDIT_FILE_HEADER_LEN];
    header[0..4].copy_from_slice(&AUDIT_MAGIC);
    header[4..6].copy_from_slice(&AUDIT_FORMAT_VERSION.to_le_bytes());
    // reserved [6..8) stays zero.
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;
    Ok(())
}

fn verify_file_header(file: &mut File) -> PersistResult<()> {
    let mut header = [0_u8; AUDIT_FILE_HEADER_LEN];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)
        .map_err(|_| PersistError::TruncatedFileHeader)?;
    verify_file_header_bytes(&header)
}

/// Validate the 8-byte audit file header at the front of `bytes` — the shared
/// core of [`verify_file_header`] and [`AuditLog::decode_all`].
fn verify_file_header_bytes(bytes: &[u8]) -> PersistResult<()> {
    let header = bytes
        .get(..AUDIT_FILE_HEADER_LEN)
        .ok_or(PersistError::TruncatedFileHeader)?;
    let observed = [header[0], header[1], header[2], header[3]];
    if observed != AUDIT_MAGIC {
        return Err(PersistError::MagicMismatch { observed });
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != AUDIT_FORMAT_VERSION {
        return Err(PersistError::UnsupportedVersion {
            artifact: PersistArtifact::AuditLog,
            major: version,
            minor: 0,
        });
    }
    if u16::from_le_bytes([header[6], header[7]]) != 0 {
        return Err(PersistError::ReservedBytesNonZero { offset: 6 });
    }
    Ok(())
}

fn encode_record(record: &AuditRecord) -> PersistResult<Vec<u8>> {
    if record.payload.len() > MAX_AUDIT_PAYLOAD_BYTES {
        return Err(PersistError::PayloadTooLarge {
            len: record.payload.len(),
            max: MAX_AUDIT_PAYLOAD_BYTES,
        });
    }
    let payload_len = record.payload.len() as u32;
    let checksum_lo = xxhash_rust::xxh3::xxh3_64(&record.payload) as u32;
    let mut header = [0_u8; AUDIT_RECORD_HEADER_LEN];
    header[4..12].copy_from_slice(&record.recorded_at_unix_nanos.to_le_bytes());
    header[12..14].copy_from_slice(&record.kind.to_le_bytes());
    // header[14..16) reserved, left zero and covered by the checksum.
    header[16..20].copy_from_slice(&payload_len.to_le_bytes());
    header[20..24].copy_from_slice(&checksum_lo.to_le_bytes());
    // Written last: its coverage is the suffix it now authenticates.
    let prefix = header_checksum(&header);
    header[0..4].copy_from_slice(&prefix.to_le_bytes());

    let mut out = Vec::with_capacity(AUDIT_RECORD_HEADER_LEN + record.payload.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&record.payload);
    Ok(out)
}

/// Atomically rewrite `path` to hold exactly `records` (file header + each
/// record), via write-tmp → fsync → rename → dir fsync.
fn rewrite_atomic(
    guard: &crate::manifest_lock::ManifestEpochGuard,
    path: &Path,
    records: &[AuditRecord],
) -> PersistResult<File> {
    let dir = guard.directory();
    // Append `.tmp` to the *full* file name (matching the snapshot/manifest tmp
    // convention) rather than replacing the extension — `with_extension("log.tmp")`
    // would turn `events.dat` into `events.log.tmp`, dropping the real extension.
    let tmp_path = match path.file_name() {
        Some(name) => {
            let mut tmp_name = name.to_os_string();
            tmp_name.push(".tmp");
            PathBuf::from(tmp_name)
        }
        None => return Err(crate::DirectoryError::InvalidName(path.into()).into()),
    };

    let mut published = false;
    let result = (|| -> PersistResult<File> {
        let mut tmp = dir.open_or_create(&tmp_path)?;
        match tmp.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(PersistError::WriterLockHeld),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        tmp.set_len(0)?;
        write_file_header(&mut tmp)?;
        tmp.seek(SeekFrom::End(0))?;
        for record in records {
            tmp.write_all(&encode_record(record)?)?;
        }
        tmp.sync_all()?;
        published = true;
        dir.rename(&tmp_path, path)?;
        dir.check_fault("audit.after_replace")?;
        dir.sync()?;
        Ok(tmp)
    })();

    if result.is_err() {
        let _ = dir.remove(&tmp_path);
    }
    result.map_err(|error| {
        if published {
            crate::DirectoryError::PublicationUncertain {
                source: Box::new(error),
            }
            .into()
        } else {
            error
        }
    })
}

mod scan;

use scan::{read_records, read_records_bytes, scan_durable_end};

#[cfg(test)]
mod tests;
