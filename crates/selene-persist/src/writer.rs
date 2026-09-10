//! Append-only WAL writer.

mod append;

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

use selene_core::HlcTimestamp;

use crate::compression::ZstdCompressor;
use crate::file_header::{WAL_FILE_HEADER_LEN, WalFileHeader};
use crate::manifest::Manifest;
use crate::manifest_lock::ManifestEpochGuard;
use crate::payload::WalCompression;
use crate::retention::{PruneOutcome, RetentionPolicy};
use crate::snapshot_writer::SnapshotBuilder;
use crate::wal_path::open_locked_wal;
use crate::writer_rotation::{RotationInputs, WalRotationOutcome, rotate_with_manifest};
use crate::{PersistError, PersistResult, StoreDirectory, StoreWriter};

/// Conventional v1.0 single-file WAL name used by embedders.
pub const DEFAULT_WAL_FILE_NAME: &str = "wal.log";

/// WAL fsync scheduling policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncPolicy {
    /// Flush and fsync after every `N` appended entries, on explicit flush,
    /// and when the writer is dropped.
    ///
    /// `EveryN(1)` is durability-by-default. Values greater than `1` opt into
    /// group commit. `EveryN(0)` is normalized to `EveryN(1)` on open.
    EveryN(u32),
    /// Never fsync during append or drop; only explicit [`WalWriter::flush`]
    /// fsyncs.
    ///
    /// This is an explicit opt-in for benchmark parity and offline paths where
    /// durability is provided elsewhere. It is not the production default.
    ///
    /// # selene-graph forces this for the committer WAL (v1.2 BRIEF 2)
    ///
    /// When a [`WalWriter`] is owned by selene-graph's single committer thread
    /// (via `SharedGraphBuilder::with_wal` / `SharedGraph::from_graph_with_wal` /
    /// recovery), the committer is the **sole fsync caller**: it appends a
    /// contiguous run of commits with fsync deferred, then issues exactly one
    /// [`WalWriter::flush`] per run as the fsync-before-publish barrier. To make
    /// that the only fsync path, selene-graph **overrides `WalConfig::sync_policy`
    /// to `OnFlushOnly`** before opening such a WAL, discarding any caller policy
    /// (the fsync cadence is instead set by `selene_graph::CommitBatching`). A
    /// `WalWriter` opened directly (outside selene-graph) still honors whatever
    /// policy the caller passes — the override lives in selene-graph, not here.
    OnFlushOnly,
}

impl SyncPolicy {
    /// Return the `EveryN` threshold when this policy syncs on append.
    #[must_use]
    pub const fn as_every_n(self) -> Option<u32> {
        match self {
            Self::EveryN(value) => Some(value),
            Self::OnFlushOnly => None,
        }
    }

    const fn normalized(self) -> Self {
        match self {
            Self::EveryN(0) => Self::EveryN(1),
            policy => policy,
        }
    }

    const fn syncs_on_drop(self) -> bool {
        matches!(self, Self::EveryN(_))
    }
}

/// WAL writer configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalConfig {
    /// Flush and fsync schedule.
    pub sync_policy: SyncPolicy,
    /// Highest WAL sequence covered by the snapshot this file extends.
    ///
    /// Written into the file header on a fresh file, and used to seed
    /// `last_sequence` so the first appended entry receives sequence
    /// `snapshot_seq + 1`. On reopen, the on-disk header is the source of
    /// truth and the config value is ignored — recovery never moves a
    /// snapshot watermark backward.
    pub snapshot_seq: u64,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq: 0,
        }
    }
}

impl WalConfig {
    /// Construct a WAL config with the legacy group-commit threshold.
    #[must_use]
    pub const fn with_fsync_every_n(fsync_every_n: u32) -> Self {
        Self {
            sync_policy: SyncPolicy::EveryN(fsync_every_n),
            snapshot_seq: 0,
        }
    }
}

