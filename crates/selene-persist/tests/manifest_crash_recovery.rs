//! On-disk crash-point recovery tests for BRIEF-148 (MANIFEST + crash-safe
//! rotate + 3-step recovery, audit Item 2 / Seam F).
//!
//! Every test here constructs a *real* on-disk crash state — the exact bytes a
//! power-loss would leave at a given rotation phase boundary — and asserts that
//! [`selene_persist::recover`] converges to the correct epoch with the correct
//! replayed change set and no hard-fail. The MANIFEST commit (Phase 3) is the
//! linearization point: nothing destructive runs before it, and the WAL reset
//! (Phase 4) runs only after it.

#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{Change, HlcTimestamp, NodeId, Origin};
use selene_persist::{
    DEFAULT_WAL_FILE_NAME, Manifest, PersistError, ProviderRegistry, RecoveryProvider,
    RecoveryResult, SectionCompression, SnapshotBuilder, SnapshotConfig, SyncPolicy, WalConfig,
    WalReader, WalWriter, recover, snapshot_path,
};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-manifest-crash-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("temp dir created");
    dir.canonicalize().expect("temp dir canonicalizes")
}

#[derive(Clone, Debug, PartialEq)]
enum Event {
    Section { sub: [u8; 4], bytes: Vec<u8> },
    Change(Box<Change>),
}

struct Recorder {
    tag: [u8; 4],
    events: Mutex<Vec<Event>>,
}

impl Recorder {
    fn new(tag: [u8; 4]) -> Arc<Self> {
        Arc::new(Self {
            tag,
            events: Mutex::new(Vec::new()),
        })
    }

    fn changes(&self) -> Vec<Change> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::Change(change) => Some((**change).clone()),
                Event::Section { .. } => None,
            })
            .collect()
    }

    fn sections(&self) -> Vec<([u8; 4], Vec<u8>)> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::Section { sub, bytes } => Some((*sub, bytes.clone())),
                Event::Change(_) => None,
            })
            .collect()
    }
}

impl RecoveryProvider for Recorder {
    fn provider_tag(&self) -> [u8; 4] {
        self.tag
    }

    fn read_section(&self, sub: [u8; 4], bytes: &[u8]) -> RecoveryResult<()> {
        self.events.lock().unwrap().push(Event::Section {
            sub,
            bytes: bytes.to_vec(),
        });
        Ok(())
    }

    fn on_change(&self, change: &Change) -> RecoveryResult<()> {
        self.events
            .lock()
            .unwrap()
            .push(Event::Change(Box::new(change.clone())));
        Ok(())
    }
}

fn registry(provider: &Arc<Recorder>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    let provider: Arc<dyn RecoveryProvider> = provider.clone();
    registry.register(provider).unwrap();
    registry
}

fn node_change(id: u64) -> Change {
    Change::NodeDeleted {
        id: NodeId::new(id),
    }
}

/// Append `id` as a single-change WAL entry, returning the assigned sequence.
fn append(writer: &mut WalWriter, id: u64) -> u64 {
    writer
        .append(
            HlcTimestamp::new(id, 0),
            Origin::Local,
            None,
            &[node_change(id)],
        )
        .expect("append succeeds")
}

/// Open a WAL writer seeded at `snapshot_seq` with durability-by-default.
fn open_wal(dir: &Path, snapshot_seq: u64) -> WalWriter {
    WalWriter::open(
        &dir.join(DEFAULT_WAL_FILE_NAME),
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq,
        },
    )
    .expect("wal opens")
}

/// Build + finalize `snapshot.{seq}.snap` carrying one CORE/META section.
fn finalize_snapshot(dir: &Path, seq: u64, meta: &[u8]) {
    let mut builder = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.to_path_buf(),
        sequence: seq,
        compression: SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    builder
        .add_section(*b"CORE", *b"META", meta.to_vec())
        .unwrap();
    let outcome = builder.finalize().expect("snapshot finalizes");
    assert_eq!(outcome.snapshot_seq, seq);
}

fn meta_builder(dir: &Path, seq: u64, meta: &[u8]) -> SnapshotBuilder {
    let mut builder = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.to_path_buf(),
        sequence: seq,
        compression: SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    builder
        .add_section(*b"CORE", *b"META", meta.to_vec())
        .unwrap();
    builder
}

