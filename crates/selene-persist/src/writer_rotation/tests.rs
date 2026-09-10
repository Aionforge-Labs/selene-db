use std::fs::{self, OpenOptions};
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{Change, HlcTimestamp, NodeId, Origin};

use super::*;
use crate::{
    RetentionPolicy, SectionCompression, SnapshotBuilder, SnapshotConfig, SyncPolicy, WalConfig,
    WalReader, WalWriter,
};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-wal-reset-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    dir
}

fn open_locked_wal(path: &Path, snapshot_seq: u64) -> File {
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.try_lock().unwrap();
    WalFileHeader::new(snapshot_seq)
        .write_to(&mut file)
        .unwrap();
    file.sync_all().unwrap();
    file.seek(SeekFrom::Start(WAL_FILE_HEADER_LEN as u64))
        .unwrap();
    file
}

fn handle_snapshot_seq(file: &mut File) -> u64 {
    file.seek(SeekFrom::Start(0)).unwrap();
    let sequence = WalFileHeader::read_from(&mut *file).unwrap().snapshot_seq;
    file.seek(SeekFrom::Start(WAL_FILE_HEADER_LEN as u64))
        .unwrap();
    sequence
}

#[test]
fn active_wal_reset_atomically_replaces_handle_and_keeps_lock() {
    let dir = temp_dir("success");
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut file = open_locked_wal(&path, 3);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    reset_active_wal_file(
        &ManifestEpochGuard::acquire(
            &crate::StoreWriter::acquire(&StoreDirectory::open(&dir).unwrap()).unwrap(),
        )
        .unwrap(),
        &mut file,
        Path::new(DEFAULT_WAL_FILE_NAME),
        9,
    )
    .unwrap();

    assert_eq!(handle_snapshot_seq(&mut file), 9);
    assert_eq!(WalReader::open(&path).unwrap().snapshot_seq(), 9);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(matches!(
        WalWriter::open(&path, WalConfig::default()),
        Err(PersistError::WriterLockHeld)
    ));
    drop(file);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn active_wal_reset_failure_before_rename_preserves_old_wal() {
    let dir = temp_dir("before-rename");
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut file = open_locked_wal(&path, 3);
    RESET_FAULT_POINT.with(|point| point.set(1));

    let error = reset_active_wal_file(
        &ManifestEpochGuard::acquire(
            &crate::StoreWriter::acquire(&StoreDirectory::open(&dir).unwrap()).unwrap(),
        )
        .unwrap(),
        &mut file,
        Path::new(DEFAULT_WAL_FILE_NAME),
        9,
    )
    .unwrap_err();

    assert!(matches!(error, PersistError::Io(_)));
    assert_eq!(handle_snapshot_seq(&mut file), 3);
    assert_eq!(WalReader::open(&path).unwrap().snapshot_seq(), 3);
    assert!(fs::read_dir(&dir).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".reset.tmp.")
    }));
    drop(file);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn active_wal_reset_failure_after_rename_leaves_new_header_valid() {
    let dir = temp_dir("after-rename");
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut file = open_locked_wal(&path, 3);
    RESET_FAULT_POINT.with(|point| point.set(2));

    let error = reset_active_wal_file(
        &ManifestEpochGuard::acquire(
            &crate::StoreWriter::acquire(&StoreDirectory::open(&dir).unwrap()).unwrap(),
        )
        .unwrap(),
        &mut file,
        Path::new(DEFAULT_WAL_FILE_NAME),
        9,
    )
    .unwrap_err();

    assert!(matches!(error, PersistError::Io(_)));
    // The caller still owns the old unlinked handle, but the recovery path
    // names the fully synced replacement header. Rotation treats this as
    // incomplete and requires reopen rather than continuing on the handle.
    assert_eq!(handle_snapshot_seq(&mut file), 3);
    assert_eq!(WalReader::open(&path).unwrap().snapshot_seq(), 9);
    drop(file);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn incomplete_rotation_poisons_all_mutating_writer_apis() {
    let dir = temp_dir("poisoned-writer");
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &path,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();
    let changes = [Change::NodeDeleted { id: NodeId::new(1) }];
    writer
        .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes)
        .unwrap();
    let builder = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.clone(),
        sequence: 1,
        compression: SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    RESET_FAULT_POINT.with(|point| point.set(2));

    assert!(matches!(
        writer.rotate_with_manifest(builder),
        Err(PersistError::WalRotationIncomplete { .. })
    ));
    assert!(matches!(
        writer.append(HlcTimestamp::new(2, 0), Origin::Local, None, &changes),
        Err(PersistError::WalWriterPoisoned)
    ));
    assert!(matches!(
        writer.flush(),
        Err(PersistError::WalWriterPoisoned)
    ));
    let retry = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.clone(),
        sequence: 1,
        compression: SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    assert!(matches!(
        writer.rotate_with_manifest(retry),
        Err(PersistError::WalWriterPoisoned)
    ));
    assert!(matches!(
        writer.prune(&RetentionPolicy::default()),
        Err(PersistError::WalWriterPoisoned)
    ));

    drop(writer);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pruned_archive_reset_failure_reports_no_archive_and_poisons_writer() {
    let dir = temp_dir("pruned-archive-reset-failure");
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    let archive = dir.join("wal.1.archive");
    let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    let changes = [Change::NodeDeleted { id: NodeId::new(1) }];
    writer
        .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes)
        .unwrap();
    writer.flush().unwrap();
    SnapshotBuilder::new(SnapshotConfig {
        dir: dir.clone(),
        sequence: 1,
        compression: SectionCompression::None,
        fsync: true,
    })
    .unwrap()
    .finalize_with_authority(writer.authority())
    .unwrap();
    fs::copy(&path, &archive).unwrap();
    Manifest {
        live_snapshot_seq: 1,
        active_wal_header_seq: 1,
        compaction_epoch: 0,
        active_wal: DEFAULT_WAL_FILE_NAME.to_owned(),
        archived_wal_seqs: vec![1],
    }
    .write_atomic_with_authority(writer.authority())
    .unwrap();
    writer
        .prune(&RetentionPolicy {
            keep_n_snapshots: 1,
            keep_n_wal_archives: 0,
            max_total_size_bytes: None,
            time_based: None,
        })
        .unwrap();
    RESET_FAULT_POINT.with(|point| point.set(2));

    let error = writer
        .rotate_with_manifest(
            SnapshotBuilder::new(SnapshotConfig {
                dir: dir.clone(),
                sequence: 1,
                compression: SectionCompression::None,
                fsync: true,
            })
            .unwrap(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        PersistError::WalRotationIncomplete {
            archived_path: None,
            ..
        }
    ));
    assert!(matches!(
        writer.flush(),
        Err(PersistError::WalWriterPoisoned)
    ));
    drop(writer);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn committed_archive_validation_rejects_header_only_file() {
    let dir = temp_dir("header-only-archive");
    let path = dir.join("wal.7.archive");
    let file = open_locked_wal(&path, 7);
    drop(file);

    assert!(matches!(
        verify_committed_archive(&StoreDirectory::open(&dir).unwrap(), Path::new("wal.7.archive"), 7),
        Err(PersistError::CommittedArchiveInvalid { path: observed }) if observed == path.canonicalize().unwrap()
    ));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn committed_archive_validation_rejects_sequence_below_header_floor() {
    let dir = temp_dir("archive-sequence-floor");
    let path = dir.join("wal.7.archive");
    let mut writer = WalWriter::open(
        &path,
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq: 5,
        },
    )
    .unwrap();
    let changes = [Change::NodeDeleted { id: NodeId::new(1) }];
    assert_eq!(
        writer
            .append(HlcTimestamp::new(6, 0), Origin::Local, None, &changes)
            .unwrap(),
        6
    );
    assert_eq!(
        writer
            .append(HlcTimestamp::new(7, 0), Origin::Local, None, &changes)
            .unwrap(),
        7
    );
    drop(writer);
    // Rewrite the first entry's sequence, then repair the prefix checksum over
    // [4..40) so the frame stays internally consistent. Without the repair the
    // frame fails its checksum, which verify_committed_archive blankets into the
    // same CommittedArchiveInvalid this test asserts — so it would go green
    // without ever reaching the sequence-continuity check it exists to pin.
    let mut bytes = fs::read(&path).unwrap();
    let frame = WAL_FILE_HEADER_LEN;
    bytes[frame + 12..frame + 20].copy_from_slice(&1_u64.to_le_bytes());
    let repaired = crate::payload::checksum_lo(&bytes[frame + 4..frame + 40]);
    bytes[frame..frame + 4].copy_from_slice(&repaired.to_le_bytes());
    fs::write(&path, bytes).unwrap();

    assert!(matches!(
        verify_committed_archive(&StoreDirectory::open(&dir).unwrap(), Path::new("wal.7.archive"), 7),
        Err(PersistError::CommittedArchiveInvalid { path: observed }) if observed == path.canonicalize().unwrap()
    ));
    fs::remove_dir_all(dir).unwrap();
}
