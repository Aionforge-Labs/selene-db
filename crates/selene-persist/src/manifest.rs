//! MANIFEST epoch descriptor: the crash-safe linearization point for rotation.
//!
//! The MANIFEST is a small, fixed-layout, atomically-rewritten file naming the
//! single *live* persistence epoch: which snapshot sequence is authoritative,
//! which WAL file is active, and which WAL archives exist. It is **not** an
//! append-only edit log — every rotation rewrites it wholesale.
//!
//! # Why a MANIFEST at all
//!
//! Per the 2026-05-26 deletion+reclamation audit (Item 2 / Seam F), snapshot
//! finalize and WAL rotate were two separate, non-atomic embedder calls. A
//! crash between them left `snapshot.N.snap` durable on disk while `wal.log`'s
//! header still named epoch `N-1`, and recovery's `find_latest_snapshot` would
//! pick `N` and then hard-fail with `WalSnapshotMismatch`. The MANIFEST closes
//! that window: its atomic rename is the **single commit point**. Everything
//! before the rename is non-destructive (the previous epoch stays fully
//! recoverable); everything after is redundant cleanup. Recovery reads the
//! MANIFEST first and treats `live_snapshot_seq` as the source of truth, so an
//! orphan `snapshot.{>live}.snap` is ignored by construction.
//!
//! # Format (`format_version = 1`)
//!
//! The byte layout mirrors the crate's "hand-rolled fixed LE header, blake3
//! body hash, postcard for variable payloads" precedent (see
//! [`crate::file_header::WalFileHeader`] and
//! [`crate::snapshot_file_header::SnapshotFileHeader`]):
//!
//! ```text
//! fixed header (hand-rolled LE):
//!   [0..4)  magic     = b"SLMF"
//!   [4..6)  format_version: u16 LE = 1
//!   [6..8)  reserved:       u16 LE = 0   (zero-checked on read)
//! body (the hashed region):
//!   live_snapshot_seq:     u64 LE
//!   active_wal_header_seq:  u64 LE
//!   compaction_epoch:       u64 LE       (RESERVED, always 0 this brief)
//!   retention_present:       u8 = 0      (RESERVED None placeholder)
//!   active_wal:             u16 LE len + UTF-8 bytes
//!   archived_wal_seqs:      postcard-encoded Vec<u64> (ascending)
//! trailer:
//!   body_hash: [u8; 16]  = blake3-low-128 over the BODY region only
//! ```
//!
//! Format breaks are free pre-v1.x (audit note); Item 4c grows the reserved
//! `compaction_epoch` field under a bumped `format_version`. Item 5 (D26)
//! retention deliberately leaves `retention_present` reserved: the
//! [`RetentionPolicy`](crate::RetentionPolicy) is embedder configuration, not
//! persisted state, so prune only shrinks `archived_wal_seqs` (it never records
//! a policy here). Keeping a second copy of the policy in the MANIFEST would be
//! the dual-source divergence the "single writer per concern" lesson rules out.

use std::io::Write;
use std::path::Path;

use crate::manifest_lock::ManifestEpochGuard;
use crate::{PersistArtifact, PersistError, PersistResult};

/// MANIFEST file magic.
pub const MANIFEST_MAGIC: [u8; 4] = *b"SLMF";
/// MANIFEST format version understood by this build.
pub const MANIFEST_FORMAT_VERSION: u16 = 1;
/// Committed MANIFEST file name (sibling tmp is [`MANIFEST_TMP_FILE_NAME`]).
pub const MANIFEST_FILE_NAME: &str = "MANIFEST";
/// Transient MANIFEST file name used by the write-then-rename publish step.
pub const MANIFEST_TMP_FILE_NAME: &str = "MANIFEST.tmp";