/// Single-threaded append-only WAL writer.
///
/// Holds an exclusive OS-level file lock on the WAL file for the writer's
/// lifetime, so a second `WalWriter::open` call on the same path
/// (in-process or cross-process) fails fast with
/// [`PersistError::WriterLockHeld`] rather than corrupting the log. A store-wide
/// lease also excludes writers using other names in the same directory. The
/// parent directory is retained as a capability; a final symlink or non-file entry
/// is rejected with [`PersistError::WalPathNotRegular`]. A new WAL is written
/// and synced under a unique sibling name before a fail-on-existing hard-link
/// publishes its complete header at the final path, so concurrent readers never
/// observe a partially initialized file.
pub struct WalWriter {
    file: File,
    path: PathBuf,
    authority: StoreWriter,
    record: Vec<u8>,
    last_sequence: u64,
    snapshot_seq: u64,
    sync_policy: SyncPolicy,
    compression: WalCompression,
    compressor: Option<ZstdCompressor>,
    entries_since_fsync: u32,
    poisoned: bool,
    /// Torn tail discarded at open, if any. See [`WalWriter::tail_repair`].
    ///
    /// Boxed because it is set at most once per open and read rarely, while
    /// `WalWriter` is embedded in a hot enum variant whose size is linted.
    tail_repair: Option<Box<crate::WalTailRepair>>,
    /// File offset of the last fully-committed entry's end. On any
    /// append-time error, the file is truncated and re-seeked to this
    /// offset so the writer's in-memory state and the on-disk state stay
    /// consistent.
    committed_offset: u64,
}

impl WalWriter {
    /// Open a WAL file for append, writing a fresh header for a new file.
    ///
    /// Existing files are scanned once. A **torn tail** — the file ending inside
    /// the last frame, a failing last frame with nothing after it, or a trailing
    /// run of zeros — is discarded, and [`Self::tail_repair`] reports that it
    /// happened. Those bytes were never acknowledged, so losing them is ordinary
    /// crash recovery.
    ///
    /// A frame that fails validation with committed bytes *after* it is not a
    /// tail. It is corruption, and open fails with
    /// [`PersistError::WalMidLogCorruption`] leaving the file untouched, rather
    /// than truncating away every committed frame beyond the damage.
    ///
    /// Acquires an exclusive OS-level file lock; a second writer on the
    /// same path fails immediately with
    /// [`PersistError::WriterLockHeld`] instead of clobbering the log.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::WalMidLogCorruption`] for damage before the tail;
    /// [`PersistError::UnsupportedVersion`] for a store from another format
    /// version; and I/O (including unsupported hard-link publication for a new
    /// WAL), header, sequence, lock, checksum, or non-regular-path errors
    /// encountered while opening and validating the WAL.
    pub fn open(path: &Path, config: WalConfig) -> PersistResult<Self> {
        Self::open_with_compression(path, config, WalCompression::default())
    }

    /// Open a WAL file with an explicit payload compression policy.
    ///
    /// This keeps the same file format as [`Self::open`]; only the append-time
    /// decision to compress each serialized payload changes. Existing readers
    /// continue to use the per-entry compression flag stored in the header.
    ///
    /// # Errors
    ///
    /// Returns I/O (including unsupported hard-link publication for a new WAL),
    /// header, sequence, lock, checksum, or non-regular-path errors encountered
    /// while opening and validating the WAL.
    pub fn open_with_compression(
        path: &Path,
        config: WalConfig,
        compression: WalCompression,
    ) -> PersistResult<Self> {
        // Anchor once, reject unsafe final components, and retain directory and
        // writer authority for every later managed artifact operation.
        let (file, stable_path, authority) = open_locked_wal(path, config.snapshot_seq)?;
        Self::from_locked_file(file, stable_path, authority, config, compression)
    }

    /// Open a WAL through an already-retained directory, acquiring writer ownership.
    ///
    /// # Errors
    /// Returns the same validation, locking, and I/O errors as [`Self::open`].
    pub fn open_in(dir: &StoreDirectory, name: &Path, config: WalConfig) -> PersistResult<Self> {
        crate::store_directory::validate_data_name(name)?;
        let authority = StoreWriter::acquire(dir)?;
        Self::open_with_authority(&authority, name, config)
    }

