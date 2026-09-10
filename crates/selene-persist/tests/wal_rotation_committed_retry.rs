#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{Change, HlcTimestamp, NodeId, Origin};
use selene_persist::{
    DEFAULT_WAL_FILE_NAME, Manifest, PersistError, RetentionPolicy, SectionCompression,
    SnapshotBuilder, SnapshotConfig, SyncPolicy, WalConfig, WalReader, WalWriter, snapshot_path,
};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-wal-committed-retry-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn meta_builder(dir: &Path, sequence: u64, bytes: &[u8]) -> SnapshotBuilder {
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

fn sequences(path: &Path) -> Vec<u64> {
    WalReader::open(path)
        .unwrap()
        .iterate(|_| true)
        .unwrap()
        .map(|entry| entry.unwrap().header.sequence)
        .collect()
}

#[test]
fn legacy_header_current_epoch_bootstraps_manifest_without_archive() {
    let dir = temp_dir("legacy-header-current");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    meta_builder(&dir, 5, b"snapshot-five")
        .finalize()
        .expect("legacy snapshot publishes");
    let mut writer = WalWriter::open(
        &wal_path,
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq: 5,
        },
    )
    .expect("legacy wal opens");

    let outcome = writer
        .rotate_with_manifest(meta_builder(&dir, 5, b"snapshot-five"))
        .expect("legacy current epoch converges");

    assert_eq!(outcome.snapshot_sequence(), 5);
    assert!(outcome.archived_path().is_none());
    assert!(!dir.join("wal.5.archive").exists());
    let manifest = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(manifest.live_snapshot_seq, 5);
    assert!(manifest.archived_wal_seqs.is_empty());
    drop(writer);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn legacy_header_current_symlink_is_rejected_before_manifest_bootstrap() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("legacy-header-current-symlink");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let snapshot = snapshot_path(&dir, 5);
    let backing = dir.join("snapshot-five.backing");
    meta_builder(&dir, 5, b"snapshot-five")
        .finalize()
        .expect("legacy snapshot publishes");
    fs::rename(&snapshot, &backing).expect("snapshot moves behind a symlink");
    symlink(&backing, &snapshot).expect("snapshot path becomes a symlink");
    let mut writer = WalWriter::open(
        &wal_path,
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq: 5,
        },
    )
    .expect("legacy wal opens");

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 5, b"snapshot-five"))
        .expect_err("a symlink cannot become the committed baseline snapshot");

    assert!(matches!(
        error,
        selene_persist::PersistError::CommittedSnapshotUnavailable { ref path }
            if path == &snapshot
    ));
    assert!(Manifest::read(&dir).unwrap().is_none());
    assert!(!dir.join("wal.5.archive").exists());
    drop(writer);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn committed_pre_reset_archive_collision_is_typed_and_poisons() {
    let dir = temp_dir("manifest-committed-foreign-archive");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let archive_path = dir.join("wal.1.archive");
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    append(&mut writer, 1);
    writer.flush().expect("wal flushes");
    meta_builder(&dir, 1, b"snapshot")
        .finalize_with_authority(writer.authority())
        .expect("phase 1 snapshot publishes");
    fs::write(&archive_path, b"foreign archive").expect("foreign archive publishes");
    Manifest {
        live_snapshot_seq: 1,
        active_wal_header_seq: 1,
        compaction_epoch: 0,
        active_wal: DEFAULT_WAL_FILE_NAME.to_owned(),
        archived_wal_seqs: vec![1],
    }
    .write_atomic_with_authority(writer.authority())
    .expect("phase 3 manifest commits");

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect_err("committed foreign archive rejects");

    assert!(matches!(
        error,
        PersistError::CommittedArchiveInvalid { path } if path == archive_path
    ));
    assert!(matches!(
        writer.flush(),
        Err(PersistError::WalWriterPoisoned)
    ));
    let changes = [Change::NodeDeleted { id: NodeId::new(2) }];
    assert!(matches!(
        writer.append(HlcTimestamp::new(2, 0), Origin::Local, None, &changes),
        Err(PersistError::WalWriterPoisoned)
    ));
    drop(writer);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rotation_retry_finishes_reset_after_committed_archive_is_pruned() {
    let dir = temp_dir("manifest-committed-pruned-before-reset");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let archive_path = dir.join("wal.1.archive");
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    append(&mut writer, 1);
    writer.flush().expect("wal flushes");
    meta_builder(&dir, 1, b"snapshot")
        .finalize_with_authority(writer.authority())
        .expect("phase 1 snapshot publishes");
    fs::copy(&wal_path, &archive_path).expect("phase 2 archive publishes");
    Manifest {
        live_snapshot_seq: 1,
        active_wal_header_seq: 1,
        compaction_epoch: 0,
        active_wal: DEFAULT_WAL_FILE_NAME.to_owned(),
        archived_wal_seqs: vec![1],
    }
    .write_atomic_with_authority(writer.authority())
    .expect("phase 3 manifest commits");
    writer
        .prune(&RetentionPolicy {
            keep_n_snapshots: 1,
            keep_n_wal_archives: 0,
            max_total_size_bytes: None,
            time_based: None,
        })
        .expect("sequential retention prunes committed archive");
    assert!(!archive_path.exists());

    let outcome = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect("retry finishes reset without recreating pruned history");

    assert_eq!(outcome.snapshot_sequence(), 1);
    assert!(outcome.archived_path().is_none());
    assert_eq!(writer.snapshot_seq(), 1);
    assert_eq!(sequences(&wal_path), Vec::<u64>::new());
    assert!(!archive_path.exists());
    assert!(
        Manifest::read(&dir)
            .unwrap()
            .unwrap()
            .archived_wal_seqs
            .is_empty()
    );
    drop(writer);
    fs::remove_dir_all(dir).unwrap();
}