/// Fixed MANIFEST header length: magic + format_version + reserved.
const MANIFEST_HEADER_LEN: usize = 8;
/// Fixed scalar prefix of the body: three `u64` + one `u8` retention marker.
const MANIFEST_BODY_FIXED_LEN: usize = 8 + 8 + 8 + 1;
/// Trailing blake3-low-128 body hash length.
const MANIFEST_BODY_HASH_LEN: usize = 16;

/// Live persistence epoch descriptor.
///
/// One [`Manifest`] names exactly one authoritative epoch. Recovery reads it
/// first and applies the snapshot named by [`Manifest::live_snapshot_seq`]
/// directly (not by scanning for the highest on-disk snapshot), so an orphan
/// snapshot left by a crashed rotation is ignored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Manifest {
    /// Snapshot sequence that is authoritative for this epoch.
    ///
    /// `0` means "no snapshot for this epoch" (a WAL-only / empty epoch);
    /// recovery applies no snapshot and replays the WAL from the start.
    pub live_snapshot_seq: u64,
    /// Snapshot sequence the active WAL header carries (or will carry after the
    /// rotation's Phase-4 reset completes).
    ///
    /// May legitimately lag `live_snapshot_seq` by one epoch after a
    /// Phase-3-before-Phase-4 crash; recovery does not cross-check it.
    pub active_wal_header_seq: u64,
    /// Reserved for Item-4c compaction epochs. Always `0` in this format.
    pub compaction_epoch: u64,
    /// Active WAL file name (currently always [`crate::DEFAULT_WAL_FILE_NAME`]).
    pub active_wal: String,
    /// Sequences whose WAL archives (`wal.{seq}.archive`) are retained,
    /// ascending. Item-5 retention prunes from this set MANIFEST-atomically.
    pub archived_wal_seqs: Vec<u64>,
}

impl Manifest {
    /// Encode this manifest into its fixed-layout byte representation.
    ///
    /// The returned bytes are header + body + blake3-low-128(body), exactly the
    /// shape [`Self::decode`] consumes.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::PayloadCodec`] if the `archived_wal_seqs` vector
    /// fails postcard encoding (effectively unreachable for `Vec<u64>`).
    pub fn encode(&self) -> PersistResult<Vec<u8>> {
        let archived = postcard::to_allocvec(&self.archived_wal_seqs)
            .map_err(|error| PersistError::PayloadCodec(error.to_string()))?;
        let active_wal_bytes = self.active_wal.as_bytes();

        let mut body = Vec::with_capacity(
            MANIFEST_BODY_FIXED_LEN + 2 + active_wal_bytes.len() + archived.len(),
        );
        body.extend_from_slice(&self.live_snapshot_seq.to_le_bytes());
        body.extend_from_slice(&self.active_wal_header_seq.to_le_bytes());
        body.extend_from_slice(&self.compaction_epoch.to_le_bytes());
        body.push(0_u8); // retention_present: reserved (Item-5 keeps policy in embedder config)
        let name_len = u16::try_from(active_wal_bytes.len()).map_err(|_| {
            PersistError::PayloadCodec("active_wal name exceeds u16 length".to_owned())
        })?;
        body.extend_from_slice(&name_len.to_le_bytes());
        body.extend_from_slice(active_wal_bytes);
        body.extend_from_slice(&archived);

        let hash = body_hash(&body);

        let mut out = Vec::with_capacity(MANIFEST_HEADER_LEN + body.len() + MANIFEST_BODY_HASH_LEN);
        out.extend_from_slice(&MANIFEST_MAGIC);
        out.extend_from_slice(&MANIFEST_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes()); // reserved
        out.extend_from_slice(&body);
        out.extend_from_slice(&hash);
        Ok(out)
    }

