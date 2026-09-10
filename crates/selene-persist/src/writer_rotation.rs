//! WAL rotation helpers, including the crash-safe multi-phase rotate
//! orchestrator whose MANIFEST commit is the rotation linearization point.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::file_header::{WAL_FILE_HEADER_LEN, WalFileHeader};
use crate::manifest::Manifest;
use crate::manifest_lock::ManifestEpochGuard;
use crate::snapshot_path::snapshot_path;
use crate::snapshot_writer::SnapshotBuilder;
use crate::{DEFAULT_WAL_FILE_NAME, PersistError, PersistResult, SnapshotReader, StoreDirectory};

/// Result of a successful WAL checkpoint operation.
///
/// A same-sequence retry after the MANIFEST and active WAL already reached the
/// requested epoch verifies the snapshot but does not synthesize a second WAL
/// archive from the header-only active file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WalRotationOutcome {
    /// This call completed a new rotation (or finished its pending reset).
    Rotated {
        /// Archive path containing the pre-rotation WAL.
        archived_path: PathBuf,
        /// Active WAL path reopened for post-snapshot appends.
        new_path: PathBuf,
        /// Last sequence contained by the archived WAL.
        archived_last_sequence: u64,
    },
    /// The requested snapshot epoch was already committed and active.
    AlreadyCurrent {
        /// Snapshot sequence already active in the WAL header and MANIFEST.
        snapshot_sequence: u64,
        /// Active WAL path that already extends `snapshot_sequence`.
        active_path: PathBuf,
    },
}

impl WalRotationOutcome {
    /// Return the checkpoint sequence covered by this outcome.
    #[must_use]
    pub const fn snapshot_sequence(&self) -> u64 {
        match self {
            Self::Rotated {
                archived_last_sequence,
                ..
            } => *archived_last_sequence,
            Self::AlreadyCurrent {
                snapshot_sequence, ..
            } => *snapshot_sequence,
        }
    }

    /// Return the active WAL path after the operation.
    #[must_use]
    pub fn active_path(&self) -> &Path {
        match self {
            Self::Rotated { new_path, .. } => new_path,
            Self::AlreadyCurrent { active_path, .. } => active_path,
        }
    }

    /// Return the archive path created or completed by this call, if any.
    #[must_use]
    pub fn archived_path(&self) -> Option<&Path> {
        match self {
            Self::Rotated { archived_path, .. } => Some(archived_path),
            Self::AlreadyCurrent { .. } => None,
        }
    }
}

/// Filename prefix shared by every archived WAL segment (`wal.{seq}.archive`).
pub const WAL_ARCHIVE_PREFIX: &str = "wal.";
/// Filename suffix shared by every archived WAL segment (`wal.{seq}.archive`).
pub const WAL_ARCHIVE_SUFFIX: &str = ".archive";

pub(crate) fn wal_archive_path(path: &Path, last_sequence: u64) -> PathBuf {
    let archive_name = format!("{WAL_ARCHIVE_PREFIX}{last_sequence}{WAL_ARCHIVE_SUFFIX}");
    path.parent().map_or_else(
        || PathBuf::from(&archive_name),
        |parent| parent.join(&archive_name),
    )
}

/// Parse `wal.{seq}.archive` into its sequence number.
///
/// Returns `None` for any name that does not match the durable-archive pattern,
/// including the in-flight `wal.{seq}.archive.tmp.{pid}.{n}` temporaries a
/// crashed rotation may leave behind (their trailing component is the attempt
/// counter, not `.archive`). Retention's directory scan relies on this so it
/// never mistakes a partial archive for a prunable durable one — the mirror of
/// [`crate::snapshot_path::parse_snapshot_filename`] for the WAL archive family.
#[must_use]
pub fn parse_wal_archive_filename(name: &std::ffi::OsStr) -> Option<u64> {
    let name = name.to_str()?;
    let seq = name
        .strip_prefix(WAL_ARCHIVE_PREFIX)?
        .strip_suffix(WAL_ARCHIVE_SUFFIX)?;
    if seq.is_empty() {
        return None;
    }
    seq.parse::<u64>().ok()
}