    /// Compose a WAL with an explicitly owned writer lease (for example audit).
    /// The WAL inode also receives its own nonblocking lifetime lock.
    ///
    /// # Errors
    /// Returns the same validation, locking, and I/O errors as [`Self::open`].
    pub fn open_with_authority(
        authority: &StoreWriter,
        name: &Path,
        config: WalConfig,
    ) -> PersistResult<Self> {
        let dir = authority.directory();
        let file = crate::wal_path::open_locked_wal_in(authority, name, config.snapshot_seq)?;
        Self::from_locked_file(
            file,
            dir.locate(name),
            authority.clone(),
            config,
            WalCompression::default(),
        )
    }

    fn from_locked_file(
        mut file: File,
        stable_path: PathBuf,
        authority: StoreWriter,
        config: WalConfig,
        compression: WalCompression,
    ) -> PersistResult<Self> {
        let sync_policy = config.sync_policy.normalized();
        file.seek(SeekFrom::Start(0))?;
        let header_snapshot_seq = WalFileHeader::read_from(&mut file)?.snapshot_seq;

        // Returns Err without proposing a truncation when the failure cannot be
        // a torn tail, so a corrupt log reaches the `?` below with its bytes
        // still on disk. Nothing between here and there may modify the file.
        let scan = crate::wal_tail::scan_existing(&mut file)?;
        if let Some(repair) = scan.repair {
            tracing::warn!(
                offset = repair.offset,
                discarded_bytes = repair.discarded_bytes,
                reason = ?repair.reason,
                "discarding torn WAL tail"
            );
            file.set_len(scan.truncate_to)?;
        }
        file.seek(SeekFrom::Start(scan.truncate_to))?;

        // Seed last_sequence from the larger of (header watermark, last
        // scanned entry). On a fresh file, scan returns 0 and the
        // watermark wins. On reopen with entries that already extend past
        // the snapshot, the entry sequence wins.
        let last_sequence = scan.last_sequence.max(header_snapshot_seq);
        Ok(Self {
            file,
            path: stable_path,
            authority,
            record: Vec::new(),
            last_sequence,
            snapshot_seq: header_snapshot_seq,
            sync_policy,
            compression,
            compressor: None,
            entries_since_fsync: 0,
            poisoned: false,
            committed_offset: scan.truncate_to,
            tail_repair: scan.repair.map(Box::new),
        })
    }

    /// Flush + fsync without appending. Useful before snapshot publication.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::WalWriterPoisoned`] after rotation detects
    /// persistence-state divergence, or I/O errors from fsync.
    #[tracing::instrument(name = "selene.persist.wal.fsync", skip(self))]
    pub fn flush(&mut self) -> PersistResult<()> {
        self.ensure_usable()?;
        self.file.sync_data()?;
        self.entries_since_fsync = 0;
        Ok(())
    }

    /// Return the last sequence assigned by this writer.
    #[must_use]
    pub const fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Return the path of the active WAL file owned by this writer.
    ///
    /// Relative caller paths and parent symlink aliases are resolved at
    /// [`Self::open`] time. This diagnostic locator may no longer name the
    /// opened store after a rename. Use [`Self::directory`] for checkpoint,
    /// recovery, and other managed I/O; never reopen this locator internally.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Retained filesystem authority. [`Self::path`] is only a diagnostic locator.
    #[must_use]
    pub fn directory(&self) -> &StoreDirectory {
        self.authority.directory()
    }

    /// Share this writer lease with a deliberately composed audit component.
    #[must_use]
    pub fn authority(&self) -> &StoreWriter {
        &self.authority
    }

    /// Report the torn tail discarded when this WAL was opened, if any.
    ///
    /// `None` means the log parsed cleanly to its end. A corrupt log never
    /// reaches this accessor: [`WalWriter::open`] fails instead of repairing,
    /// so a `Some` here always describes bytes that were never acknowledged.
    #[must_use]
    pub fn tail_repair(&self) -> Option<crate::WalTailRepair> {
        self.tail_repair.as_deref().copied()
    }

    /// Return the durable file offset of the last fully committed WAL entry.
    #[must_use]
    pub const fn committed_offset(&self) -> u64 {
        self.committed_offset
    }

    /// Return the snapshot sequence stored in this WAL file's header.
    #[must_use]
    pub const fn snapshot_seq(&self) -> u64 {
        self.snapshot_seq
    }