fn manifest_for(live: u64, header_seq: u64, archived: Vec<u64>) -> Manifest {
    Manifest {
        live_snapshot_seq: live,
        active_wal_header_seq: header_seq,
        compaction_epoch: 0,
        active_wal: DEFAULT_WAL_FILE_NAME.to_owned(),
        archived_wal_seqs: archived,
    }
}

// ---------------------------------------------------------------------------
// Crash point 1: after Phase 1 (snapshot N published), before Phase 3 (commit).
// On disk: snapshot.N.snap (orphan) + OLD MANIFEST naming N-1 + intact wal.log
// extending N-1. Recovery must apply N-1, replay the intact wal.log, and ignore
// the orphan N. No data loss, no hard-fail.
// ---------------------------------------------------------------------------
#[test]
fn first_rotation_phase2_failure_ignores_orphan_under_baseline_manifest() {
    let dir = temp_dir("first-phase2-failure");
    let mut writer = open_wal(&dir, 0);
    assert_eq!(append(&mut writer, 1), 1);
    assert_eq!(append(&mut writer, 2), 2);

    // Exhaust the archive temporary paths so Phase 2 fails deterministically
    // after Phase 1 publishes snapshot 2. The first rotation must already have
    // bootstrapped MANIFEST(0), making the new snapshot an ignored orphan.
    let archive = dir.join("wal.2.archive");
    for attempt in 0..128_u8 {
        let tmp = archive.with_extension(format!("archive.tmp.{}.{attempt}", std::process::id()));
        fs::write(tmp, b"occupied").unwrap();
    }
    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 2, b"orphan2"))
        .expect_err("Phase 2 fails deterministically");
    assert!(matches!(
        error,
        PersistError::Io(source) if source.kind() == std::io::ErrorKind::AlreadyExists
    ));
    let baseline = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(baseline.live_snapshot_seq, 0);
    assert_eq!(baseline.active_wal_header_seq, 0);
    assert!(baseline.archived_wal_seqs.is_empty());
    assert!(snapshot_path(&dir, 2).exists());

    // The failed rotation does not poison the writer, and recovery follows the
    // baseline instead of applying the orphan through the legacy fallback.
    assert_eq!(append(&mut writer, 3), 3);
    writer.flush().unwrap();
    drop(writer);
    let core = Recorder::new(*b"CORE");
    let recovered = recover(&dir, &registry(&core)).unwrap();
    assert!(recovered.manifest_present);
    assert_eq!(recovered.applied_snapshot_seq, 0);
    assert_eq!(recovered.last_wal_seq, 3);
    assert!(core.sections().is_empty());
    assert_eq!(
        core.changes(),
        vec![node_change(1), node_change(2), node_change(3)]
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn first_rotation_rejects_nonzero_baseline_without_snapshot() {
    let dir = temp_dir("missing-baseline-snapshot");
    let mut writer = open_wal(&dir, 5);
    assert_eq!(append(&mut writer, 6), 6);

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 6, b"epoch6"))
        .expect_err("missing epoch-5 baseline snapshot rejects");
    assert!(matches!(
        error,
        PersistError::CommittedSnapshotUnavailable { ref path }
            if path == &snapshot_path(&dir, 5)
    ));
    assert!(Manifest::read(&dir).unwrap().is_none());
    assert!(!snapshot_path(&dir, 6).exists());
    assert!(!dir.join("wal.6.archive").exists());
    assert_eq!(append(&mut writer, 7), 7);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn crash_after_phase1_before_commit_recovers_previous_epoch_and_ignores_orphan() {
    let dir = temp_dir("phase1-orphan");
    // Epoch N-1 = 5 is live: snapshot 5 + MANIFEST(5) + wal.log extending 5.
    finalize_snapshot(&dir, 5, b"epoch5");
    manifest_for(5, 5, vec![]).write_atomic(&dir).unwrap();
    {
        let mut writer = open_wal(&dir, 5);
        assert_eq!(append(&mut writer, 6), 6);
        assert_eq!(append(&mut writer, 7), 7);
        writer.flush().unwrap();
    }
    // Crash leaves an orphan snapshot 7 (Phase 1 ran, Phase 3 never did).
    finalize_snapshot(&dir, 7, b"orphan7");

    let core = Recorder::new(*b"CORE");
    let outcome = recover(&dir, &registry(&core)).unwrap();

    assert!(outcome.manifest_present);
    assert_eq!(outcome.applied_snapshot_seq, 5);
    assert_eq!(outcome.last_wal_seq, 7);
    // The applied snapshot is the live one (5), NOT the orphan (7).
    assert_eq!(core.sections(), vec![(*b"META", b"epoch5".to_vec())]);
    // wal.log entries 6 and 7 replay (> live floor 5).
    assert_eq!(core.changes(), vec![node_change(6), node_change(7)]);
    let _ = fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Crash point 2: after Phase 3 (MANIFEST N committed), before Phase 4 (reset).
// On disk: MANIFEST(N) + snapshot.N.snap + un-reset wal.log whose header still
// says N-1 and which still carries entries <= N (and may carry fresh > N
// appends). Recovery applies N and replays only entries > N; the <= N entries
// are filtered out by the > floor predicate. No hard-fail (cross-check relaxed).
// ---------------------------------------------------------------------------
#[test]
fn crash_after_commit_before_reset_recovers_new_epoch_and_filters_stale_entries() {
    let dir = temp_dir("phase4-unreset");
    // Epoch N-1 = 5 live, wal.log carries 6,7 extending snapshot 5.
    finalize_snapshot(&dir, 5, b"epoch5");
    {
        let mut writer = open_wal(&dir, 5);
        assert_eq!(append(&mut writer, 6), 6);
        assert_eq!(append(&mut writer, 7), 7);
        writer.flush().unwrap();
    }
    // Rotation advanced to N = 7: snapshot 7 published, MANIFEST(7) committed,
    // but the wal.log reset (Phase 4) never ran — its header still says 5 and
    // it still carries entries 6,7.
    finalize_snapshot(&dir, 7, b"epoch7");
    manifest_for(7, 7, vec![7]).write_atomic(&dir).unwrap();

    let core = Recorder::new(*b"CORE");
    let outcome = recover(&dir, &registry(&core)).unwrap();

    assert!(outcome.manifest_present);
    assert_eq!(outcome.applied_snapshot_seq, 7);
    // No WAL entry is > 7, so nothing replays; the stale 6,7 are filtered out.
    assert_eq!(outcome.last_wal_seq, 7);
    assert_eq!(outcome.wal_changes_applied, 0);
    assert_eq!(core.sections(), vec![(*b"META", b"epoch7".to_vec())]);
    assert!(core.changes().is_empty());
    let _ = fs::remove_dir_all(dir);
}

// As above, but with fresh post-rotation appends (seq > N) in the un-reset
// wal.log. They must replay cleanly on top of snapshot N.
#[test]
fn crash_after_commit_before_reset_replays_post_rotation_appends() {
    let dir = temp_dir("phase4-with-appends");
    finalize_snapshot(&dir, 5, b"epoch5");
    {
        let mut writer = open_wal(&dir, 5);
        append(&mut writer, 6);
        append(&mut writer, 7);
        // Post-rotation appends 8,9 appended to the still-un-reset wal.log.
        append(&mut writer, 8);
        append(&mut writer, 9);
        writer.flush().unwrap();
    }
    finalize_snapshot(&dir, 7, b"epoch7");
    manifest_for(7, 7, vec![7]).write_atomic(&dir).unwrap();

    let core = Recorder::new(*b"CORE");
    let outcome = recover(&dir, &registry(&core)).unwrap();

    assert_eq!(outcome.applied_snapshot_seq, 7);
    assert_eq!(outcome.last_wal_seq, 9);
    assert_eq!(outcome.wal_changes_applied, 2);
    // Only 8,9 (> floor 7) replay; 6,7 are filtered out.
    assert_eq!(core.changes(), vec![node_change(8), node_change(9)]);
    let _ = fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Crash point 3: mid-MANIFEST-write. A partial/garbage MANIFEST.tmp is on disk
// alongside an intact committed MANIFEST(N-1). Recovery uses the previous epoch;
// the tmp is never consulted.
// ---------------------------------------------------------------------------
#[test]
fn crash_mid_manifest_write_uses_previous_committed_manifest() {
    let dir = temp_dir("mid-write");
    finalize_snapshot(&dir, 5, b"epoch5");
    manifest_for(5, 5, vec![]).write_atomic(&dir).unwrap();
    {
        let mut writer = open_wal(&dir, 5);
        append(&mut writer, 6);
        writer.flush().unwrap();
    }
    // A crash mid-rename leaves a partial tmp; the committed MANIFEST(5) stands.
    fs::write(
        dir.join("MANIFEST.tmp"),
        b"SLMF\x01\x00\x00\x00garbage-half-written",
    )
    .unwrap();

    let core = Recorder::new(*b"CORE");
    let outcome = recover(&dir, &registry(&core)).unwrap();

    assert!(outcome.manifest_present);
    assert_eq!(outcome.applied_snapshot_seq, 5);
    assert_eq!(outcome.last_wal_seq, 6);
    assert_eq!(core.sections(), vec![(*b"META", b"epoch5".to_vec())]);
    assert_eq!(core.changes(), vec![node_change(6)]);
    let _ = fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Seam-F regression: the EXACT scenario that used to hard-fail
// (snapshot N present, wal.log header says N-1) now AUTO-RECONCILES because a
// MANIFEST names N as live. This is the headline bug fix.
// ---------------------------------------------------------------------------
#[test]
fn seam_f_scenario_auto_reconciles_with_manifest() {
    let dir = temp_dir("seam-f-fixed");
    // snapshot 50 on disk, wal.log header says 50 too (it extends 50), and a
    // higher snapshot 100 exists. Pre-MANIFEST this combination + a wal header
    // lagging the live snapshot was the hard-fail. With MANIFEST(100) it
    // reconciles: live = 100, wal entries <= 100 filtered, header lag ignored.
    finalize_snapshot(&dir, 50, b"epoch50");
    finalize_snapshot(&dir, 100, b"epoch100");
    {
        // wal.log header carries 50 — one epoch behind the live snapshot 100.
        let mut writer = open_wal(&dir, 50);
        append(&mut writer, 51);
        writer.flush().unwrap();
    }
    manifest_for(100, 100, vec![100])
        .write_atomic(&dir)
        .unwrap();

    let core = Recorder::new(*b"CORE");
    let outcome = recover(&dir, &registry(&core)).expect("auto-reconciles, no hard-fail");

    assert!(outcome.manifest_present);
    assert_eq!(outcome.applied_snapshot_seq, 100);
    // Entry 51 is <= live floor 100, filtered out.
    assert_eq!(outcome.wal_changes_applied, 0);
    assert_eq!(core.sections(), vec![(*b"META", b"epoch100".to_vec())]);
    let _ = fs::remove_dir_all(dir);
}

// Legacy honesty: the same shape WITHOUT a MANIFEST still hard-fails, so the
// fallback path stays strict where no authoritative epoch exists.
#[test]
fn seam_f_scenario_without_manifest_still_hard_fails() {
    let dir = temp_dir("seam-f-legacy");
    finalize_snapshot(&dir, 50, b"epoch50");
    finalize_snapshot(&dir, 100, b"epoch100");
    {
        let mut writer = open_wal(&dir, 50); // header says 50, find_latest picks 100
        append(&mut writer, 51);
        writer.flush().unwrap();
    }
    // No MANIFEST written.
    let core = Recorder::new(*b"CORE");
    let err = recover(&dir, &registry(&core)).unwrap_err();
    assert!(matches!(
        err,
        PersistError::WalSnapshotMismatch {
            wal_snapshot_seq: 50,
            snapshot_seq: 100
        }
    ));
    let _ = fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Full happy-path rotate via the orchestrator, then recover. Asserts the new
// epoch is authoritative and the active wal.log was reset to N.
// ---------------------------------------------------------------------------
#[test]
fn rotate_with_manifest_commits_epoch_and_recovers() {
    let dir = temp_dir("rotate-happy");
    let mut writer = open_wal(&dir, 0);
    append(&mut writer, 1);
    append(&mut writer, 2);

    let outcome = writer
        .rotate_with_manifest(meta_builder(&dir, writer.last_sequence(), b"snap2"))
        .expect("rotate commits");
    assert_eq!(outcome.snapshot_sequence(), 2);
    assert_eq!(
        outcome.archived_path(),
        Some(dir.join("wal.2.archive").as_path())
    );
    assert_eq!(writer.snapshot_seq(), 2);
    assert_eq!(writer.last_sequence(), 2);

    // The MANIFEST names epoch 2 and records the archive.
    let manifest = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(manifest.live_snapshot_seq, 2);
    assert_eq!(manifest.active_wal_header_seq, 2);
    assert_eq!(manifest.archived_wal_seqs, vec![2]);
    assert!(snapshot_path(&dir, 2).exists());

    // The active wal.log was reset to header seq 2 and is empty of entries.
    assert_eq!(
        WalReader::open(&dir.join(DEFAULT_WAL_FILE_NAME))
            .unwrap()
            .snapshot_seq(),
        2
    );

    // A post-rotation append continues from 3.
    assert_eq!(append(&mut writer, 3), 3);
    writer.flush().unwrap();
    drop(writer);

    let core = Recorder::new(*b"CORE");
    let recovered = recover(&dir, &registry(&core)).unwrap();
    assert!(recovered.manifest_present);
    assert_eq!(recovered.applied_snapshot_seq, 2);
    assert_eq!(recovered.last_wal_seq, 3);
    assert_eq!(core.sections(), vec![(*b"META", b"snap2".to_vec())]);
    assert_eq!(core.changes(), vec![node_change(3)]);
    let _ = fs::remove_dir_all(dir);
}

// Two sequential rotations: the second MANIFEST must carry BOTH archives so
// retention (Item-5) has the full history.
#[test]
fn rotate_with_manifest_accumulates_archive_history() {
    let dir = temp_dir("rotate-history");
    let mut writer = open_wal(&dir, 0);
    append(&mut writer, 1);
    writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"s1"))
        .unwrap();
    append(&mut writer, 2);
    writer
        .rotate_with_manifest(meta_builder(&dir, 2, b"s2"))
        .unwrap();

    let manifest = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(manifest.live_snapshot_seq, 2);
    assert_eq!(manifest.archived_wal_seqs, vec![1, 2]);
    assert!(dir.join("wal.1.archive").exists());
    assert!(dir.join("wal.2.archive").exists());
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Idempotent re-rotate: simulate a crash after Phase 1+2 (snapshot N + archive
// published) but before Phase 3, then re-invoke the orchestrator. It must
// converge to epoch N after exact snapshot/archive identity checks.
// ---------------------------------------------------------------------------
#[test]
fn rotate_with_manifest_is_idempotent_after_partial_crash() {
    let dir = temp_dir("idempotent");
    // Live epoch 0 (no MANIFEST yet), wal.log carries 1,2.
    let mut writer = open_wal(&dir, 0);
    append(&mut writer, 1);
    append(&mut writer, 2);
    writer.flush().unwrap();

    // Simulate Phase 1 + Phase 2 having already run before the crash:
    // snapshot.2.snap published and wal.2.archive published. Re-invoking the
    // orchestrator must verify and accept both byte-identical artifacts.
    meta_builder(&dir, 2, b"snap2")
        .finalize_with_authority(writer.authority())
        .unwrap();
    fs::copy(dir.join(DEFAULT_WAL_FILE_NAME), dir.join("wal.2.archive")).unwrap();

    // Re-invoke the full rotate: Phase 1 and Phase 2 compare the existing files
    // with their new temporaries, Phase 3 commits MANIFEST(2), Phase 4 resets.
    let outcome = writer
        .rotate_with_manifest(meta_builder(&dir, 2, b"snap2"))
        .expect("re-rotate converges");
    assert_eq!(outcome.snapshot_sequence(), 2);
    assert_eq!(writer.snapshot_seq(), 2);

    let manifest = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(manifest.live_snapshot_seq, 2);

    drop(writer);
    let core = Recorder::new(*b"CORE");
    let recovered = recover(&dir, &registry(&core)).unwrap();
    assert_eq!(recovered.applied_snapshot_seq, 2);
    assert_eq!(core.sections(), vec![(*b"META", b"snap2".to_vec())]);
    let _ = fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Cross-crate seam: recover after a Phase-3-before-Phase-4 crash, reopen a live
// writer (as selene-graph does), append, and verify the next append cannot
// regress below or collide with the recovered tip, and that a fresh rotate then
// converges.
// ---------------------------------------------------------------------------
#[test]
fn recover_after_partial_rotate_then_append_does_not_regress() {
    let dir = temp_dir("partial-then-append");
    finalize_snapshot(&dir, 5, b"epoch5");
    {
        let mut writer = open_wal(&dir, 5);
        append(&mut writer, 6);
        append(&mut writer, 7);
        writer.flush().unwrap();
    }
    // Phase 3 committed MANIFEST(7) + snapshot 7, Phase 4 never reset wal.log.
    finalize_snapshot(&dir, 7, b"epoch7");
    manifest_for(7, 7, vec![7]).write_atomic(&dir).unwrap();

    // Recover (the selene-graph pattern: recover, then bump generation to
    // last_wal_seq, then reopen a fresh live writer).
    let core = Recorder::new(*b"CORE");
    let recovered = recover(&dir, &registry(&core)).unwrap();
    assert_eq!(recovered.applied_snapshot_seq, 7);
    assert_eq!(recovered.last_wal_seq, 7);

    // Reopen the WAL as a live writer (WalConfig::default, like recover_inner).
    // The on-disk header still says 5, but it carries entries up to seq 7, so
    // scan_existing seeds last_sequence from the max(header, scanned) = 7.
    let mut writer =
        WalWriter::open(&dir.join(DEFAULT_WAL_FILE_NAME), WalConfig::default()).unwrap();
    assert_eq!(writer.last_sequence(), 7);
    // Next append must be 8 — strictly above the recovered tip, no regression.
    assert_eq!(append(&mut writer, 8), 8);

    // A fresh rotate from here converges to epoch 8.
    let outcome = writer
        .rotate_with_manifest(meta_builder(&dir, 8, b"epoch8"))
        .expect("rotate after partial-recovery append");
    assert_eq!(outcome.snapshot_sequence(), 8);
    let manifest = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(manifest.live_snapshot_seq, 8);
    // The archive history accumulates the earlier 7 + the new 8.
    assert_eq!(manifest.archived_wal_seqs, vec![7, 8]);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

// rotate_with_manifest rejects a builder whose sequence does not cover the WAL.
#[test]
fn rotate_with_manifest_rejects_sequence_mismatch_without_committing() {
    let dir = temp_dir("seq-mismatch");
    let mut writer = open_wal(&dir, 0);
    append(&mut writer, 1);
    append(&mut writer, 2);

    let err = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"stale"))
        .expect_err("stale snapshot sequence rejects");
    assert!(matches!(
        err,
        PersistError::WalRotationSequenceMismatch {
            snapshot_seq: 1,
            last_sequence: 2
        }
    ));
    // No MANIFEST committed; the writer is still usable.
    assert!(Manifest::read(&dir).unwrap().is_none());
    assert_eq!(append(&mut writer, 3), 3);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

// MANIFEST naming live_snapshot_seq = 0 (WAL-only epoch): no snapshot applied,
// WAL replays from the start.
#[test]
fn manifest_zero_live_snapshot_replays_full_wal() {
    let dir = temp_dir("zero-live");
    {
        let mut writer = open_wal(&dir, 0);
        append(&mut writer, 1);
        append(&mut writer, 2);
        writer.flush().unwrap();
    }
    manifest_for(0, 0, vec![]).write_atomic(&dir).unwrap();

    let core = Recorder::new(*b"CORE");
    let outcome = recover(&dir, &registry(&core)).unwrap();
    assert!(outcome.manifest_present);
    assert_eq!(outcome.applied_snapshot_seq, 0);
    assert_eq!(outcome.last_wal_seq, 2);
    assert!(core.sections().is_empty());
    assert_eq!(core.changes(), vec![node_change(1), node_change(2)]);
    let _ = fs::remove_dir_all(dir);
}

// A MANIFEST pointing at a snapshot that does not exist on disk is a hard error
// (the live snapshot must be present — this is a genuinely corrupt directory,
// not a recoverable crash state).
#[test]
fn manifest_naming_missing_snapshot_is_hard_error() {
    let dir = temp_dir("missing-snap");
    manifest_for(9, 9, vec![]).write_atomic(&dir).unwrap();
    let core = Recorder::new(*b"CORE");
    let err = recover(&dir, &registry(&core)).unwrap_err();
    assert!(matches!(err, PersistError::Io(_)));
    let _ = fs::remove_dir_all(dir);
}