    /// Decode a manifest from its fixed-layout byte representation.
    ///
    /// Validates magic, format version, reserved zero, and recomputes the body
    /// hash. The atomic-rename publish guarantees a committed MANIFEST is never
    /// torn, so any truncation/corruption here is a hard error.
    ///
    /// # Errors
    ///
    /// - [`PersistError::TruncatedFileHeader`] for a buffer too short to hold
    ///   the header, fixed body prefix, or trailing hash.
    /// - [`PersistError::MagicMismatch`] for a wrong magic.
    /// - [`PersistError::UnsupportedVersion`] for an unknown format version.
    /// - [`PersistError::ReservedBytesNonZero`] for nonzero reserved bytes.
    /// - [`PersistError::BodyHashMismatch`] for a hash mismatch.
    /// - [`PersistError::PayloadCodec`] for a malformed body tail.
    pub fn decode(bytes: &[u8]) -> PersistResult<Self> {
        if bytes.len() < MANIFEST_HEADER_LEN + MANIFEST_BODY_HASH_LEN {
            return Err(PersistError::TruncatedFileHeader);
        }
        let observed = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if observed != MANIFEST_MAGIC {
            return Err(PersistError::MagicMismatch { observed });
        }
        let format_version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if format_version != MANIFEST_FORMAT_VERSION {
            return Err(PersistError::UnsupportedVersion {
                artifact: PersistArtifact::Manifest,
                major: format_version,
                minor: 0,
            });
        }
        let reserved = u16::from_le_bytes([bytes[6], bytes[7]]);
        if reserved != 0 {
            return Err(PersistError::ReservedBytesNonZero { offset: 6 });
        }

        let hash_start = bytes.len() - MANIFEST_BODY_HASH_LEN;
        let body = &bytes[MANIFEST_HEADER_LEN..hash_start];
        let stored_hash = &bytes[hash_start..];
        let computed = body_hash(body);
        if computed != stored_hash {
            let mut expected = [0_u8; MANIFEST_BODY_HASH_LEN];
            expected.copy_from_slice(stored_hash);
            return Err(PersistError::BodyHashMismatch {
                expected,
                observed: computed,
            });
        }

        decode_body(body)
    }

    /// Atomically publish this manifest into `dir`.
    ///
    /// Writes `MANIFEST.tmp`, fsyncs it (data + metadata — a brand-new file's
    /// size must be durable before the rename can name it), atomically renames
    /// it onto `MANIFEST`, then fsyncs the parent directory so the new directory
    /// entry is durable. Publication holds the directory's persistent
    /// [`crate::MANIFEST_LOCK_FILE_NAME`] lock so it cannot clobber a concurrent
    /// rotation or prune temporary and cannot replace the epoch beneath a
    /// [`crate::PersistenceReadGuard`]. Callers constructing a replacement from
    /// a prior read must still provide their own semantic freshness policy; the
    /// lock serializes publication but is not a compare-and-swap operation.
    ///
    /// # Why this is the commit point
    ///
    /// Per the MANIFEST commit-point invariant (audit Item 2 / Seam F), the
    /// rename is the instant the named epoch becomes authoritative. POSIX
    /// `rename(2)` is atomic and overwrites the prior `MANIFEST` in place, so a
    /// crash either leaves the previous MANIFEST fully intact or the new one
    /// fully committed — never a torn live read. Unlike the snapshot/archive
    /// publish (which use `hard_link` with fail-on-collision because each
    /// sequence is written once), the MANIFEST is *rewritten every rotation*
    /// and therefore deliberately uses overwriting `rename`.
    ///
    /// # Errors
    ///
    /// Returns writer contention, I/O errors opening/acquiring the epoch lock, or tmp
    /// write, fsync, rename, or directory fsync. Targets whose filesystems do
    /// not support file locking return that underlying I/O error. A best-effort
    /// tmp cleanup runs on any pre-rename failure.
    pub fn write_atomic(&self, dir: &Path) -> PersistResult<()> {
        self.write_atomic_in(&crate::StoreDirectory::open(dir)?)
    }

    /// Acquire standalone writer ownership and publish through a retained directory.
    ///
    /// # Errors
    /// Returns directory, protocol, encoding, locking, or publication errors.
    pub fn write_atomic_in(&self, dir: &crate::StoreDirectory) -> PersistResult<()> {
        let authority = crate::StoreWriter::acquire(dir)?;
        self.write_atomic_with_authority(&authority)
    }