    /// Return the number of entries appended since the last fsync.
    #[must_use]
    pub const fn entries_since_fsync(&self) -> u32 {
        self.entries_since_fsync
    }

    /// Crash-safe rotate: finalize `builder`, commit a MANIFEST, then reset.
    ///
    /// This is the v1.x replacement for the embedder's two-call
    /// finalize-then-`rotate` sequence. It runs the 4-phase rotation
    /// whose MANIFEST write is the single linearization / commit point, so a
    /// crash at any point either leaves the previous epoch fully recoverable or
    /// the new epoch fully committed — never the [`PersistError::WalSnapshotMismatch`]
    /// (Seam F) hard-fail the split calls could produce.
    ///
    /// `builder` must target the same sequence as this writer's current
    /// high-water mark (`builder.sequence() == self.last_sequence()`); the
    /// builder is finalized as Phase 1, so the caller adds every section before
    /// calling. The MANIFEST's `archived_wal_seqs` extends the set already named
    /// by any live MANIFEST in this writer's directory, so retention (Item-5)
    /// has the full archive history. Once the builder directory resolves to the
    /// writer anchor, Phase 1 consumes that anchor rather than the caller path.
    ///
    /// Before the first Phase 1 in a legacy MANIFEST-less directory, rotation
    /// durably bootstraps a baseline MANIFEST naming the current WAL-header
    /// snapshot epoch. A non-zero baseline must already have a valid snapshot;
    /// otherwise rotation rejects it before publishing the requested snapshot.
    /// Phase 4 uses a synced same-directory replacement + atomic rename, so a
    /// reset failure leaves recovery with either the old valid WAL or the new
    /// valid header rather than a truncated active file.
    /// Rotation accepts only the conventional `wal.log` filename, a positive
    /// sequence, a builder directory resolving to the WAL's stable parent, and
    /// an existing MANIFEST that also names `wal.log` as its active WAL.
    /// Same-sequence crash artifacts are accepted only after exact byte
    /// comparison with the newly encoded snapshot and copied WAL temporary. A
    /// fully committed same-sequence call returns
    /// [`WalRotationOutcome::AlreadyCurrent`] instead of re-archiving the
    /// header-only active WAL. Rotation holds
    /// [`crate::MANIFEST_LOCK_FILE_NAME`] from its authoritative MANIFEST read
    /// through active-WAL reset, so owned maintenance cannot interleave.
    ///
    /// A second mutable borrow cannot overlap the rotation:
    ///
    /// ```compile_fail
    /// # use selene_persist::{SnapshotBuilder, SnapshotConfig, WalWriter};
    /// fn cannot_overlap(writer: &mut WalWriter, builder: SnapshotBuilder) {
    ///     let active = writer;
    ///     let _ = writer.rotate_with_manifest(builder);
    ///     let _ = active.last_sequence();
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns typed errors for a poisoned writer, a non-default WAL filename,
    /// sequence zero, a builder/WAL directory mismatch, a builder sequence that
    /// does not match the writer high-water mark, or a MANIFEST ahead of the
    /// request; identity, I/O, or format errors from snapshot finalize, archive,
    /// MANIFEST commit, or WAL reset; or
    /// [`PersistError::WalRotationIncomplete`] if the MANIFEST committed but the
    /// active WAL could not be reset (recovery still converges on the new
    /// epoch). After `WalRotationIncomplete`, a MANIFEST-ahead rejection, or an
    /// error validating an already-committed target, this writer is poisoned
    /// and rejects append, flush, rotation, and prune calls; reopen and recover
    /// the active epoch before mutating again. A same-sequence collision while
    /// the MANIFEST still names the prior epoch leaves the writer usable.
    pub fn rotate_with_manifest(
        &mut self,
        builder: SnapshotBuilder,
    ) -> PersistResult<WalRotationOutcome> {
        self.ensure_usable()?;
        self.validate_rotation_inputs(&builder)?;
        #[cfg(test)]
        crate::wal_path::run_after_rotation_preflight_hook();
        let mut epoch_guard = ManifestEpochGuard::acquire(&self.authority)?;
        let prior_manifest = self.read_prior_manifest(&epoch_guard, builder.sequence())?;
        let target_already_committed = prior_manifest
            .as_ref()
            .is_some_and(|manifest| manifest.live_snapshot_seq == builder.sequence())
            || (prior_manifest.is_none()
                && self.snapshot_seq == builder.sequence()
                && self.committed_offset == WAL_FILE_HEADER_LEN as u64);
        self.finish_rotation(
            builder,
            &mut epoch_guard,
            prior_manifest,
            target_already_committed,
        )
    }

