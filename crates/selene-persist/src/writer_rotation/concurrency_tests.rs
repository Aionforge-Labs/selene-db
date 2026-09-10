use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use selene_core::{Change, HlcTimestamp, NodeId, Origin};

use super::set_before_manifest_commit_hook;
use crate::manifest_lock::set_contention_hook;
use crate::retention::{prune_with_authority, set_before_commit_hook};
use crate::{
    DEFAULT_WAL_FILE_NAME, Manifest, RetentionPolicy, SectionCompression, SnapshotBuilder,
    SnapshotConfig, SnapshotReader, SyncPolicy, WalConfig, WalReader, WalWriter, snapshot_path,
};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-prune-rotation-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    dir
}

fn builder(dir: &Path, sequence: u64, bytes: &[u8]) -> SnapshotBuilder {
    let mut builder = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.to_path_buf(),
        sequence,
        compression: SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    builder
        .add_section(*b"CORE", *b"META", bytes.to_vec())
        .unwrap();
    builder
}

fn append(writer: &mut WalWriter, sequence: u64) {
    let changes = [Change::NodeDeleted {
        id: NodeId::new(sequence),
    }];
    assert_eq!(
        writer
            .append(
                HlcTimestamp::new(sequence, 0),
                Origin::Local,
                None,
                &changes,
            )
            .unwrap(),
        sequence
    );
}

fn writer_at_epoch_one(dir: &Path) -> WalWriter {
    let mut writer = WalWriter::open(
        &dir.join(DEFAULT_WAL_FILE_NAME),
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq: 0,
        },
    )
    .unwrap();
    append(&mut writer, 1);
    writer
        .rotate_with_manifest(builder(dir, 1, b"snapshot-one"))
        .unwrap();
    append(&mut writer, 2);
    writer
}

fn wait(rx: &Receiver<()>, phase: &str) {
    rx.recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|error| panic!("timed out waiting for {phase}: {error}"));
}

fn assert_epoch_two(dir: &Path) {
    let manifest = Manifest::read(dir).unwrap().unwrap();
    assert_eq!(manifest.live_snapshot_seq, 2);
    assert_eq!(manifest.active_wal_header_seq, 2);
    assert_eq!(manifest.archived_wal_seqs, vec![2]);
    assert!(snapshot_path(dir, 2).is_file());
    assert!(dir.join("wal.2.archive").is_file());
    assert_eq!(
        WalReader::open(&dir.join(DEFAULT_WAL_FILE_NAME))
            .unwrap()
            .snapshot_seq(),
        2
    );
    let mut snapshot = SnapshotReader::open(&snapshot_path(dir, 2)).unwrap();
    assert_eq!(
        snapshot.read_section(*b"CORE", *b"META").unwrap(),
        b"snapshot-two"
    );
}

