//! Recovery orchestration.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use selene_core::{NodeId, Origin, metrics};

use crate::manifest::Manifest;
use crate::wal_path::require_regular_wal_or_absent;
use crate::{
    DEFAULT_WAL_FILE_NAME, PersistError, PersistResult, PersistenceReadGuard, ProviderRegistry,
    SnapshotReader, WalReader, snapshot_path,
};

/// Summary of a completed recovery pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryOutcome {
    /// Sequence of the snapshot whose bytes were applied, or `0` if none.
    pub applied_snapshot_seq: u64,
    /// Sequence of the last valid WAL entry read, or the snapshot sequence when
    /// no WAL file exists.
    pub last_wal_seq: u64,
    /// Provider tags that received at least one callback, sorted ascending.
    pub providers_invoked: Vec<[u8; 4]>,
    /// Provider tags that restored at least one snapshot section, sorted ascending.
    pub snapshot_providers_invoked: Vec<[u8; 4]>,
    /// Number of WAL `Change` values delivered to provider fan-out.
    pub wal_changes_applied: u64,
    /// Number of logical WAL commit frames applied after the snapshot.
    ///
    /// This counts ordinary empty commits, but excludes checkpoint watermarks
    /// and replicated frames rejected by deduplication.
    pub wal_commit_entries_applied: u64,
    /// Number of replicated `Change` values skipped by `(source, sequence)`
    /// dedupe.
    pub replicated_changes_deduplicated: u64,
    /// Whether a committed `MANIFEST` drove this recovery (the authoritative
    /// three-step path) rather than the legacy `find_latest_snapshot` fallback.
    pub manifest_present: bool,
}

impl RecoveryOutcome {
    /// Outcome for a directory with no snapshot, no WAL, and no MANIFEST.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            applied_snapshot_seq: 0,
            last_wal_seq: 0,
            providers_invoked: Vec::new(),
            snapshot_providers_invoked: Vec::new(),
            wal_changes_applied: 0,
            wal_commit_entries_applied: 0,
            replicated_changes_deduplicated: 0,
            manifest_present: false,
        }
    }
}

/// Run the three-step recovery against `dir`: MANIFEST read, snapshot apply,
/// WAL replay (D15 amendment).
///
/// **Step 0 — MANIFEST read (authoritative).** When a committed `MANIFEST`
/// exists, `live_snapshot_seq` is the source of truth: the snapshot it names is
/// applied directly (not the highest on-disk snapshot), so an orphan
/// `snapshot.{>live}.snap` left by a crashed rotation is ignored by
/// construction. The legacy WAL/snapshot epoch cross-check is *not* performed —
/// the WAL header may legitimately lag the live snapshot by one epoch after a
/// Phase-3-before-Phase-4 rotation crash. See
/// [`crate::manifest`] for the commit-point invariant.
///
/// **Step 0 fallback — MANIFEST absent.** A fresh or pre-MANIFEST directory
/// recovers via the legacy `find_latest_snapshot` path with the original
/// epoch cross-check intact, so existing on-disk directories recover
/// identically. The next rotation writes a MANIFEST, migrating the directory
/// forward.
///
/// Snapshot sections are routed by provider tag to [`ProviderRegistry`]. WAL
/// entries after the applied snapshot sequence are decoded. Checkpoint
/// watermarks advance physical sequence only and are skipped; logical commit
/// changes are fanned out to every registered provider in deterministic
/// provider-tag order.
/// The input directory is canonicalized once for the entire pass, and a present
/// active WAL must be a regular final directory entry before any provider
/// callback runs.
///
/// # Errors
///
/// Returns directory-resolution, non-regular-WAL, persistence-format, or
/// provider errors, or — on the legacy MANIFEST-absent path only — a
/// [`PersistError::WalSnapshotMismatch`] when the WAL does not extend the
/// applied snapshot epoch. Acquiring the shared persistence guard may create
/// the permanent `MANIFEST.lock` coordination file.
#[tracing::instrument(name = "selene.persist.recover", skip(registry), fields(dir = %dir.display()))]
pub fn recover(dir: &Path, registry: &ProviderRegistry) -> PersistResult<RecoveryOutcome> {
    let guard = PersistenceReadGuard::acquire(dir)?;
    recover_guarded(&guard, registry)
}