    /// Publish using an existing store writer lease, without reacquiring `LOCK`.
    ///
    /// # Errors
    /// Returns protocol, encoding, epoch-lock, or publication errors.
    pub fn write_atomic_with_authority(&self, authority: &crate::StoreWriter) -> PersistResult<()> {
        authority.directory().require_legacy()?;
        let mut guard = ManifestEpochGuard::acquire(authority)?;
        self.write_atomic_locked(&mut guard)
    }

    /// Publish while the caller holds the directory epoch lock.
    pub(crate) fn write_atomic_locked(&self, guard: &mut ManifestEpochGuard) -> PersistResult<()> {
        let dir = guard.directory();
        dir.require_legacy()?;
        crate::store_directory::validate_name(Path::new(&self.active_wal))?;
        let bytes = self.encode()?;
        let tmp_path = Path::new(MANIFEST_TMP_FILE_NAME);
        let final_path = Path::new(MANIFEST_FILE_NAME);

        let result = (|| -> PersistResult<()> {
            // create + truncate: the tmp is transient, overwriting a stale tmp
            // from a crashed prior rotation is correct.
            let mut file = dir.open_or_create(tmp_path)?;
            file.set_len(0)?;
            file.write_all(&bytes)?;
            // sync_all (data + metadata): the new file's size must be durable
            // before rename publishes its name.
            file.sync_all()?;
            drop(file);
            dir.rename(tmp_path, final_path)?;
            Ok(())
        })();

        if result.is_err() {
            let _ = dir.remove(tmp_path);
            return result;
        }
        // Parent-dir fsync AFTER the rename makes the new directory entry
        // durable. Per the MANIFEST commit-point invariant this must follow,
        // never precede, the rename.
        dir.sync()
    }

    /// Read the committed manifest from `dir`, if present.
    ///
    /// Returns `Ok(None)` when no `MANIFEST` exists (a fresh or pre-MANIFEST
    /// directory). A leftover `MANIFEST.tmp` is never consulted — only the
    /// committed `MANIFEST` is read.
    ///
    /// This is an atomic metadata read, not a lease on the snapshot, WAL, or
    /// archive paths the returned manifest names. Callers that will open or
    /// copy those artifacts must first acquire a
    /// [`crate::PersistenceReadGuard`] and retain it through artifact use.
    ///
    /// # Errors
    ///
    /// Returns I/O errors reading the file, or any decode error
    /// ([`Self::decode`]) — a committed MANIFEST is never torn, so corruption
    /// is fatal rather than silently ignored.
    pub fn read(dir: &Path) -> PersistResult<Option<Self>> {
        Self::read_in(&crate::StoreDirectory::open(dir)?)
    }

