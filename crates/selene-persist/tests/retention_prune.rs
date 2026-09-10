//! End-to-end retention proof for BRIEF-Item-5 (RetentionPolicy + MANIFEST-atomic
//! prune, audit Item 5 / D26).
//!
//! Where the unit tests in `src/retention/tests.rs` exercise the planner against
//! synthetic byte files, these tests build a *real* persistence directory via the
//! rotate orchestrator (genuine snapshots + WAL archives + committed MANIFEST),
//! prune it, and then run [`selene_persist::recover`] to prove the load-bearing
//! safety invariant: **prune never deletes anything recovery needs.** The
//! recovered epoch after a prune is byte-identical to the epoch before it.

#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{Change, HlcTimestamp, NodeId, Origin};
use selene_persist::{
    DEFAULT_WAL_FILE_NAME, Manifest, ProviderRegistry, RecoveryProvider, RecoveryResult,
    RetentionPolicy, SectionCompression, SnapshotBuilder, SnapshotConfig, SyncPolicy, WalConfig,
    WalWriter, recover, snapshot_path,
};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-retention-e2e-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("temp dir created");
    dir.canonicalize().expect("temp dir canonicalizes")
}

struct Recorder {
    tag: [u8; 4],
    sections: Mutex<Vec<([u8; 4], Vec<u8>)>>,
    changes: Mutex<Vec<Change>>,
}

impl Recorder {
    fn new(tag: [u8; 4]) -> Arc<Self> {
        Arc::new(Self {
            tag,
            sections: Mutex::new(Vec::new()),
            changes: Mutex::new(Vec::new()),
        })
    }
}

impl RecoveryProvider for Recorder {
    fn provider_tag(&self) -> [u8; 4] {
        self.tag
    }

    fn read_section(&self, sub: [u8; 4], bytes: &[u8]) -> RecoveryResult<()> {
        self.sections.lock().unwrap().push((sub, bytes.to_vec()));
        Ok(())
    }

    fn on_change(&self, change: &Change) -> RecoveryResult<()> {
        self.changes.lock().unwrap().push(change.clone());
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

/// Roll the directory forward through `rotations` epochs, returning a live writer
/// positioned after the last rotation. After this, the directory holds snapshots
/// `1..=rotations`, archives `1..=rotations`, and a MANIFEST naming epoch
/// `rotations`.
fn build_epochs(dir: &Path, rotations: u64) -> WalWriter {
    let mut writer = open_wal(dir, 0);
    for seq in 1..=rotations {
        append(&mut writer, seq);
        writer
            .rotate_with_manifest(meta_builder(dir, seq, format!("snap{seq}").as_bytes()))
            .expect("rotate commits");
    }
    writer
}

#[test]
fn recover_after_default_prune_yields_identical_live_epoch() {
    let dir = temp_dir("recover-after-prune");
    let mut writer = build_epochs(&dir, 5);
    // A post-rotation append lives in the active wal.log (replays after recovery).
    append(&mut writer, 6);
    writer.flush().unwrap();

    // Pre-prune sanity: five snapshots + five archives + a MANIFEST at epoch 5.
    for seq in 1..=5 {
        assert!(snapshot_path(&dir, seq).exists());
        assert!(dir.join(format!("wal.{seq}.archive")).exists());
    }

    let outcome = writer.prune(&RetentionPolicy::default()).unwrap();
    // keep 2 snapshots → {4,5}; keep 4 archives → {2,3,4,5}.
    assert_eq!(outcome.retained_snapshots, vec![4, 5]);
    assert_eq!(outcome.deleted_snapshots, vec![1, 2, 3]);
    assert_eq!(outcome.retained_wal_archives, vec![2, 3, 4, 5]);
    assert_eq!(outcome.deleted_wal_archives, vec![1]);
    assert!(outcome.bytes_reclaimed > 0);

    // The live snapshot + active wal.log survive; the pruned files are gone.
    assert!(snapshot_path(&dir, 5).exists());
    assert!(dir.join(DEFAULT_WAL_FILE_NAME).exists());
    assert!(!snapshot_path(&dir, 1).exists());
    assert!(!dir.join("wal.1.archive").exists());
    drop(writer);

    // The headline invariant: recovery after the prune reproduces epoch 5 exactly.
    let core = Recorder::new(*b"CORE");
    let recovered = recover(&dir, &registry(&core)).unwrap();
    assert!(recovered.manifest_present);
    assert_eq!(recovered.applied_snapshot_seq, 5);
    assert_eq!(recovered.last_wal_seq, 6);
    assert_eq!(
        *core.sections.lock().unwrap(),
        vec![(*b"META", b"snap5".to_vec())]
    );
    // Only entry 6 (> live floor 5) replays.
    assert_eq!(*core.changes.lock().unwrap(), vec![node_change(6)]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_then_rotate_then_recover_still_converges() {
    let dir = temp_dir("prune-rotate-recover");
    let mut writer = build_epochs(&dir, 3);

    // Prune mid-life, then keep operating: append + a fresh rotation.
    writer.prune(&RetentionPolicy::default()).unwrap();
    append(&mut writer, 4);
    writer
        .rotate_with_manifest(meta_builder(&dir, 4, b"snap4"))
        .expect("rotate after prune");
    append(&mut writer, 5);
    writer.flush().unwrap();

    // The archive history continues to accumulate atop the pruned set.
    let manifest = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(manifest.live_snapshot_seq, 4);
    assert!(manifest.archived_wal_seqs.contains(&4));
    drop(writer);

    let core = Recorder::new(*b"CORE");
    let recovered = recover(&dir, &registry(&core)).unwrap();
    assert_eq!(recovered.applied_snapshot_seq, 4);
    assert_eq!(recovered.last_wal_seq, 5);
    assert_eq!(
        *core.sections.lock().unwrap(),
        vec![(*b"META", b"snap4".to_vec())]
    );
    assert_eq!(*core.changes.lock().unwrap(), vec![node_change(5)]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn aggressive_prune_keeps_only_live_and_recovers() {
    let dir = temp_dir("aggressive");
    let mut writer = build_epochs(&dir, 4);
    append(&mut writer, 5);
    writer.flush().unwrap();

    // Keep nothing but the floor: live snapshot + zero archives.
    let policy = RetentionPolicy {
        keep_n_snapshots: 0,
        keep_n_wal_archives: 0,
        max_total_size_bytes: None,
        time_based: None,
    };
    let outcome = writer.prune(&policy).unwrap();
    assert_eq!(outcome.retained_snapshots, vec![4]);
    assert!(outcome.retained_wal_archives.is_empty());
    drop(writer);

    // Even stripped to the floor, recovery reproduces the live epoch + tail WAL.
    let core = Recorder::new(*b"CORE");
    let recovered = recover(&dir, &registry(&core)).unwrap();
    assert_eq!(recovered.applied_snapshot_seq, 4);
    assert_eq!(recovered.last_wal_seq, 5);
    assert_eq!(
        *core.sections.lock().unwrap(),
        vec![(*b"META", b"snap4".to_vec())]
    );
    assert_eq!(*core.changes.lock().unwrap(), vec![node_change(5)]);
    let _ = fs::remove_dir_all(dir);
}
