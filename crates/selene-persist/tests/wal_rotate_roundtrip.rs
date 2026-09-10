#![allow(missing_docs)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{Change, HlcTimestamp, NodeId, Origin};
use selene_persist::{
    DEFAULT_WAL_FILE_NAME, Manifest, PersistError, RetentionPolicy, SectionCompression,
    SnapshotBuilder, SnapshotConfig, SnapshotReader, SyncPolicy, WalConfig, WalReader, WalWriter,
    snapshot_path,
};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-wal-rotate-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("temp dir is created");
    dir.canonicalize().expect("temp dir canonicalizes")
}

fn changes(id: u64) -> [Change; 1] {
    [Change::NodeDeleted {
        id: NodeId::new(id),
    }]
}

fn append(writer: &mut WalWriter, id: u64) -> u64 {
    writer
        .append(HlcTimestamp::new(id, 0), Origin::Local, None, &changes(id))
        .expect("wal append succeeds")
}

fn sequences(path: &Path) -> Vec<u64> {
    WalReader::open(path)
        .expect("wal opens for read")
        .iterate(|_| true)
        .expect("wal iterates")
        .map(|entry| entry.expect("entry reads").header.sequence)
        .collect()
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
        .expect("section adds");
    builder
}

fn assert_writer_poisoned(writer: &mut WalWriter, dir: &Path) {
    assert!(matches!(
        writer.append(HlcTimestamp::new(99, 0), Origin::Local, None, &changes(99),),
        Err(PersistError::WalWriterPoisoned)
    ));
    assert!(matches!(
        writer.flush(),
        Err(PersistError::WalWriterPoisoned)
    ));
    let sequence = writer.last_sequence();
    assert!(matches!(
        writer.rotate_with_manifest(meta_builder(dir, sequence, b"poisoned")),
        Err(PersistError::WalWriterPoisoned)
    ));
    assert!(matches!(
        writer.prune(&RetentionPolicy::default()),
        Err(PersistError::WalWriterPoisoned)
    ));
}