    /// Read legacy MANIFEST through a retained capability (not an artifact lease).
    ///
    /// # Errors
    /// Returns protocol, managed-file, I/O, or decoding errors.
    pub fn read_in(dir: &crate::StoreDirectory) -> PersistResult<Option<Self>> {
        use std::io::Read;
        dir.require_legacy()?;
        match dir.open_read(MANIFEST_FILE_NAME) {
            Ok(mut file) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                Ok(Some(Self::decode(&bytes)?))
            }
            Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

fn decode_body(body: &[u8]) -> PersistResult<Manifest> {
    // Fixed prefix + the 2-byte name length must be present.
    if body.len() < MANIFEST_BODY_FIXED_LEN + 2 {
        return Err(PersistError::TruncatedFileHeader);
    }
    // Every field read goes through a bounds-checked window reader so the
    // decoder cannot panic on a short body regardless of how the `len < 27`
    // guard above is later refactored — the `expect`s inside the helpers are on
    // exact-width windows and are genuinely infallible.
    let live_snapshot_seq = le_u64(body, 0)?;
    let active_wal_header_seq = le_u64(body, 8)?;
    let compaction_epoch = le_u64(body, 16)?;
    let retention_present = byte_at(body, 24)?;
    if retention_present != 0 {
        // Reserved None placeholder (Item-5); a nonzero marker is a format we
        // do not understand in this version.
        return Err(PersistError::ReservedBytesNonZero { offset: 24 });
    }

    let name_len = le_u16(body, 25)? as usize;
    let name_start = MANIFEST_BODY_FIXED_LEN + 2;
    let name_end = name_start
        .checked_add(name_len)
        .ok_or(PersistError::TruncatedFileHeader)?;
    if body.len() < name_end {
        return Err(PersistError::TruncatedFileHeader);
    }
    let active_wal = std::str::from_utf8(&body[name_start..name_end])
        .map_err(|error| PersistError::PayloadCodec(error.to_string()))?
        .to_owned();

    // No explicit count cap here: postcard + serde already clamp the speculative
    // sequence preallocation (`de::size_hint::cautious`) and the `Vec<u64>` only
    // grows as real input bytes are consumed (>= 1 postcard byte per element), so
    // a crafted length prefix yields a prompt `PayloadCodec` error, not a runaway
    // allocation. A decode-side cap was deliberately NOT added: it would make
    // decode reject a manifest the writer can legitimately produce (a long-lived
    // unpruned archive set), bricking the directory on the next read — decode must
    // accept everything `encode` can emit.
    let archived_wal_seqs: Vec<u64> = postcard::from_bytes(&body[name_end..])
        .map_err(|error| PersistError::PayloadCodec(error.to_string()))?;

    Ok(Manifest {
        live_snapshot_seq,
        active_wal_header_seq,
        compaction_epoch,
        active_wal,
        archived_wal_seqs,
    })
}

/// Read a little-endian `u64` at `at`, or [`PersistError::TruncatedFileHeader`]
/// if the 8-byte window runs past the slice (overflow-safe in `at`).
fn le_u64(bytes: &[u8], at: usize) -> PersistResult<u64> {
    let end = at.checked_add(8).ok_or(PersistError::TruncatedFileHeader)?;
    let window = bytes
        .get(at..end)
        .ok_or(PersistError::TruncatedFileHeader)?;
    Ok(u64::from_le_bytes(
        window.try_into().expect("8-byte window"),
    ))
}

/// Read a little-endian `u16` at `at`, or [`PersistError::TruncatedFileHeader`]
/// if the 2-byte window runs past the slice (overflow-safe in `at`).
fn le_u16(bytes: &[u8], at: usize) -> PersistResult<u16> {
    let end = at.checked_add(2).ok_or(PersistError::TruncatedFileHeader)?;
    let window = bytes
        .get(at..end)
        .ok_or(PersistError::TruncatedFileHeader)?;
    Ok(u16::from_le_bytes(
        window.try_into().expect("2-byte window"),
    ))
}

/// Read a single byte at `at`, or [`PersistError::TruncatedFileHeader`].
fn byte_at(bytes: &[u8], at: usize) -> PersistResult<u8> {
    bytes
        .get(at)
        .copied()
        .ok_or(PersistError::TruncatedFileHeader)
}

/// Compute the blake3-low-128 body hash, mirroring [`crate::section::body_hash`].
fn body_hash(body: &[u8]) -> [u8; MANIFEST_BODY_HASH_LEN] {
    let digest = blake3::hash(body);
    let mut hash = [0_u8; MANIFEST_BODY_HASH_LEN];
    hash.copy_from_slice(&digest.as_bytes()[..MANIFEST_BODY_HASH_LEN]);
    hash
}

/// Fsync a directory so a preceding rename/hard_link is durably ordered.
///
/// This is the std parent-directory-fsync idiom: open the directory and
/// `sync_all`. Per the MANIFEST commit-point invariant the data file's own
/// `sync_data`/`sync_all` must precede its rename, and this directory fsync must
/// *follow* the rename — never the reverse — so the new directory entry is
/// durable.
///
/// # Errors
///
/// Returns I/O errors from opening or syncing the directory.
pub fn sync_dir(dir: &Path) -> PersistResult<()> {
    crate::StoreDirectory::open(dir)?.sync()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "selene-manifest-{name}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        dir
    }

    fn sample() -> Manifest {
        Manifest {
            live_snapshot_seq: 42,
            active_wal_header_seq: 42,
            compaction_epoch: 0,
            active_wal: crate::DEFAULT_WAL_FILE_NAME.to_owned(),
            archived_wal_seqs: vec![7, 19, 41],
        }
    }

    #[test]
    fn encode_decode_round_trips() {
        let manifest = sample();
        let bytes = manifest.encode().unwrap();
        assert_eq!(&bytes[0..4], &MANIFEST_MAGIC);
        let decoded = Manifest::decode(&bytes).unwrap();
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn empty_archive_list_round_trips() {
        let manifest = Manifest {
            live_snapshot_seq: 0,
            active_wal_header_seq: 0,
            compaction_epoch: 0,
            active_wal: crate::DEFAULT_WAL_FILE_NAME.to_owned(),
            archived_wal_seqs: Vec::new(),
        };
        let bytes = manifest.encode().unwrap();
        assert_eq!(Manifest::decode(&bytes).unwrap(), manifest);
    }

    #[test]
    fn corrupt_magic_is_rejected() {
        let mut bytes = sample().encode().unwrap();
        bytes[0..4].copy_from_slice(b"NOPE");
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(PersistError::MagicMismatch { observed }) if observed == *b"NOPE"
        ));
    }

