//! Snapshot + WAL-archive retention: typed policy and MANIFEST-atomic prune.
//!
//! Per the 2026-05-26 deletion+reclamation audit (Item 5 / D26), a v1.0
//! persistence directory grows without bound: every rotation publishes a new
//! `snapshot.{seq}.snap` and a new `wal.{seq}.archive`, and nothing ever
//! reclaims the superseded ones. [`prune`] is the engine-side escape valve.
//!
//! # The retention floor (load-bearing safety invariant)
//!
//! Prune **never** deletes anything required to recover the live epoch. The
//! snapshot named by the MANIFEST's [`Manifest::live_snapshot_seq`] and the
//! active WAL ([`Manifest::active_wal`]) are sacrosanct regardless of policy —
//! even `keep_n_snapshots = 0` retains the live snapshot. WAL archives are
//! fully prunable because the live snapshot subsumes every change they hold;
//! they are kept only for point-in-time / forensic recovery.
//!
//! # Crash safety (the MANIFEST commit-point invariant)
//!
//! Prune mirrors rotation's ordering: rewrite the MANIFEST *first* (the atomic
//! rename is the single linearization point), delete files *after*. A crash
//! before the rewrite leaves the prior MANIFEST and every file intact; a crash
//! after it leaves orphan files that recovery already ignores (it opens only
//! `live_snapshot_seq` and the archives named by the committed MANIFEST), and
//! the next prune reclaims them. The MANIFEST is therefore never observed
//! referencing a deleted archive.
//!
//! # Policy is configuration, not persisted state
//!
//! [`RetentionPolicy`] is supplied by the embedder at prune time, exactly like
//! [`crate::SnapshotConfig`]. It is deliberately *not* written into the
//! MANIFEST: a persisted policy would be a second source of truth that could
//! diverge from the embedder's configuration, the dual-write divergence the
//! audit's "single writer per concern" lesson rules out. The MANIFEST's
//! reserved `retention_present` byte stays reserved.
//!
//! # Prune cadence is the embedder's responsibility
//!
//! The MANIFEST's `archived_wal_seqs` vector gains one entry per rotation and is
//! only ever shrunk by [`prune`]; it is *not* soft-capped by the engine. An
//! embedder that never prunes therefore grows the vector — and the MANIFEST it
//! is re-encoded into every rotation — without bound. This is the documented
//! contract, not a defect: retention is opt-in engine policy, and bounding the
//! archive history is exactly what calling [`prune`] (or
//! [`crate::WalWriter::prune`]) on a cadence does. Prune is idempotent and
//! MANIFEST-atomic, so a periodic call is safe and a no-op when nothing is
//! reclaimable.
//!
//! # Serialization with rotation and readers
//!
//! Prune holds the persistent [`crate::MANIFEST_LOCK_FILE_NAME`] lock from its
//! authoritative MANIFEST read through every post-commit deletion. WAL
//! rotation holds the same lock through its active-WAL reset. This makes the
//! two operations linear rather than allowing a stale prune plan to overwrite
//! a newer epoch or delete its just-published artifacts. The lock file is
//! permanent coordination state and must never be unlinked while the directory
//! may be in use. [`crate::PersistenceReadGuard`] takes the shared side of the
//! same lock, so prune also waits for online recovery and backup readers to
//! finish using the artifact set they selected under the guard.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::PersistResult;
use crate::manifest::Manifest;
use crate::manifest_lock::ManifestEpochGuard;
use crate::snapshot_path::parse_snapshot_filename;
use crate::writer_rotation::parse_wal_archive_filename;

/// Default number of snapshots retained (the live one plus one predecessor).
pub const DEFAULT_KEEP_SNAPSHOTS: u32 = 2;
/// Default number of WAL archives retained.
pub const DEFAULT_KEEP_WAL_ARCHIVES: u32 = 4;

/// Typed retention policy governing how many snapshots and WAL archives survive
/// a [`prune`].
///
/// The four constraints are **conjunctive**: a file is retained only if it
/// survives the count selection *and* is not aged out by [`time_based`] *and*
/// does not have to be evicted to fit [`max_total_size_bytes`]. The live
/// snapshot is exempt from every constraint (see the [module docs](self)).
///
/// [`time_based`]: Self::time_based
/// [`max_total_size_bytes`]: Self::max_total_size_bytes
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    /// Maximum snapshots to retain, counting the live one. `0` still retains
    /// the live snapshot (the floor); `2` keeps the live snapshot plus the most
    /// recent predecessor.
    pub keep_n_snapshots: u32,
    /// Maximum WAL archives to retain (newest sequences kept).
    pub keep_n_wal_archives: u32,
    /// Optional cap on the total bytes of all retained snapshots + archives.
    /// When `Some`, the oldest non-live files are evicted until the retained
    /// set fits. `None` disables the size constraint.
    pub max_total_size_bytes: Option<u64>,
    /// Optional maximum age: retained files whose modification time is older
    /// than `now - time_based` are evicted (the live snapshot is exempt).
    /// `None` disables the age constraint.
    pub time_based: Option<Duration>,
}