pub(crate) fn archive_current_wal(
    guard: &ManifestEpochGuard,
    file: &mut File,
    archived_path: &Path,
    committed_offset: u64,
) -> PersistResult<()> {
    let dir = guard.directory();
    let (tmp_path, mut archive) = create_archive_tmp(dir, archived_path)?;
    let result = (|| -> PersistResult<()> {
        file.seek(SeekFrom::Start(0))?;
        let copied = {
            let mut source = (&mut *file).take(committed_offset);
            std::io::copy(&mut source, &mut archive)?
        };
        if copied != committed_offset {
            return Err(PersistError::TruncatedEntry { offset: copied });
        }
        archive.sync_data()?;
        drop(archive);
        publish_archive_tmp(dir, &tmp_path, archived_path)
    })();
    let restored = file.seek(SeekFrom::Start(committed_offset));
    if result.is_err() {
        let _ = dir.remove(&tmp_path);
    }
    restored?;
    result
}

fn create_archive_tmp(
    dir: &StoreDirectory,
    archived_path: &Path,
) -> PersistResult<(PathBuf, File)> {
    for attempt in 0..128_u8 {
        let tmp_path =
            archived_path.with_extension(format!("archive.tmp.{}.{}", std::process::id(), attempt));
        match dir.create_new(&tmp_path) {
            Ok(file) => return Ok((tmp_path, file)),
            Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "wal archive temporary path attempts exhausted",
    )
    .into())
}

fn publish_archive_tmp(
    dir: &StoreDirectory,
    tmp_path: &Path,
    archived_path: &Path,
) -> PersistResult<()> {
    let published = match dir.hard_link(tmp_path, archived_path) {
        Ok(()) => true,
        Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            crate::artifact_identity::require_identical_regular_files(
                dir,
                tmp_path,
                archived_path,
            )?;
            false
        }
        Err(error) => return Err(error),
    };
    // The existing inode may have been linked by an attempt that crashed before
    // its directory barrier. Sync it again before accepting exact identity.
    if !published {
        dir.open_write(archived_path)?.sync_all()?;
    }
    let _ = dir.remove(tmp_path);
    // Parent-dir fsync AFTER the hard_link publish so the new archive directory
    // entry is durable. It is repeated for an identical existing archive in
    // case the earlier attempt crashed between link publication and this
    // barrier.
    dir.sync()?;
    Ok(())
}

pub(crate) fn reset_active_wal_file(
    guard: &ManifestEpochGuard,
    file: &mut File,
    wal_path: &Path,
    snapshot_seq: u64,
) -> PersistResult<()> {
    let dir = guard.directory();
    let (tmp_path, mut replacement) = create_reset_tmp(dir, wal_path)?;
    let mut renamed = false;
    let result = (|| -> PersistResult<()> {
        match replacement.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(PersistError::WriterLockHeld);
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        // Preserve the active WAL's access policy; replacement creation must
        // not silently widen a deliberately restrictive mode such as 0600.
        replacement.set_permissions(file.metadata()?.permissions())?;
        WalFileHeader::new(snapshot_seq).write_to(&mut replacement)?;
        replacement.sync_all()?;
        replacement.seek(SeekFrom::Start(WAL_FILE_HEADER_LEN as u64))?;
        #[cfg(test)]
        fail_reset_at(1)?;

        // Replacing the directory entry is the only destructive operation.
        // Before it, recovery sees the intact old WAL; after it, recovery sees
        // a fully written and synced new header. A crash before the following
        // directory fsync may retain either entry, and both are valid.
        dir.rename(&tmp_path, wal_path)?;
        renamed = true;
        #[cfg(test)]
        fail_reset_at(2)?;
        dir.sync()?;
        *file = replacement;
        Ok(())
    })();
    if result.is_err() && !renamed {
        let _ = dir.remove(&tmp_path);
    }
    result
}