    /// Append a typed checkpoint watermark and rotate its prepared snapshot.
    ///
    /// `builder.sequence()` must be exactly one greater than this writer's
    /// current high-water mark. The method holds `MANIFEST.lock` while it
    /// rejects a stale writer, appends the local/principal-free/empty marker,
    /// flushes it through the existing rotation barrier, and commits the new
    /// snapshot epoch. Provider serialization should be complete before this
    /// call so preparation failures cannot consume a physical sequence.
    ///
    /// Unlike [`Self::rotate_with_manifest`], this operation intentionally
    /// creates a fresh epoch even when the active WAL is header-only. The
    /// marker is a physical sequence watermark, not a logical graph commit.
    ///
    /// # Errors
    ///
    /// Returns typed layout, sequence, stale-MANIFEST, marker append, or
    /// rotation errors. A marker append failure poisons the writer; reopen it
    /// before further mutation. Once append succeeds, the marker sequence is
    /// consumed even if a pre-MANIFEST artifact collision leaves the writer
    /// usable. Such a caller must prepare a fresh builder for the new
    /// `last_sequence + 1`, not retry the old builder. Ambiguous or
    /// post-MANIFEST failures follow [`Self::rotate_with_manifest`]'s
    /// poison/reopen contract.
    pub fn rotate_with_checkpoint_watermark(
        &mut self,
        builder: SnapshotBuilder,
        hlc: HlcTimestamp,
    ) -> PersistResult<WalRotationOutcome> {
        self.ensure_usable()?;
        let expected_sequence =
            self.last_sequence
                .checked_add(1)
                .ok_or(PersistError::WalSequenceExhausted {
                    last_sequence: self.last_sequence,
                })?;
        self.validate_rotation_layout(&builder)?;
        if builder.sequence() != expected_sequence {
            return Err(PersistError::WalCheckpointSequenceMismatch {
                snapshot_seq: builder.sequence(),
                expected_sequence,
            });
        }
        #[cfg(test)]
        crate::wal_path::run_after_rotation_preflight_hook();
        let mut epoch_guard = ManifestEpochGuard::acquire(&self.authority)?;
        let prior_manifest = self.read_prior_manifest(&epoch_guard, self.last_sequence)?;
        if let Err(error) = self.append_checkpoint_watermark_record(hlc) {
            self.poisoned = true;
            return Err(error);
        }
        self.finish_rotation(builder, &mut epoch_guard, prior_manifest, false)
    }

    fn read_prior_manifest(
        &mut self,
        epoch_guard: &ManifestEpochGuard,
        maximum_sequence: u64,
    ) -> PersistResult<Option<Manifest>> {
        let prior_manifest = match Manifest::read_in(epoch_guard.directory()) {
            Ok(manifest) => manifest,
            Err(error) => {
                self.poisoned = true;
                return Err(error);
            }
        };
        if let Some(manifest) = &prior_manifest
            && manifest.active_wal != DEFAULT_WAL_FILE_NAME
        {
            self.poisoned = true;
            return Err(PersistError::UnexpectedActiveWal {
                observed: manifest.active_wal.clone(),
                expected: DEFAULT_WAL_FILE_NAME,
            });
        }
        if let Some(manifest) = &prior_manifest
            && manifest.live_snapshot_seq > maximum_sequence
        {
            self.poisoned = true;
            return Err(PersistError::WalRotationManifestAhead {
                manifest_sequence: manifest.live_snapshot_seq,
                snapshot_sequence: maximum_sequence,
            });
        }
        Ok(prior_manifest)
    }