/// Recover through an already-held shared persistence epoch guard.
///
/// This is the composable counterpart to [`recover`]. It lets callers that
/// already own a [`PersistenceReadGuard`] keep one authoritative epoch pinned
/// across related reads. The guard remains held through snapshot verification,
/// every provider callback, and complete WAL replay, so rotation and prune
/// cannot replace or delete selected artifacts midway through recovery.
/// Ordinary WAL appends may continue; replay observes a valid framed prefix.
///
/// Recovery-provider callbacks must not invoke same-directory checkpoint,
/// rotation, prune, or direct MANIFEST publication while this function runs,
/// because those operations require the exclusive side of the held lock.
///
/// # Errors
///
/// Returns non-regular-WAL, persistence-format, or provider errors, or — on the
/// legacy MANIFEST-absent path only — [`PersistError::WalSnapshotMismatch`]
/// when the WAL does not extend the applied snapshot epoch.
#[tracing::instrument(
    name = "selene.persist.recover_guarded",
    skip(guard, registry),
    fields(dir = %guard.dir().display())
)]
pub fn recover_guarded(
    guard: &PersistenceReadGuard,
    registry: &ProviderRegistry,
) -> PersistResult<RecoveryOutcome> {
    let started = Instant::now();
    let dir = guard.directory();
    dir.require_legacy()?;
    require_regular_wal_or_absent(dir, Path::new(DEFAULT_WAL_FILE_NAME))?;
    let mut outcome = RecoveryOutcome::empty();
    let mut providers_invoked = BTreeSet::new();
    let mut snapshot_providers_invoked = BTreeSet::new();

    // Step 0: the MANIFEST, if present, is authoritative.
    let manifest = guard.read_manifest()?;
    outcome.manifest_present = manifest.is_some();

    // The active-WAL name is a de-facto fixed contract: every producer
    // (`rotate_with_manifest`) writes `DEFAULT_WAL_FILE_NAME`, and recovery opens
    // the active WAL by that name (`replay_wal`). Rather than silently ignoring a
    // divergent `manifest.active_wal`, assert the contract so a future producer
    // that records a different name fails loudly instead of recovering the wrong
    // file. Honouring an arbitrary name is deferred until a producer needs it.
    if let Some(manifest) = &manifest
        && manifest.active_wal != DEFAULT_WAL_FILE_NAME
    {
        return Err(PersistError::UnexpectedActiveWal {
            observed: manifest.active_wal.clone(),
            expected: DEFAULT_WAL_FILE_NAME,
        });
    }

    // Step 1: snapshot apply.
    outcome.applied_snapshot_seq = match &manifest {
        Some(manifest) => apply_manifest_snapshot(
            dir,
            manifest,
            registry,
            &mut providers_invoked,
            &mut snapshot_providers_invoked,
        )?,
        None => apply_snapshot(
            dir,
            registry,
            &mut providers_invoked,
            &mut snapshot_providers_invoked,
        )?,
    };
    outcome.last_wal_seq = outcome.applied_snapshot_seq;

    // Step 2: WAL replay. The cross-check is only honoured on the legacy path —
    // with a MANIFEST the live_snapshot_seq is the source of truth.
    let cross_check = manifest.is_none();
    replay_wal(
        dir,
        registry,
        cross_check,
        &mut outcome,
        &mut providers_invoked,
    )?;
    outcome.providers_invoked = providers_invoked.into_iter().collect();
    outcome.snapshot_providers_invoked = snapshot_providers_invoked.into_iter().collect();
    metrics::counter_inc(metrics::RECOVERIES_TOTAL);
    metrics::histogram_record(
        metrics::RECOVERY_DURATION_SECONDS,
        started.elapsed().as_secs_f64(),
    );
    Ok(outcome)
}

/// Apply the snapshot named by a committed MANIFEST.
///
/// Opens `snapshot.{live_snapshot_seq}.snap` directly rather than scanning for
/// the highest on-disk snapshot, so an orphan snapshot (seq > live) produced by
/// a crash between Phase 1 and Phase 3 is ignored by construction. A
/// `live_snapshot_seq` of `0` means "no snapshot for this epoch" — nothing is
/// applied and WAL replay starts from the beginning.
fn apply_manifest_snapshot(
    dir: &crate::StoreDirectory,
    manifest: &Manifest,
    registry: &ProviderRegistry,
    providers_invoked: &mut BTreeSet<[u8; 4]>,
    snapshot_providers_invoked: &mut BTreeSet<[u8; 4]>,
) -> PersistResult<u64> {
    if manifest.live_snapshot_seq == 0 {
        return Ok(0);
    }
    let path = snapshot_path(Path::new(""), manifest.live_snapshot_seq);
    route_snapshot_sections(
        dir,
        &path,
        registry,
        providers_invoked,
        snapshot_providers_invoked,
    )?;
    Ok(manifest.live_snapshot_seq)
}