impl Default for RetentionPolicy {
    /// `keep_n_snapshots = 2`, `keep_n_wal_archives = 4`, no size or time limit
    /// — the audit's D26 defaults.
    fn default() -> Self {
        Self {
            keep_n_snapshots: DEFAULT_KEEP_SNAPSHOTS,
            keep_n_wal_archives: DEFAULT_KEEP_WAL_ARCHIVES,
            max_total_size_bytes: None,
            time_based: None,
        }
    }
}

/// What a [`prune`] reclaimed and retained.
///
/// Sequence vectors are ascending. A no-op prune (nothing to reclaim, or no
/// MANIFEST present) returns empty `deleted_*` vectors and `bytes_reclaimed`
/// of `0`, so callers can treat prune as idempotent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PruneOutcome {
    /// Snapshot sequences whose files were deleted.
    pub deleted_snapshots: Vec<u64>,
    /// WAL-archive sequences whose files were deleted.
    pub deleted_wal_archives: Vec<u64>,
    /// Snapshot sequences retained (includes the live snapshot).
    pub retained_snapshots: Vec<u64>,
    /// WAL-archive sequences retained.
    pub retained_wal_archives: Vec<u64>,
    /// Total bytes reclaimed across all deleted files.
    pub bytes_reclaimed: u64,
}

/// One on-disk persistence file the prune planner reasons about.
#[derive(Clone, Debug)]
struct FileEntry {
    seq: u64,
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

/// Prune `dir` per `policy`, committing through the MANIFEST.
///
/// Returns a [`PruneOutcome`] describing the reclamation. When `dir` has no
/// committed MANIFEST (a fresh or pre-MANIFEST directory) prune is a no-op: it
/// has no authoritative live epoch to protect against, so it declines to delete
/// anything and returns an empty outcome.
///
/// # Errors
///
/// Returns writer contention or I/O errors acquiring the writer/exclusive epoch, scanning the
/// directory, or committing the rewritten MANIFEST, or any [`Manifest::decode`]
/// error from a corrupt committed MANIFEST. Acquisition waits for active
/// [`crate::PersistenceReadGuard`] readers. Best-effort file deletion runs
/// *after* the MANIFEST commit and a missing file is treated as
/// already-reclaimed, so post-commit deletion does not fail the prune (a
/// residual orphan is reclaimed by the next prune).
pub fn prune(dir: &Path, policy: &RetentionPolicy) -> PersistResult<PruneOutcome> {
    let dir = match crate::StoreDirectory::open(dir) {
        Ok(dir) => dir,
        Err(crate::PersistError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PruneOutcome::default());
        }
        Err(error) => return Err(error),
    };
    prune_in(&dir, policy)
}

/// Acquire standalone writer ownership, then prune through one exclusive epoch.
///
/// # Errors
/// Returns the same selection, validation and publication errors as [`prune`].
pub fn prune_in(
    dir: &crate::StoreDirectory,
    policy: &RetentionPolicy,
) -> PersistResult<PruneOutcome> {
    let authority = crate::StoreWriter::acquire(dir)?;
    prune_with_authority(&authority, policy)
}

/// Prune using an existing store writer lease, without reacquiring `LOCK`.
///
/// # Errors
/// Returns epoch-lock, selection, validation, and publication errors.
pub fn prune_with_authority(
    authority: &crate::StoreWriter,
    policy: &RetentionPolicy,
) -> PersistResult<PruneOutcome> {
    let mut guard = ManifestEpochGuard::acquire(authority)?;
    prune_locked(&mut guard, policy)
}