    fn finish_rotation(
        &mut self,
        builder: SnapshotBuilder,
        epoch_guard: &mut ManifestEpochGuard,
        prior_manifest: Option<Manifest>,
        target_already_committed: bool,
    ) -> PersistResult<WalRotationOutcome> {
        // Every input invariant above is checked before this durability write.
        // Invalid callers must not flush group-commit state or create artifacts.
        self.flush()?;
        let inputs = RotationInputs {
            file: &mut self.file,
            wal_path: &self.path,
            committed_offset: self.committed_offset,
            last_sequence: self.last_sequence,
            snapshot_seq: self.snapshot_seq,
            prior_manifest,
        };
        let (outcome, state) = match rotate_with_manifest(inputs, builder, epoch_guard) {
            Ok(result) => result,
            Err(error @ PersistError::WalRotationIncomplete { .. }) => {
                self.poisoned = true;
                return Err(error);
            }
            Err(error) if target_already_committed => {
                self.poisoned = true;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        self.last_sequence = state.last_sequence;
        self.snapshot_seq = state.snapshot_seq;
        self.committed_offset = state.committed_offset;
        self.entries_since_fsync = 0;
        Ok(outcome)
    }

    fn validate_rotation_inputs(&self, builder: &SnapshotBuilder) -> PersistResult<()> {
        if builder.sequence() == 0 || self.last_sequence == 0 {
            return Err(PersistError::WalRotationZeroSequence);
        }
        if builder.sequence() != self.last_sequence {
            return Err(PersistError::WalRotationSequenceMismatch {
                snapshot_seq: builder.sequence(),
                last_sequence: self.last_sequence,
            });
        }
        self.validate_rotation_layout(builder)
    }

    fn validate_rotation_layout(&self, builder: &SnapshotBuilder) -> PersistResult<()> {
        let observed_name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());
        if observed_name != DEFAULT_WAL_FILE_NAME {
            return Err(PersistError::UnexpectedActiveWal {
                observed: observed_name,
                expected: DEFAULT_WAL_FILE_NAME,
            });
        }
        let wal_dir = self.directory().locator().to_path_buf();
        let snapshot_dir = builder.dir().to_path_buf();
        if !builder.directory().same_directory(self.directory())? {
            return Err(PersistError::WalRotationDirectoryMismatch {
                snapshot_dir,
                wal_dir,
            });
        }
        Ok(())
    }

    /// Prune superseded snapshots + WAL archives in this writer's directory per
    /// `policy`, committing through the MANIFEST.
    ///
    /// Thin ergonomic wrapper over [`crate::retention::prune`] bound to this
    /// writer's directory. It acquires [`crate::MANIFEST_LOCK_FILE_NAME`] before
    /// flushing pending appends and holds it through post-commit deletion. The
    /// `&mut self` receiver serializes this handle while the directory lock
    /// serializes maintenance sharing this writer proof. Independent standalone
    /// mutations fail on `LOCK` instead of reacquiring ownership. Prune never
    /// touches the active WAL; it reclaims only superseded snapshot/archive
    /// files.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::WalWriterPoisoned`] when the writer was already
    /// poisoned. Otherwise returns an epoch-lock, flush, directory-scan,
    /// MANIFEST-decode, or MANIFEST-commit error. These prune failures do not
    /// newly poison the writer. Post-commit file deletion is best-effort and
    /// never fails the prune.
    pub fn prune(&mut self, policy: &RetentionPolicy) -> PersistResult<PruneOutcome> {
        self.ensure_usable()?;
        let mut epoch_guard = ManifestEpochGuard::acquire(&self.authority)?;
        self.flush()?;
        crate::retention::prune_locked(&mut epoch_guard, policy)
    }

    fn ensure_usable(&self) -> PersistResult<()> {
        if self.poisoned {
            Err(PersistError::WalWriterPoisoned)
        } else {
            Ok(())
        }
    }
}

impl Drop for WalWriter {
    fn drop(&mut self) {
        if self.sync_policy.syncs_on_drop()
            && let Err(error) = self.file.sync_data()
        {
            tracing::error!(%error, "failed to fsync WAL writer on drop");
        }
        // The exclusive file lock is released when `file` is dropped.
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "writer/checkpoint_watermark_tests.rs"]
mod checkpoint_watermark_tests;