#[test]
fn wal_rotate_round_trips_new_and_archived_logs() {
    let dir = temp_dir("roundtrip");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &wal_path,
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq: 0,
        },
    )
    .expect("wal opens");

    assert_eq!(append(&mut writer, 1), 1);

    let rotation = writer
        .rotate_with_manifest(meta_builder(&dir, writer.last_sequence(), b"snap1"))
        .expect("wal rotates after snapshot");

    assert_eq!(rotation.snapshot_sequence(), 1);
    assert_eq!(
        rotation.archived_path(),
        Some(dir.join("wal.1.archive").as_path())
    );
    assert_eq!(rotation.active_path(), wal_path);
    assert!(
        rotation
            .archived_path()
            .expect("rotation has archive")
            .exists()
    );
    assert!(snapshot_path(&dir, 1).exists());
    assert_eq!(writer.snapshot_seq(), 1);
    assert_eq!(writer.last_sequence(), 1);

    let archived_before_repeat = fs::read(rotation.archived_path().expect("archive path"))
        .expect("archive reads before repeat");
    let repeated = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snap1"))
        .expect("completed same-sequence rotation is already current");
    assert_eq!(repeated.snapshot_sequence(), 1);
    assert!(repeated.archived_path().is_none());
    assert_eq!(repeated.active_path(), wal_path);
    assert_eq!(
        fs::read(dir.join("wal.1.archive")).expect("archive reads after repeat"),
        archived_before_repeat
    );

    // The MANIFEST is the commit point and names the live epoch + archive set.
    let manifest = Manifest::read(&dir)
        .expect("manifest reads")
        .expect("present");
    assert_eq!(manifest.live_snapshot_seq, 1);
    assert_eq!(manifest.active_wal_header_seq, 1);
    assert_eq!(manifest.active_wal, DEFAULT_WAL_FILE_NAME);
    assert_eq!(manifest.archived_wal_seqs, vec![1]);

    assert_eq!(append(&mut writer, 2), 2);
    writer.flush().expect("rotated wal flushes");

    let archived_path = rotation.archived_path().expect("rotation has archive");
    let archived_reader = WalReader::open(archived_path).expect("archive opens");
    assert_eq!(archived_reader.snapshot_seq(), 0);
    assert_eq!(sequences(archived_path), vec![1]);

    let new_reader = WalReader::open(&wal_path).expect("new wal opens");
    assert_eq!(new_reader.snapshot_seq(), 1);
    assert_eq!(sequences(&wal_path), vec![2]);

    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn wal_rotate_rejects_builder_sequence_that_does_not_cover_current_wal() {
    let dir = temp_dir("stale-snapshot");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    assert_eq!(append(&mut writer, 2), 2);
    writer.flush().expect("wal flushes");

    // Builder targets sequence 1 while the writer high-water mark is 2.
    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"stale"))
        .expect_err("stale snapshot rejects");
    assert!(matches!(
        error,
        PersistError::WalRotationSequenceMismatch {
            snapshot_seq: 1,
            last_sequence: 2
        }
    ));
    // Nothing was published or committed: no archive, no snapshot, no MANIFEST.
    assert!(!dir.join("wal.2.archive").exists());
    assert!(!snapshot_path(&dir, 1).exists());
    assert!(Manifest::read(&dir).expect("manifest reads").is_none());
    assert_eq!(sequences(&wal_path), vec![1, 2]);
    assert_eq!(append(&mut writer, 3), 3);

    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rotation_rejects_different_existing_snapshot_bytes() {
    let dir = temp_dir("snapshot-identity-mismatch");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    meta_builder(&dir, 1, b"foreign-snapshot")
        .finalize_with_authority(writer.authority())
        .expect("foreign snapshot publishes");
    let snapshot = snapshot_path(&dir, 1);
    let foreign_bytes = fs::read(&snapshot).expect("foreign snapshot reads");

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"intended-snapshot"))
        .expect_err("different snapshot bytes reject");

    assert!(matches!(
        error,
        PersistError::ArtifactIdentityMismatch { path } if path == snapshot
    ));
    assert_eq!(fs::read(&snapshot).unwrap(), foreign_bytes);
    assert!(!dir.join("wal.1.archive").exists());
    assert_eq!(Manifest::read(&dir).unwrap().unwrap().live_snapshot_seq, 0);

    fs::remove_file(&snapshot).expect("foreign snapshot removes");
    let retry = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"intended-snapshot"))
        .expect("writer remains usable after pre-commit mismatch");
    assert_eq!(retry.snapshot_sequence(), 1);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rotation_rejects_existing_snapshot_with_unhashed_trailing_bytes() {
    let dir = temp_dir("snapshot-trailing-bytes");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    meta_builder(&dir, 1, b"intended")
        .finalize_with_authority(writer.authority())
        .expect("snapshot publishes");
    let snapshot = snapshot_path(&dir, 1);
    OpenOptions::new()
        .append(true)
        .open(&snapshot)
        .unwrap()
        .write_all(b"trailing-junk")
        .unwrap();
    let mut reader = SnapshotReader::open(&snapshot).expect("snapshot envelope opens");
    reader
        .verify_body_hash()
        .expect("envelope hash intentionally ignores trailing bytes");

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"intended"))
        .expect_err("exact file identity rejects trailing bytes");

    assert!(matches!(
        error,
        PersistError::ArtifactIdentityMismatch { path } if path == snapshot
    ));
    assert!(!dir.join("wal.1.archive").exists());
    assert_eq!(Manifest::read(&dir).unwrap().unwrap().live_snapshot_seq, 0);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rotation_rejects_different_existing_archive_bytes() {
    let dir = temp_dir("archive-identity-mismatch");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let archive_path = dir.join("wal.1.archive");
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    writer.flush().expect("wal flushes");
    let mut foreign_bytes = fs::read(&wal_path).expect("wal reads");
    let last = foreign_bytes.last_mut().expect("wal contains an entry");
    *last ^= 0x5a;
    fs::write(&archive_path, &foreign_bytes).expect("foreign archive writes");

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect_err("different archive bytes reject");

    assert!(matches!(
        error,
        PersistError::ArtifactIdentityMismatch { path } if path == archive_path
    ));
    assert_eq!(fs::read(&archive_path).unwrap(), foreign_bytes);
    assert_eq!(Manifest::read(&dir).unwrap().unwrap().live_snapshot_seq, 0);
    assert_eq!(sequences(&wal_path), vec![1]);
    assert!(snapshot_path(&dir, 1).exists());

    fs::remove_file(&archive_path).expect("foreign archive removes");
    let retry = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect("cursor and writer remain usable after mismatch");
    assert_eq!(retry.snapshot_sequence(), 1);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn rotation_rejects_archive_symlink_collision() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("archive-symlink");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let archive_path = dir.join("wal.1.archive");
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    symlink(&wal_path, &archive_path).expect("archive symlink creates");

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect_err("symlink collision rejects");

    assert!(matches!(
        error,
        PersistError::ArtifactIdentityMismatch { path } if path == archive_path
    ));
    assert_eq!(Manifest::read(&dir).unwrap().unwrap().live_snapshot_seq, 0);
    assert_eq!(sequences(&wal_path), vec![1]);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn already_current_rotation_does_not_recreate_pruned_archive() {
    let dir = temp_dir("already-current-pruned");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let archive_path = dir.join("wal.1.archive");
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect("initial rotation succeeds");
    let mut manifest = Manifest::read(&dir).unwrap().unwrap();
    manifest.archived_wal_seqs.clear();
    manifest
        .write_atomic_with_authority(writer.authority())
        .expect("pruned manifest commits");
    fs::remove_file(&archive_path).expect("pruned archive removes");

    let outcome = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect("already-current snapshot is a no-op after retention");

    assert_eq!(outcome.snapshot_sequence(), 1);
    assert!(outcome.archived_path().is_none());
    assert!(!archive_path.exists());
    assert_eq!(Manifest::read(&dir).unwrap().unwrap(), manifest);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn already_current_rotation_does_not_recreate_missing_snapshot() {
    let dir = temp_dir("already-current-missing-snapshot");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let snapshot = snapshot_path(&dir, 1);
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect("initial rotation succeeds");
    let committed = Manifest::read(&dir).unwrap().unwrap();
    fs::remove_file(&snapshot).expect("live snapshot removes");

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect_err("committed missing snapshot rejects");

    assert!(matches!(
        error,
        PersistError::CommittedSnapshotUnavailable { path } if path == snapshot
    ));
    assert!(
        !snapshot.exists(),
        "retry must not reconstruct authoritative state"
    );
    assert_eq!(Manifest::read(&dir).unwrap().unwrap(), committed);
    assert_writer_poisoned(&mut writer, &dir);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn already_current_rotation_rejects_different_committed_snapshot() {
    let dir = temp_dir("already-current-different-snapshot");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let snapshot = snapshot_path(&dir, 1);
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"original"))
        .expect("initial rotation succeeds");
    fs::remove_file(&snapshot).expect("original snapshot removes");
    meta_builder(&dir, 1, b"foreign")
        .finalize_with_authority(writer.authority())
        .expect("foreign snapshot publishes");
    let foreign_bytes = fs::read(&snapshot).unwrap();

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"original"))
        .expect_err("different committed snapshot rejects");

    assert!(matches!(
        error,
        PersistError::CommittedSnapshotIdentityMismatch { path } if path == snapshot
    ));
    assert_eq!(fs::read(&snapshot).unwrap(), foreign_bytes);
    assert_writer_poisoned(&mut writer, &dir);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn already_current_rotation_rejects_invalid_committed_archive() {
    let dir = temp_dir("already-current-invalid-archive");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let archive_path = dir.join("wal.1.archive");
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect("initial rotation succeeds");
    fs::remove_file(&archive_path).expect("original archive removes");
    fs::copy(&wal_path, &archive_path).expect("header-only archive replacement publishes");

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect_err("invalid retained archive rejects");

    assert!(matches!(
        error,
        PersistError::CommittedArchiveInvalid { path } if path == archive_path
    ));
    assert_writer_poisoned(&mut writer, &dir);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rotation_retry_finishes_reset_after_manifest_commit() {
    let dir = temp_dir("manifest-committed-before-reset");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let archive_path = dir.join("wal.1.archive");
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    writer.flush().expect("wal flushes");
    meta_builder(&dir, 1, b"snapshot")
        .finalize_with_authority(writer.authority())
        .expect("phase 1 snapshot publishes");
    fs::copy(&wal_path, &archive_path).expect("phase 2 archive publishes");
    let committed = Manifest {
        live_snapshot_seq: 1,
        active_wal_header_seq: 1,
        compaction_epoch: 0,
        active_wal: DEFAULT_WAL_FILE_NAME.to_owned(),
        archived_wal_seqs: vec![1],
    };
    committed
        .write_atomic_with_authority(writer.authority())
        .expect("phase 3 manifest commits");

    let outcome = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect("retry verifies artifacts and finishes reset");

    assert_eq!(outcome.snapshot_sequence(), 1);
    assert_eq!(outcome.archived_path(), Some(archive_path.as_path()));
    assert_eq!(writer.snapshot_seq(), 1);
    assert_eq!(sequences(&wal_path), Vec::<u64>::new());
    assert_eq!(Manifest::read(&dir).unwrap().unwrap(), committed);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn wal_rotate_with_manifest_is_idempotent_on_pre_published_archive() {
    // A crashed rotation can leave `snapshot.N.snap` and `wal.N.archive`
    // published before the MANIFEST commit. Re-rotating to the same epoch must
    // converge (Phase 1/2 treat AlreadyExists as already-done) rather than
    // hard-fail the way the deleted snapshot-less `rotate` would on a collision.
    let dir = temp_dir("idempotent-rerotate");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);
    writer.flush().expect("wal flushes");

    // Pre-publish the archive a crashed Phase 2 would have left behind.
    let archive_path = dir.join("wal.1.archive");
    fs::copy(&wal_path, &archive_path).expect("pre-publish archive");
    assert!(archive_path.exists());

    let rotation = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snap1"))
        .expect("rotate converges over pre-published archive");
    assert_eq!(rotation.snapshot_sequence(), 1);
    assert_eq!(rotation.archived_path(), Some(archive_path.as_path()));

    let manifest = Manifest::read(&dir)
        .expect("manifest reads")
        .expect("present");
    assert_eq!(manifest.live_snapshot_seq, 1);
    assert_eq!(manifest.archived_wal_seqs, vec![1]);
    assert_eq!(writer.last_sequence(), 1);
    assert_eq!(sequences(&wal_path), Vec::<u64>::new());

    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rotation_rejects_nondefault_wal_before_flush_or_artifacts() {
    let dir = temp_dir("nondefault-name");
    let wal_path = dir.join("custom.log");
    let mut writer = WalWriter::open(
        &wal_path,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();
    assert_eq!(append(&mut writer, 1), 1);

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .unwrap_err();

    assert!(matches!(
        error,
        PersistError::UnexpectedActiveWal { observed, expected }
            if observed == "custom.log" && expected == DEFAULT_WAL_FILE_NAME
    ));
    assert_eq!(writer.entries_since_fsync(), 1);
    assert!(Manifest::read(&dir).unwrap().is_none());
    assert!(!snapshot_path(&dir, 1).exists());
    assert!(!dir.join("wal.1.archive").exists());
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rotation_rejects_nondefault_manifest_wal_before_flush_or_artifacts() {
    let dir = temp_dir("nondefault-manifest-wal");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let invalid_manifest = Manifest {
        live_snapshot_seq: 0,
        active_wal_header_seq: 0,
        compaction_epoch: 0,
        active_wal: "custom.log".to_owned(),
        archived_wal_seqs: Vec::new(),
    };
    invalid_manifest
        .write_atomic(&dir)
        .expect("invalid manifest fixture writes");
    let mut writer = WalWriter::open(
        &wal_path,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect_err("non-default manifest active WAL rejects");

    assert!(matches!(
        error,
        PersistError::UnexpectedActiveWal { observed, expected }
            if observed == "custom.log" && expected == DEFAULT_WAL_FILE_NAME
    ));
    assert_eq!(writer.entries_since_fsync(), 1);
    assert_eq!(
        Manifest::read(&dir).expect("manifest reads"),
        Some(invalid_manifest)
    );
    assert!(!snapshot_path(&dir, 1).exists());
    assert!(!dir.join("wal.1.archive").exists());
    assert_writer_poisoned(&mut writer, &dir);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rotation_rejects_manifest_ahead_before_flush_or_artifacts() {
    let dir = temp_dir("manifest-ahead");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let ahead = Manifest {
        live_snapshot_seq: 2,
        active_wal_header_seq: 2,
        compaction_epoch: 0,
        active_wal: DEFAULT_WAL_FILE_NAME.to_owned(),
        archived_wal_seqs: vec![2],
    };
    ahead.write_atomic(&dir).expect("manifest fixture writes");
    let mut writer = WalWriter::open(
        &wal_path,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .expect("wal opens");
    assert_eq!(append(&mut writer, 1), 1);

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 1, b"snapshot"))
        .expect_err("newer manifest rejects stale rotation");

    assert!(matches!(
        error,
        PersistError::WalRotationManifestAhead {
            manifest_sequence: 2,
            snapshot_sequence: 1,
        }
    ));
    assert_eq!(writer.entries_since_fsync(), 1);
    assert_eq!(Manifest::read(&dir).unwrap(), Some(ahead));
    assert!(!snapshot_path(&dir, 1).exists());
    assert!(!dir.join("wal.1.archive").exists());
    assert_writer_poisoned(&mut writer, &dir);
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rotation_rejects_zero_sequence_before_artifacts() {
    let dir = temp_dir("zero-sequence");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(&wal_path, WalConfig::default()).unwrap();

    let error = writer
        .rotate_with_manifest(meta_builder(&dir, 0, b"zero"))
        .unwrap_err();

    assert!(matches!(error, PersistError::WalRotationZeroSequence));
    assert!(Manifest::read(&dir).unwrap().is_none());
    assert!(!snapshot_path(&dir, 0).exists());
    assert!(!dir.join("wal.0.archive").exists());
    drop(writer);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rotation_rejects_snapshot_directory_mismatch_before_flush() {
    let dir = temp_dir("directory-mismatch");
    let other = temp_dir("directory-mismatch-other");
    let wal_path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &wal_path,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();
    assert_eq!(append(&mut writer, 1), 1);

    let error = writer
        .rotate_with_manifest(meta_builder(&other, 1, b"wrong-dir"))
        .unwrap_err();

    assert!(matches!(
        error,
        PersistError::WalRotationDirectoryMismatch {
            snapshot_dir,
            wal_dir,
        } if snapshot_dir == other && wal_dir == dir
    ));
    assert_eq!(writer.entries_since_fsync(), 1);
    assert!(Manifest::read(&dir).unwrap().is_none());
    assert!(!snapshot_path(&other, 1).exists());
    drop(writer);
    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(other);
}