/// Execute prune while the caller holds the directory epoch lock.
pub(crate) fn prune_locked(
    guard: &mut ManifestEpochGuard,
    policy: &RetentionPolicy,
) -> PersistResult<PruneOutcome> {
    let cap = guard.directory().clone();
    let dir = &cap;
    let Some(manifest) = Manifest::read_in(dir)? else {
        return Ok(PruneOutcome::default());
    };
    let live = manifest.live_snapshot_seq;

    // Snapshots are disk-authoritative (recovery finds the live one by sequence);
    // archives are MANIFEST-authoritative (`archived_wal_seqs` is the logical set
    // a tool/PITR would consult). We scan the disk for both to get file paths,
    // sizes, and mtimes, then drive archive *selection* from the manifest list so
    // prune never resurrects a crash-orphan archive into the committed manifest.
    let snapshots = scan(dir, parse_snapshot_filename)?;
    let archives_on_disk = scan(dir, parse_wal_archive_filename)?;
    // O(log n) membership for the archive selection below (the persisted MANIFEST
    // `archived_wal_seqs` stays a Vec; this set is a per-prune lookup accelerator).
    let tracked_seqs: BTreeSet<u64> = manifest.archived_wal_seqs.iter().copied().collect();
    let tracked: Vec<FileEntry> = archives_on_disk
        .iter()
        .filter(|e| tracked_seqs.contains(&e.seq))
        .cloned()
        .collect();

    let mut retained_snaps = select_snapshots(&snapshots, live, policy.keep_n_snapshots);
    let mut retained_archs = select_newest(&tracked, policy.keep_n_wal_archives);

    apply_time_constraint(
        policy,
        live,
        &snapshots,
        &tracked,
        &mut retained_snaps,
        &mut retained_archs,
    );
    apply_size_constraint(
        policy,
        live,
        &snapshots,
        &tracked,
        &mut retained_snaps,
        &mut retained_archs,
    );

    // MANIFEST-atomic commit (the linearization point) — only when the retained
    // archive set actually changes. The rewritten list is always a subset of the
    // prior one (never grows), so the manifest can shrink but never adopt an
    // orphan. A manifest seq whose file vanished is also dropped here.
    let new_archived: Vec<u64> = retained_archs.iter().copied().collect();
    #[cfg(test)]
    run_before_commit_hook();
    if new_archived != manifest.archived_wal_seqs {
        let next = Manifest {
            archived_wal_seqs: new_archived.clone(),
            ..manifest.clone()
        };
        next.write_atomic_locked(guard)?;
    }

    // Post-commit cleanup: safe-after-commit destructive deletes.
    let mut outcome = PruneOutcome {
        retained_snapshots: retained_snaps.iter().copied().collect(),
        retained_wal_archives: new_archived,
        ..PruneOutcome::default()
    };
    // Snapshots: delete everything not retained (this includes orphan snapshots
    // with seq > live left by a crashed rotation — recovery ignores them).
    for entry in &snapshots {
        if !retained_snaps.contains(&entry.seq) {
            outcome.bytes_reclaimed += delete_file(dir, &entry.path, entry.size);
            outcome.deleted_snapshots.push(entry.seq);
        }
    }
    // Archives: delete tracked-but-not-retained files, plus untracked orphan
    // files strictly older than the live snapshot (provably superseded — the
    // live snapshot captures every change they hold). The current-epoch boundary
    // (seq >= live) is left to rotation's idempotent retry, never to prune.
    for entry in &archives_on_disk {
        let tracked = tracked_seqs.contains(&entry.seq);
        let retained = retained_archs.contains(&entry.seq);
        let superseded_orphan = !tracked && entry.seq < live;
        if (tracked && !retained) || superseded_orphan {
            outcome.bytes_reclaimed += delete_file(dir, &entry.path, entry.size);
            outcome.deleted_wal_archives.push(entry.seq);
        }
    }
    outcome.deleted_snapshots.sort_unstable();
    outcome.deleted_wal_archives.sort_unstable();
    Ok(outcome)
}