    #[test]
    fn unsupported_format_version_is_rejected() {
        let mut bytes = sample().encode().unwrap();
        bytes[4..6].copy_from_slice(&2_u16.to_le_bytes());
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(PersistError::UnsupportedVersion {
                artifact: PersistArtifact::Manifest,
                major: 2,
                minor: 0
            })
        ));
    }

    #[test]
    fn nonzero_reserved_is_rejected() {
        let mut bytes = sample().encode().unwrap();
        bytes[6] = 1;
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(PersistError::ReservedBytesNonZero { offset: 6 })
        ));
    }

    #[test]
    fn corrupt_body_hash_is_rejected() {
        let mut bytes = sample().encode().unwrap();
        *bytes.last_mut().unwrap() ^= 0x01;
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(PersistError::BodyHashMismatch { .. })
        ));
    }

    #[test]
    fn flipping_a_body_byte_is_caught_by_hash() {
        let mut bytes = sample().encode().unwrap();
        // Flip the first body byte (live_snapshot_seq low byte).
        bytes[MANIFEST_HEADER_LEN] ^= 0x01;
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(PersistError::BodyHashMismatch { .. })
        ));
    }

    #[test]
    fn truncated_buffer_is_rejected() {
        let bytes = sample().encode().unwrap();
        for len in [
            0_usize,
            4,
            8,
            MANIFEST_HEADER_LEN + MANIFEST_BODY_HASH_LEN - 1,
        ] {
            assert!(matches!(
                Manifest::decode(&bytes[..len.min(bytes.len())]),
                Err(PersistError::TruncatedFileHeader)
            ));
        }
    }

    /// Assemble valid header + matching body hash around an arbitrary body, so
    /// the decoder reaches `decode_body` instead of failing the hash check.
    fn frame(body: &[u8]) -> Vec<u8> {
        let hash = body_hash(body);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MANIFEST_MAGIC);
        bytes.extend_from_slice(&MANIFEST_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(&hash);
        bytes
    }

    #[test]
    fn large_archive_list_round_trips() {
        // A genuinely large (but valid) archive set must decode without a cap —
        // decode accepts everything the writer can emit, so a long-lived unpruned
        // directory never bricks itself on the next read.
        let manifest = Manifest {
            live_snapshot_seq: 1,
            active_wal_header_seq: 1,
            compaction_epoch: 0,
            active_wal: crate::DEFAULT_WAL_FILE_NAME.to_owned(),
            archived_wal_seqs: (0..100_000_u64).collect(),
        };
        assert_eq!(
            Manifest::decode(&manifest.encode().unwrap()).unwrap(),
            manifest
        );
    }

    #[test]
    fn crafted_huge_archive_count_varint_rejects_promptly() {
        // body = [24-byte fixed u64s][retention=0][name_len=0][postcard tail].
        // The tail is a Vec<u64> length varint claiming a huge count with zero
        // element bytes, so postcard hits EOF immediately -> PayloadCodec, no
        // multi-GB allocation or hang (postcard + serde bound the preallocation;
        // growth is bounded by the consumed input length).
        let mut body = Vec::new();
        body.extend_from_slice(&1_u64.to_le_bytes());
        body.extend_from_slice(&1_u64.to_le_bytes());
        body.extend_from_slice(&0_u64.to_le_bytes());
        body.push(0); // retention_present
        body.extend_from_slice(&0_u16.to_le_bytes()); // name_len = 0
        body.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0x7F]); // huge LEB128 count, no elements
        assert!(matches!(
            Manifest::decode(&frame(&body)),
            Err(PersistError::PayloadCodec(_))
        ));
    }

    #[test]
    fn body_one_byte_short_of_name_len_is_truncated_not_panic() {
        // A 26-byte body is one short of the 27 the name-length read needs.
        // Wrapped with a valid header + matching hash, decode must return
        // TruncatedFileHeader, never panic on the field reads.
        assert!(matches!(
            Manifest::decode(&frame(&[0_u8; 26])),
            Err(PersistError::TruncatedFileHeader)
        ));
    }

    #[test]
    fn write_atomic_then_read_round_trips() {
        let dir = temp_dir("write-read");
        let manifest = sample();
        manifest.write_atomic(&dir).unwrap();
        let read = Manifest::read(&dir).unwrap().unwrap();
        assert_eq!(read, manifest);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_absent_manifest_returns_none() {
        let dir = temp_dir("absent");
        assert!(Manifest::read(&dir).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_atomic_leaves_no_tmp_on_success() {
        let dir = temp_dir("no-tmp");
        sample().write_atomic(&dir).unwrap();
        assert!(!dir.join(MANIFEST_TMP_FILE_NAME).exists());
        assert!(dir.join(MANIFEST_FILE_NAME).exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_atomic_overwrites_prior_manifest() {
        let dir = temp_dir("overwrite");
        sample().write_atomic(&dir).unwrap();
        let next = Manifest {
            live_snapshot_seq: 99,
            active_wal_header_seq: 99,
            compaction_epoch: 0,
            active_wal: crate::DEFAULT_WAL_FILE_NAME.to_owned(),
            archived_wal_seqs: vec![42],
        };
        next.write_atomic(&dir).unwrap();
        assert_eq!(Manifest::read(&dir).unwrap().unwrap(), next);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn leftover_tmp_is_ignored_by_read() {
        let dir = temp_dir("leftover-tmp");
        let manifest = sample();
        manifest.write_atomic(&dir).unwrap();
        // Simulate a crashed rotation that left a partial tmp behind.
        fs::write(dir.join(MANIFEST_TMP_FILE_NAME), b"partial-garbage").unwrap();
        // read() only ever consults the committed MANIFEST.
        assert_eq!(Manifest::read(&dir).unwrap().unwrap(), manifest);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_committed_manifest_is_hard_error() {
        let dir = temp_dir("corrupt-committed");
        sample().write_atomic(&dir).unwrap();
        let path = dir.join(MANIFEST_FILE_NAME);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0x01;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            Manifest::read(&dir),
            Err(PersistError::BodyHashMismatch { .. })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sync_dir_succeeds_on_existing_dir() {
        let dir = temp_dir("sync");
        sync_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(dir);
    }
}