fn create_reset_tmp(dir: &StoreDirectory, wal_path: &Path) -> PersistResult<(PathBuf, File)> {
    for attempt in 0..128_u8 {
        let mut name = wal_path
            .file_name()
            .expect("active WAL path always has a file name")
            .to_os_string();
        name.push(format!(".reset.tmp.{}.{attempt}", std::process::id()));
        let path = wal_path.with_file_name(name);
        match dir.create_new(&path) {
            Ok(file) => return Ok((path, file)),
            Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "active WAL reset temporary path attempts exhausted",
    )
    .into())
}

#[cfg(test)]
std::thread_local! {
    static RESET_FAULT_POINT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn fail_reset_at(point: u8) -> PersistResult<()> {
    RESET_FAULT_POINT.with(|configured| {
        if configured.get() == point {
            configured.set(0);
            Err(std::io::Error::other("injected active WAL reset failure").into())
        } else {
            Ok(())
        }
    })
}

/// In-memory state the [`crate::WalWriter`] adopts after a MANIFEST rotation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RotationCommitState {
    /// New high-water sequence (== the committed snapshot sequence).
    pub last_sequence: u64,
    /// New WAL-header snapshot sequence after the Phase-4 reset.
    pub snapshot_seq: u64,
    /// New committed offset (the post-reset header length).
    pub committed_offset: u64,
}

/// Per-rotation inputs the orchestrator needs from the live writer.
pub(crate) struct RotationInputs<'a> {
    /// The live WAL file handle.
    pub file: &'a mut File,
    /// The active WAL path (used to derive the archive path + parent dir).
    pub wal_path: &'a Path,
    /// Durable offset of the last fully committed WAL entry.
    pub committed_offset: u64,
    /// Writer high-water sequence; must equal the snapshot sequence.
    pub last_sequence: u64,
    /// Snapshot epoch recorded in the current active WAL header.
    pub snapshot_seq: u64,
    /// Committed MANIFEST observed before this rotation, if one exists.
    pub prior_manifest: Option<Manifest>,
}