/// Open a snapshot, verify its body hash, and route every section to its
/// provider. Shared by the legacy and MANIFEST snapshot-apply paths.
fn route_snapshot_sections(
    dir: &crate::StoreDirectory,
    path: &Path,
    registry: &ProviderRegistry,
    providers_invoked: &mut BTreeSet<[u8; 4]>,
    snapshot_providers_invoked: &mut BTreeSet<[u8; 4]>,
) -> PersistResult<()> {
    let mut reader = SnapshotReader::open_in(dir, path)?;
    reader.verify_body_hash()?;
    // Cheap Arc pointer-bump (not a deep Vec clone) decouples the section
    // iteration from the `&mut reader` payload reads inside the loop body.
    let sections = reader.sections_arc();
    for (index, entry) in sections.iter().copied().enumerate() {
        let provider = registry
            .lookup(entry.provider)
            .ok_or(PersistError::UnknownProvider {
                provider: entry.provider,
                sub: entry.sub,
            })?;
        let bytes = reader.read_section_at(index)?;
        provider.read_section(entry.sub, &bytes).map_err(|source| {
            PersistError::ProviderFailed {
                provider: entry.provider,
                sub: Some(entry.sub),
                source,
            }
        })?;
        providers_invoked.insert(entry.provider);
        snapshot_providers_invoked.insert(entry.provider);
    }
    Ok(())
}

fn apply_snapshot(
    dir: &crate::StoreDirectory,
    registry: &ProviderRegistry,
    providers_invoked: &mut BTreeSet<[u8; 4]>,
    snapshot_providers_invoked: &mut BTreeSet<[u8; 4]>,
) -> PersistResult<u64> {
    let Some((snapshot_seq, path)) = crate::find_latest_snapshot_in(dir)? else {
        return Ok(0);
    };
    route_snapshot_sections(
        dir,
        &path,
        registry,
        providers_invoked,
        snapshot_providers_invoked,
    )?;
    Ok(snapshot_seq)
}

fn replay_wal(
    dir: &crate::StoreDirectory,
    registry: &ProviderRegistry,
    cross_check: bool,
    outcome: &mut RecoveryOutcome,
    providers_invoked: &mut BTreeSet<[u8; 4]>,
) -> PersistResult<()> {
    let wal_path = Path::new(DEFAULT_WAL_FILE_NAME);
    if !dir.contains(wal_path)? {
        return Ok(());
    }

    let reader = WalReader::open_in(dir, wal_path)?;
    // Why: the WAL/snapshot epoch cross-check is the Seam-F hard-fail. It is
    // honoured only on the legacy (MANIFEST-absent) path, where no authoritative
    // epoch exists and a WAL extending a different snapshot is genuinely
    // ambiguous. With a MANIFEST, the commit-point invariant (audit Item 2 /
    // Seam F) makes the epoch atomic and lets the WAL header lag the live
    // snapshot by one epoch after a Phase-3-before-Phase-4 crash, so the
    // cross-check is suppressed and the live_snapshot_seq replay floor governs.
    if cross_check && reader.snapshot_seq() != outcome.applied_snapshot_seq {
        return Err(PersistError::WalSnapshotMismatch {
            wal_snapshot_seq: reader.snapshot_seq(),
            snapshot_seq: outcome.applied_snapshot_seq,
        });
    }

    let applied_snapshot_seq = outcome.applied_snapshot_seq;
    let mut seen_source_seqs: HashMap<NodeId, HashSet<u64>> = HashMap::new();
    let stream = reader.iterate(move |header| header.sequence > applied_snapshot_seq)?;
    for view in stream {
        let view = match view {
            Ok(view) => view,
            Err(PersistError::TruncatedEntry { .. }) => break,
            Err(error) => return Err(error),
        };
        let entry = view.into_entry()?;
        outcome.last_wal_seq = entry.header.sequence;
        if entry.header.is_checkpoint_watermark() {
            continue;
        }
        if skip_replicated_entry(
            entry.header.origin,
            entry.changes.len(),
            &mut seen_source_seqs,
            outcome,
        ) {
            continue;
        }
        outcome.wal_commit_entries_applied = outcome.wal_commit_entries_applied.saturating_add(1);
        if entry.changes.is_empty() {
            continue;
        }
        for provider in registry.iter() {
            let tag = provider.provider_tag();
            provider
                .on_changes(&entry.changes)
                .map_err(|source| PersistError::ProviderFailed {
                    provider: tag,
                    sub: None,
                    source,
                })?;
            providers_invoked.insert(tag);
        }
        outcome.wal_changes_applied = outcome
            .wal_changes_applied
            .saturating_add(entry.changes.len() as u64);
    }
    Ok(())
}

fn skip_replicated_entry(
    origin: Origin,
    change_count: usize,
    seen_source_seqs: &mut HashMap<NodeId, HashSet<u64>>,
    outcome: &mut RecoveryOutcome,
) -> bool {
    let Origin::Replicated {
        source_node_id,
        source_seq,
    } = origin
    else {
        return false;
    };
    if seen_source_seqs
        .entry(source_node_id)
        .or_default()
        .insert(source_seq)
    {
        return false;
    }
    outcome.replicated_changes_deduplicated = outcome
        .replicated_changes_deduplicated
        .saturating_add(change_count as u64);
    true
}

#[cfg(test)]
mod tests;