#[cfg(test)]
thread_local! {
    static BEFORE_COMMIT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_before_commit_hook(hook: impl FnOnce() + 'static) {
    BEFORE_COMMIT_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn run_before_commit_hook() {
    BEFORE_COMMIT_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

/// Build the snapshot-retention floor + count selection.
///
/// Always retains `live` (when nonzero). Beyond that, keeps the newest
/// `keep_n - 1` snapshots strictly older than `live`. Orphan snapshots with a
/// sequence greater than `live` (left by a crashed rotation, ignored by
/// recovery) are never retained, so prune reclaims them too.
fn select_snapshots(snapshots: &[FileEntry], live: u64, keep_n: u32) -> BTreeSet<u64> {
    let mut retained = BTreeSet::new();
    let has_live = live != 0 && snapshots.iter().any(|e| e.seq == live);
    if has_live {
        retained.insert(live);
    }
    let extra = keep_n.saturating_sub(u32::from(has_live)) as usize;
    let mut older: Vec<u64> = snapshots
        .iter()
        .map(|e| e.seq)
        .filter(|s| if live == 0 { true } else { *s < live })
        .collect();
    older.sort_unstable_by(|a, b| b.cmp(a));
    for seq in older.into_iter().take(extra) {
        retained.insert(seq);
    }
    retained
}

/// Keep the newest `keep_n` sequences from `entries`.
fn select_newest(entries: &[FileEntry], keep_n: u32) -> BTreeSet<u64> {
    let mut seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
    seqs.sort_unstable_by(|a, b| b.cmp(a));
    seqs.into_iter().take(keep_n as usize).collect()
}

/// Evict retained files older than `now - time_based` (live snapshot exempt).
fn apply_time_constraint(
    policy: &RetentionPolicy,
    live: u64,
    snapshots: &[FileEntry],
    archives: &[FileEntry],
    retained_snaps: &mut BTreeSet<u64>,
    retained_archs: &mut BTreeSet<u64>,
) {
    let Some(max_age) = policy.time_based else {
        return;
    };
    let Some(cutoff) = SystemTime::now().checked_sub(max_age) else {
        return; // unrepresentable cutoff: decline to age anything out.
    };
    retained_snaps.retain(|seq| *seq == live || !is_older_than(snapshots, *seq, cutoff));
    retained_archs.retain(|seq| !is_older_than(archives, *seq, cutoff));
}

/// Evict the oldest non-live retained files until the total fits the byte cap.
fn apply_size_constraint(
    policy: &RetentionPolicy,
    live: u64,
    snapshots: &[FileEntry],
    archives: &[FileEntry],
    retained_snaps: &mut BTreeSet<u64>,
    retained_archs: &mut BTreeSet<u64>,
) {
    let Some(budget) = policy.max_total_size_bytes else {
        return;
    };
    // Combined retained set, oldest-first, tagged by kind. Snapshots and
    // archives share the sequence space, so a stable (seq, is_snapshot) order is
    // a deterministic oldest-first ordering.
    let mut combined: Vec<(u64, bool, u64)> = Vec::new(); // (seq, is_snapshot, size)
    for e in snapshots {
        if retained_snaps.contains(&e.seq) {
            combined.push((e.seq, true, e.size));
        }
    }
    for e in archives {
        if retained_archs.contains(&e.seq) {
            combined.push((e.seq, false, e.size));
        }
    }
    let mut total: u64 = combined.iter().map(|(_, _, size)| *size).sum();
    if total <= budget {
        return;
    }
    combined.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    for (seq, is_snapshot, size) in combined {
        if total <= budget {
            break;
        }
        if is_snapshot && seq == live {
            continue; // live snapshot is exempt.
        }
        if is_snapshot {
            retained_snaps.remove(&seq);
        } else {
            retained_archs.remove(&seq);
        }
        total = total.saturating_sub(size);
    }
}

/// Whether the entry for `seq` in `entries` is older than `cutoff`.
///
/// A missing entry or unreadable mtime is treated as *not* older, so clock or
/// metadata anomalies never cause over-deletion.
fn is_older_than(entries: &[FileEntry], seq: u64, cutoff: SystemTime) -> bool {
    entries
        .iter()
        .find(|e| e.seq == seq)
        .and_then(|e| e.modified)
        .is_some_and(|modified| modified < cutoff)
}

/// Scan `dir` for regular files whose name parses via `parse`, capturing size +
/// mtime. Non-files (directories, symlinks) and unparsable names are ignored.
fn scan(
    dir: &crate::StoreDirectory,
    parse: fn(&std::ffi::OsStr) -> Option<u64>,
) -> PersistResult<Vec<FileEntry>> {
    let mut out = Vec::new();
    for name in dir.entries()? {
        let Some(seq) = parse(&name) else {
            continue;
        };
        let Some(metadata) = dir.metadata(Path::new(&name))? else {
            continue;
        };
        if !metadata.regular || !metadata.single_link {
            continue;
        }
        out.push(FileEntry {
            seq,
            path: PathBuf::from(name),
            size: metadata.len,
            modified: metadata.modified,
        });
    }
    Ok(out)
}

/// Delete `path`, returning the bytes reclaimed (`size` on success, `0` on a
/// missing file or other I/O error — best-effort post-commit cleanup).
fn delete_file(dir: &crate::StoreDirectory, path: &Path, size: u64) -> u64 {
    match dir.remove(path) {
        Ok(()) => size,
        Err(crate::PersistError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "retention: best-effort delete failed");
            0
        }
    }
}

#[cfg(test)]
mod tests;