/// Run the crash-safe 4-phase rotate, committing at the MANIFEST rename.
///
/// The MANIFEST write in Phase 3 is the single linearization / commit point for
/// the new epoch. A first-rotation baseline MANIFEST only makes the current
/// epoch authoritative; it does not advance it. Everything before Phase 3 is
/// non-destructive — the previous epoch stays fully recoverable — and
/// everything after it is redundant cleanup. The
/// required ordering for advancing to snapshot epoch `N`
/// (`builder.sequence == last_sequence == N`):
///
/// - **Baseline** (first rotation only): if no MANIFEST exists, validate any
///   non-zero snapshot named by the current WAL header and durably commit that
///   previous epoch before Phase 1. This prevents legacy highest-snapshot
///   recovery from adopting an orphan created by the first rotation.
/// - **Phase 1** (non-destructive): finalize `snapshot.N.snap` then
///   `sync_dir`. The old `snapshot.{N-1}.snap` and the live MANIFEST (still
///   naming `N-1`) are untouched.
/// - **Phase 2** (non-destructive): archive the committed WAL bytes to
///   `wal.{N}.archive` then `sync_dir`. The active `wal.log` is only seeked,
///   never truncated.
/// - **Phase 3** (**COMMIT**): `Manifest::write_atomic` names `live_snapshot_seq
///   = N`; the atomic rename is the instant `N` becomes authoritative.
/// - **Phase 4** (safe-after-commit replacement): write and sync a fresh WAL
///   header seeded with `N`, atomically rename it over `wal.log`, fsync the
///   directory, and only then swap the writer's live handle. Snapshot `N`
///   already captures the replaced `<= N` entries.
///
/// # Why the reset must follow the commit
///
/// Per the MANIFEST commit-point invariant (audit Item 2 / Seam F): if the WAL
/// reset replacement ran *before* the MANIFEST commit and a crash occurred,
/// recovery would read the OLD MANIFEST (epoch `N-1`) but find `wal.log` no
/// longer carrying the `N-1..N` entries — permanent committed-data loss. Hence
/// reset is strictly gated on the Phase-3 rename + dir fsync.
///
/// # Idempotent re-rotate
///
/// Re-invoking after a pre-commit crash converges only when the existing
/// snapshot and archive exactly match the newly written temporary files. If
/// the MANIFEST already committed `N` but the active WAL still carries the old
/// epoch, the retry verifies those artifacts and finishes Phase 4. If both the
/// MANIFEST and header already name `N`, it verifies the exact snapshot and
/// returns [`WalRotationOutcome::AlreadyCurrent`] without synthesizing an
/// archive from the header-only active WAL.
///
/// # Errors
///
/// Returns [`PersistError::WalRotationSequenceMismatch`] when `builder`'s
/// sequence does not equal `inputs.last_sequence`, plus any I/O / format error
/// from snapshot finalize, artifact identity checking, archive, MANIFEST
/// commit, or WAL reset. A failure before the MANIFEST advances leaves the
/// previous epoch fully recoverable; a failure in Phase 4 (after commit)
/// surfaces [`PersistError::WalRotationIncomplete`] but the new epoch is
/// already durable and recovery converges on re-open.
pub(crate) fn rotate_with_manifest(
    inputs: RotationInputs<'_>,
    builder: SnapshotBuilder,
    epoch_guard: &mut ManifestEpochGuard,
) -> PersistResult<(WalRotationOutcome, RotationCommitState)> {
    let cap = epoch_guard.directory().clone();
    let dir = &cap;
    let RotationInputs {
        file,
        wal_path,
        committed_offset,
        last_sequence,
        snapshot_seq: prior_snapshot_seq,
        prior_manifest,
    } = inputs;
    let wal_name = Path::new(DEFAULT_WAL_FILE_NAME);

    let snapshot_seq = builder.sequence();
    if snapshot_seq == 0 || last_sequence == 0 {
        return Err(PersistError::WalRotationZeroSequence);
    }
    if snapshot_seq != last_sequence {
        return Err(PersistError::WalRotationSequenceMismatch {
            snapshot_seq,
            last_sequence,
        });
    }
    if let Some(manifest) = &prior_manifest
        && manifest.live_snapshot_seq > snapshot_seq
    {
        return Err(PersistError::WalRotationManifestAhead {
            manifest_sequence: manifest.live_snapshot_seq,
            snapshot_sequence: snapshot_seq,
        });
    }
    let manifest_committed_target = prior_manifest
        .as_ref()
        .is_some_and(|manifest| manifest.live_snapshot_seq == snapshot_seq);
    let baseline_already_current = prior_manifest.is_none()
        && prior_snapshot_seq == snapshot_seq
        && committed_offset == WAL_FILE_HEADER_LEN as u64;
    let target_already_committed = manifest_committed_target || baseline_already_current;
    let prior_archived_seqs = prior_manifest
        .as_ref()
        .map(|manifest| manifest.archived_wal_seqs.clone())
        .unwrap_or_default();

    // A MANIFEST-less directory has no authoritative epoch while Phase 1 is
    // publishing the first new snapshot. Commit a durable baseline first so a
    // crash during Phase 1 cannot make legacy recovery select the orphan. A
    // non-zero WAL header is only a valid baseline when its snapshot exists and
    // verifies; never commit a MANIFEST that would name a missing/corrupt file.
    if prior_manifest.is_none() {
        if prior_snapshot_seq != 0 {
            let prior_snapshot = snapshot_path(Path::new(""), prior_snapshot_seq);
            verify_committed_snapshot(dir, &prior_snapshot)?;
            // Make the baseline snapshot and its directory entry durable before
            // the baseline MANIFEST makes that epoch authoritative.
            dir.open_write(&prior_snapshot)?.sync_all()?;
            dir.sync()?;
        }
        Manifest {
            live_snapshot_seq: prior_snapshot_seq,
            active_wal_header_seq: prior_snapshot_seq,
            compaction_epoch: 0,
            active_wal: DEFAULT_WAL_FILE_NAME.to_owned(),
            archived_wal_seqs: Vec::new(),
        }
        .write_atomic_locked(epoch_guard)?;
    }

    // Phase 1 — publish the snapshot. A re-rotate accepts an existing final
    // path only after comparing it byte-for-byte with the newly encoded temp.
    // Once the MANIFEST already commits this target, absence is corruption and
    // must never be repaired from caller-supplied bytes.
    let snapshot_file = snapshot_path(Path::new(""), snapshot_seq);
    if target_already_committed {
        verify_committed_snapshot(dir, &snapshot_file)?;
    }
    match builder.finalize_for_rotation(epoch_guard, target_already_committed) {
        Err(PersistError::ArtifactIdentityMismatch { path }) if target_already_committed => {
            return Err(PersistError::CommittedSnapshotIdentityMismatch { path });
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    // Durability barrier (audit Item 2 / Seam F): the rotate primitive itself
    // guarantees the published snapshot's bytes are durable BEFORE the Phase-3
    // MANIFEST names it the live epoch — independent of the caller's
    // `SnapshotConfig.fsync`. `SnapshotBuilder::finalize` only `sync_data`s the
    // body when the caller opted into fsync, so without this barrier a crash
    // after the Phase-3 commit could leave the MANIFEST naming a snapshot whose
    // body never reached disk; recovery would then hard-fail on the body hash.
    // Re-opening the published file and `sync_all`ing flushes its inode
    // regardless of which handle wrote it, and is idempotent on the verified
    // existing-artifact branch. The directory-entry fsync follows the file fsync.
    dir.open_write(&snapshot_file)?.sync_all()?;
    dir.sync()?;

    let archived_name = wal_archive_path(wal_name, last_sequence);
    let archived_path = dir.locate(&archived_name);

    // A completed same-sequence retry has no new WAL bytes to archive: the
    // active file is a header-only continuation of this snapshot, while the
    // historical archive (if retention kept it) legitimately contains the
    // prior epoch's header and entries. Verify tracked history structurally and
    // report an explicit no-op instead of comparing those unequal files.
    if target_already_committed
        && prior_snapshot_seq == snapshot_seq
        && committed_offset == WAL_FILE_HEADER_LEN as u64
    {
        if prior_archived_seqs.contains(&snapshot_seq) {
            verify_committed_archive(dir, &archived_name, snapshot_seq)?;
        }
        return Ok((
            WalRotationOutcome::AlreadyCurrent {
                snapshot_sequence: snapshot_seq,
                active_path: wal_path.to_path_buf(),
            },
            RotationCommitState {
                last_sequence: snapshot_seq,
                snapshot_seq,
                committed_offset,
            },
        ));
    }

    // Phase 2 — archive the current WAL. A pre-commit or pre-reset collision is
    // accepted only when the existing archive exactly matches the copied temp.
    if manifest_committed_target && !prior_archived_seqs.contains(&snapshot_seq) {
        if reset_active_wal_file(epoch_guard, file, wal_name, snapshot_seq).is_err() {
            return Err(PersistError::WalRotationIncomplete {
                archived_path: None,
                new_path: wal_path.to_path_buf(),
            });
        }
        return Ok((
            WalRotationOutcome::AlreadyCurrent {
                snapshot_sequence: snapshot_seq,
                active_path: wal_path.to_path_buf(),
            },
            RotationCommitState {
                last_sequence: snapshot_seq,
                snapshot_seq,
                committed_offset: WAL_FILE_HEADER_LEN as u64,
            },
        ));
    }
    match archive_current_wal(epoch_guard, file, &archived_name, committed_offset) {
        Err(PersistError::ArtifactIdentityMismatch { .. }) if manifest_committed_target => {
            return Err(PersistError::CommittedArchiveInvalid {
                path: archived_path,
            });
        }
        Err(error) => return Err(error),
        Ok(()) => {}
    }

    #[cfg(test)]
    run_before_manifest_commit_hook();

    // Phase 3 — COMMIT. The atomic rename is the linearization point.
    if !manifest_committed_target {
        let mut archived_wal_seqs = prior_archived_seqs;
        if !archived_wal_seqs.contains(&last_sequence) {
            archived_wal_seqs.push(last_sequence);
        }
        archived_wal_seqs.sort_unstable();
        let manifest = Manifest {
            live_snapshot_seq: snapshot_seq,
            active_wal_header_seq: snapshot_seq,
            compaction_epoch: 0,
            active_wal: DEFAULT_WAL_FILE_NAME.to_owned(),
            archived_wal_seqs,
        };
        manifest.write_atomic_locked(epoch_guard)?;
    }

    // Phase 4 — safe-after-commit atomic WAL replacement.
    if reset_active_wal_file(epoch_guard, file, wal_name, snapshot_seq).is_err() {
        return Err(PersistError::WalRotationIncomplete {
            archived_path: Some(archived_path),
            new_path: wal_path.to_path_buf(),
        });
    }

    Ok((
        WalRotationOutcome::Rotated {
            archived_path,
            new_path: wal_path.to_path_buf(),
            archived_last_sequence: last_sequence,
        },
        RotationCommitState {
            last_sequence: snapshot_seq,
            snapshot_seq,
            committed_offset: WAL_FILE_HEADER_LEN as u64,
        },
    ))
}

#[cfg(test)]
thread_local! {
    static BEFORE_MANIFEST_COMMIT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_before_manifest_commit_hook(hook: impl FnOnce() + 'static) {
    BEFORE_MANIFEST_COMMIT_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn run_before_manifest_commit_hook() {
    BEFORE_MANIFEST_COMMIT_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

fn verify_committed_archive(
    dir: &StoreDirectory,
    path: &Path,
    expected_last_sequence: u64,
) -> PersistResult<()> {
    let valid = (|| -> PersistResult<()> {
        crate::artifact_identity::require_regular_file(dir, path)?;
        let reader = crate::WalReader::open_in(dir, path)?;
        if reader.snapshot_seq() >= expected_last_sequence {
            return Err(crate::artifact_identity::identity_mismatch(path));
        }
        let mut expected_next = reader.snapshot_seq().checked_add(1);
        let mut observed_last_sequence = None;
        for entry in reader.iterate(|_| true)? {
            let entry = entry?;
            if expected_next != Some(entry.header.sequence) {
                return Err(crate::artifact_identity::identity_mismatch(path));
            }
            entry.body()?;
            observed_last_sequence = Some(entry.header.sequence);
            expected_next = entry.header.sequence.checked_add(1);
        }
        if observed_last_sequence != Some(expected_last_sequence) {
            return Err(crate::artifact_identity::identity_mismatch(path));
        }
        Ok(())
    })();
    if valid.is_err() {
        return Err(PersistError::CommittedArchiveInvalid {
            path: dir.locate(path),
        });
    }
    Ok(())
}

fn verify_committed_snapshot(dir: &StoreDirectory, path: &Path) -> PersistResult<()> {
    if !dir
        .metadata(path)?
        .is_some_and(|m| m.regular && m.single_link)
    {
        return Err(PersistError::CommittedSnapshotUnavailable {
            path: dir.locate(path),
        });
    }
    let mut reader = SnapshotReader::open_in(dir, path)?;
    reader.verify_body_hash()
}

#[cfg(test)]
#[path = "writer_rotation/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "writer_rotation/concurrency_tests.rs"]
mod concurrency_tests;

#[cfg(test)]
#[path = "writer_rotation/path_anchor_tests.rs"]
mod path_anchor_tests;