#[test]
fn owned_prune_cannot_stale_overwrite_newer_rotation_manifest() {
    let dir = temp_dir("stale-manifest");
    let writer = writer_at_epoch_one(&dir);
    let policy = RetentionPolicy {
        keep_n_snapshots: 1,
        keep_n_wal_archives: 0,
        max_total_size_bytes: None,
        time_based: None,
    };
    let (prune_reached_tx, prune_reached_rx) = sync_channel(0);
    let (resume_prune_tx, resume_prune_rx) = sync_channel(0);
    let prune_authority = writer.authority().clone();
    let prune_thread = thread::spawn(move || {
        set_before_commit_hook(move || {
            prune_reached_tx.send(()).unwrap();
            resume_prune_rx.recv().unwrap();
        });
        prune_with_authority(&prune_authority, &policy)
    });
    wait(&prune_reached_rx, "prune plan");

    let (rotation_contended_tx, rotation_contended_rx) = sync_channel(0);
    let rotation_dir = dir.clone();
    let rotation_thread = thread::spawn(move || {
        set_contention_hook(move || rotation_contended_tx.send(()).unwrap());
        let mut writer = writer;
        writer.rotate_with_manifest(builder(&rotation_dir, 2, b"snapshot-two"))
    });
    wait(&rotation_contended_rx, "rotation lock contention");

    resume_prune_tx.send(()).unwrap();
    let prune_outcome = prune_thread.join().unwrap().unwrap();
    let rotation = rotation_thread.join().unwrap().unwrap();

    assert_eq!(rotation.snapshot_sequence(), 2);
    assert_eq!(prune_outcome.deleted_wal_archives, vec![1]);
    assert!(!prune_outcome.deleted_snapshots.contains(&2));
    assert_epoch_two(&dir);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn owned_prune_cannot_delete_inflight_rotation_artifacts() {
    let dir = temp_dir("inflight-artifacts");
    let writer = writer_at_epoch_one(&dir);
    let prune_authority = writer.authority().clone();
    let (rotation_reached_tx, rotation_reached_rx) = sync_channel(0);
    let (resume_rotation_tx, resume_rotation_rx) = sync_channel(0);
    let rotation_dir = dir.clone();
    let rotation_thread = thread::spawn(move || {
        set_before_manifest_commit_hook(move || {
            rotation_reached_tx.send(()).unwrap();
            resume_rotation_rx.recv().unwrap();
        });
        let mut writer = writer;
        writer.rotate_with_manifest(builder(&rotation_dir, 2, b"snapshot-two"))
    });
    wait(&rotation_reached_rx, "rotation phase two");
    assert!(snapshot_path(&dir, 2).is_file());
    assert!(dir.join("wal.2.archive").is_file());
    assert_eq!(Manifest::read(&dir).unwrap().unwrap().live_snapshot_seq, 1);

    let policy = RetentionPolicy {
        keep_n_snapshots: 1,
        keep_n_wal_archives: 1,
        max_total_size_bytes: None,
        time_based: None,
    };
    let (prune_contended_tx, prune_contended_rx) = sync_channel(0);
    let prune_thread = thread::spawn(move || {
        set_contention_hook(move || prune_contended_tx.send(()).unwrap());
        prune_with_authority(&prune_authority, &policy)
    });
    wait(&prune_contended_rx, "prune lock contention");

    resume_rotation_tx.send(()).unwrap();
    let rotation = rotation_thread.join().unwrap().unwrap();
    let prune_outcome = prune_thread.join().unwrap().unwrap();

    assert_eq!(rotation.snapshot_sequence(), 2);
    assert_eq!(prune_outcome.retained_snapshots, vec![2]);
    assert_eq!(prune_outcome.retained_wal_archives, vec![2]);
    assert_eq!(prune_outcome.deleted_snapshots, vec![1]);
    assert_eq!(prune_outcome.deleted_wal_archives, vec![1]);
    assert_epoch_two(&dir);
    assert!(!snapshot_path(&dir, 1).exists());
    assert!(!dir.join("wal.1.archive").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn lock_obstruction_rejects_rotation_before_flush_or_artifacts() {
    let dir = temp_dir("lock-obstruction");
    std::fs::create_dir(dir.join(crate::MANIFEST_LOCK_FILE_NAME)).unwrap();
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &wal_path,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();
    append(&mut writer, 1);

    let error = writer
        .rotate_with_manifest(builder(&dir, 1, b"snapshot-one"))
        .expect_err("lock obstruction rejects rotation");

    assert!(matches!(
        error,
        crate::PersistError::Directory(crate::DirectoryError::NotRegular(_))
    ));
    assert_eq!(writer.entries_since_fsync(), 1);
    assert!(Manifest::read(&dir).unwrap().is_none());
    assert!(!snapshot_path(&dir, 1).exists());
    assert!(!dir.join("wal.1.archive").exists());

    std::fs::remove_dir(dir.join(crate::MANIFEST_LOCK_FILE_NAME)).unwrap();
    writer
        .rotate_with_manifest(builder(&dir, 1, b"snapshot-one"))
        .expect("writer remains usable after pre-mutation lock error");
    drop(writer);
    std::fs::remove_dir_all(dir).unwrap();
}
