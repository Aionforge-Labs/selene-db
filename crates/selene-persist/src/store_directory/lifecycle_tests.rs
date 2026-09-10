#![cfg(any(target_os = "linux", target_os = "macos"))]

use crate::*;
use selene_core::{Change, HlcTimestamp, NodeId, Origin};
use selene_testing::PersistenceTestPath;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::time::Duration;

fn fixture() -> (PersistenceTestPath, PathBuf, StoreDirectory) {
    let fixture = PersistenceTestPath::new();
    let root = fixture.parent().unwrap().join("store");
    std::fs::create_dir(&root).unwrap();
    let dir = StoreDirectory::from_file(File::open(&root).unwrap(), &root).unwrap();
    (fixture, root, dir)
}

fn append(writer: &mut WalWriter, seq: u64) {
    assert_eq!(
        writer
            .append(
                HlcTimestamp::new(seq, 0),
                Origin::Local,
                None,
                &[Change::NodeDeleted {
                    id: NodeId::new(seq)
                }]
            )
            .unwrap(),
        seq
    );
}

fn builder(dir: &StoreDirectory, sequence: u64) -> SnapshotBuilder {
    SnapshotBuilder::new_in(
        dir,
        SnapshotConfig {
            sequence,
            compression: SectionCompression::None,
            ..SnapshotConfig::default()
        },
    )
}

fn empty_manifest() -> Manifest {
    Manifest {
        live_snapshot_seq: 0,
        active_wal_header_seq: 0,
        compaction_epoch: 0,
        active_wal: DEFAULT_WAL_FILE_NAME.into(),
        archived_wal_seqs: Vec::new(),
    }
}

#[test]
fn standalone_manifest_requires_independent_writer_ownership() {
    let (_fixture, _root, dir) = fixture();
    let _owner = StoreWriter::acquire(&dir).unwrap();
    assert!(matches!(
        empty_manifest().write_atomic_in(&dir),
        Err(PersistError::WriterLockHeld)
    ));
    assert!(!dir.contains(MANIFEST_FILE_NAME).unwrap());
}

#[test]
fn standalone_snapshot_requires_independent_writer_ownership() {
    let (_fixture, _root, dir) = fixture();
    let _owner = StoreWriter::acquire(&dir).unwrap();
    assert!(matches!(
        builder(&dir, 1).finalize(),
        Err(PersistError::WriterLockHeld)
    ));
    assert!(!dir.contains("snapshot.1.snap").unwrap());
}

#[test]
fn standalone_prune_requires_independent_writer_ownership() {
    let (_fixture, _root, dir) = fixture();
    empty_manifest().write_atomic_in(&dir).unwrap();
    let _owner = StoreWriter::acquire(&dir).unwrap();
    assert!(matches!(
        retention::prune_in(&dir, &RetentionPolicy::default()),
        Err(PersistError::WriterLockHeld)
    ));
}

#[test]
fn owned_publication_and_prune_reuse_the_lease_while_readers_remain_independent() {
    let (_fixture, _root, dir) = fixture();
    let owner = StoreWriter::acquire(&dir).unwrap();
    empty_manifest()
        .write_atomic_with_authority(&owner)
        .unwrap();
    let reader = PersistenceReadGuard::acquire_in(&dir).unwrap();
    assert_eq!(reader.read_manifest().unwrap(), Some(empty_manifest()));
    assert_eq!(
        recover_guarded(&reader, &ProviderRegistry::new())
            .unwrap()
            .last_wal_seq,
        0
    );
    drop(reader);
    builder(&dir, 1).finalize_with_authority(&owner).unwrap();
    let outcome = retention::prune_with_authority(
        &owner,
        &RetentionPolicy {
            keep_n_snapshots: 0,
            ..RetentionPolicy::default()
        },
    )
    .unwrap();
    assert_eq!(outcome.deleted_snapshots, vec![1]);
    assert!(matches!(
        StoreWriter::acquire(&dir),
        Err(PersistError::WriterLockHeld)
    ));
    drop(owner);
    // Thin standalone APIs acquire their own lease once the old owner releases.
    builder(&dir, 2).finalize().unwrap();
    empty_manifest().write_atomic_in(&dir).unwrap();
    retention::prune_in(&dir, &RetentionPolicy::default()).unwrap();
}

#[test]
fn snapshot_foreign_writer_authority_fails_before_epoch_or_artifact_creation() {
    let (_fixture_a, _root_a, target) = fixture();
    let (_fixture_b, _root_b, foreign) = fixture();
    let owner = StoreWriter::acquire(&foreign).unwrap();
    assert!(matches!(
        builder(&target, 1).finalize_with_authority(&owner),
        Err(PersistError::WalRotationDirectoryMismatch { .. })
    ));
    assert!(target.entries().unwrap().is_empty());
    assert!(!foreign.contains(MANIFEST_LOCK_FILE_NAME).unwrap());
    assert!(!foreign.contains("snapshot.1.snap").unwrap());
}

#[test]
fn exclusive_epoch_retains_writer_proof_until_the_guard_is_dropped() {
    let (_fixture, _root, dir) = fixture();
    let owner = StoreWriter::acquire(&dir).unwrap();
    let epoch = crate::manifest_lock::ManifestEpochGuard::acquire(&owner).unwrap();
    drop(owner);
    assert!(matches!(
        StoreWriter::acquire(&dir),
        Err(PersistError::WriterLockHeld)
    ));
    drop(epoch);
    drop(StoreWriter::acquire(&dir).unwrap());
}

#[test]
fn reader_blocks_owned_prune_until_its_artifact_lease_is_released() {
    let (_fixture, _root, dir) = fixture();
    let owner = StoreWriter::acquire(&dir).unwrap();
    empty_manifest()
        .write_atomic_with_authority(&owner)
        .unwrap();
    builder(&dir, 1).finalize_with_authority(&owner).unwrap();
    let reader = PersistenceReadGuard::acquire_in(&dir).unwrap();
    let worker_owner = owner.clone();
    let (contended_tx, contended_rx) = sync_channel(1);
    let worker = std::thread::spawn(move || {
        crate::manifest_lock::set_contention_hook(move || contended_tx.send(()).unwrap());
        retention::prune_with_authority(
            &worker_owner,
            &RetentionPolicy {
                keep_n_snapshots: 0,
                ..RetentionPolicy::default()
            },
        )
        .unwrap()
    });
    contended_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(reader.directory().contains("snapshot.1.snap").unwrap());
    drop(reader);
    assert_eq!(worker.join().unwrap().deleted_snapshots, vec![1]);
    assert!(!dir.contains("snapshot.1.snap").unwrap());
}

#[test]
fn real_parent_replacement_cannot_redirect_any_legacy_artifact_family() {
    let (_fixture, root, dir) = fixture();
    let retained = root.with_file_name("retained");
    let mut writer = WalWriter::open_in(&dir, Path::new("wal.log"), WalConfig::default()).unwrap();
    append(&mut writer, 1);
    let reader = WalReader::open_in(&dir, Path::new("wal.log")).unwrap();
    let snapshot = builder(&dir, 1);
    std::fs::rename(&root, &retained).unwrap();
    std::fs::create_dir(&root).unwrap();
    assert_eq!(reader.iterate(|_| true).unwrap().count(), 1);
    let mut audit =
        AuditLog::open_with_authority(writer.authority(), Path::new("audit.log")).unwrap();
    audit
        .append(&AuditRecord {
            recorded_at_unix_nanos: 1,
            kind: 0,
            payload: vec![7],
        })
        .unwrap();
    writer.rotate_with_manifest(snapshot).unwrap();
    append(&mut writer, 2);
    writer.rotate_with_manifest(builder(&dir, 2)).unwrap();
    let mut snapshot_reader = SnapshotReader::open_in(&dir, Path::new("snapshot.2.snap")).unwrap();
    snapshot_reader.verify_body_hash().unwrap();
    assert_eq!(find_latest_snapshot_in(&dir).unwrap().unwrap().0, 2);
    writer
        .prune(&RetentionPolicy {
            keep_n_snapshots: 1,
            keep_n_wal_archives: 0,
            ..RetentionPolicy::default()
        })
        .unwrap();
    audit
        .prune(
            &AuditRetentionPolicy {
                keep_n_events: Some(0),
                max_age: None,
            },
            2,
        )
        .unwrap();
    assert!(
        AuditLog::read_all_in(&dir, Path::new("audit.log"))
            .unwrap()
            .is_empty()
    );
    let guard = PersistenceReadGuard::acquire_in(&dir).unwrap();
    assert_eq!(guard.read_manifest().unwrap().unwrap().live_snapshot_seq, 2);
    let recovered = recover_guarded(&guard, &ProviderRegistry::new()).unwrap();
    assert_eq!(recovered.applied_snapshot_seq, 2);
    assert_eq!(recovered.last_wal_seq, 2);
    assert!(!dir.contains("snapshot.1.snap").unwrap());
    assert!(!dir.contains("wal.2.archive").unwrap());
    assert!(
        dir.entries()
            .unwrap()
            .iter()
            .all(|name| !name.to_string_lossy().contains(".tmp"))
    );
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    assert!(retained.join("LOCK").is_file());
    assert!(retained.join("MANIFEST.lock").is_file());
}

#[test]
fn anchored_epoch_reader_blocks_rotation_across_parent_replacement() {
    let (_fixture, root, dir) = fixture();
    let mut writer = WalWriter::open_in(&dir, Path::new("wal.log"), WalConfig::default()).unwrap();
    append(&mut writer, 1);
    writer.rotate_with_manifest(builder(&dir, 1)).unwrap();
    append(&mut writer, 2);
    let reader = PersistenceReadGuard::acquire_in(&dir).unwrap();
    let next = builder(&dir, 2);
    let (contended_tx, contended_rx) = sync_channel(1);
    let worker = std::thread::spawn(move || {
        crate::manifest_lock::set_contention_hook(move || contended_tx.send(()).unwrap());
        writer.rotate_with_manifest(next).unwrap()
    });
    contended_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    std::fs::rename(&root, root.with_file_name("retained")).unwrap();
    std::fs::create_dir(&root).unwrap();
    assert_eq!(
        reader.read_manifest().unwrap().unwrap().live_snapshot_seq,
        1
    );
    assert_eq!(
        recover_guarded(&reader, &ProviderRegistry::new())
            .unwrap()
            .last_wal_seq,
        2
    );
    drop(reader);
    assert_eq!(worker.join().unwrap().snapshot_sequence(), 2);
    assert_eq!(
        Manifest::read_in(&dir).unwrap().unwrap().live_snapshot_seq,
        2
    );
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
}

#[test]
fn empty_control_also_survives_renamed_real_parent() {
    let (_fixture, root, dir) = fixture();
    let compatibility =
        control::CompatibilityIdentity::new("fixture", 1, [1; 32], [16, 0, 0], "binary", 1)
            .unwrap();
    let mut store = control::EmptyStoreControl::create_empty(&dir, compatibility.clone()).unwrap();
    let id = store.manifest().store_id();
    std::fs::rename(&root, root.with_file_name("retained")).unwrap();
    std::fs::create_dir(&root).unwrap();
    store.publish_empty().unwrap();
    drop(store);
    let store = control::EmptyStoreControl::open(&dir, &compatibility).unwrap();
    assert_eq!(store.manifest().store_id(), id);
    assert_eq!(store.manifest().generation().get(), 2);
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
}

#[test]
fn audit_replacement_sync_uncertainty_fences_old_inode_appends() {
    let (_fixture, _root, dir) = fixture();
    let owner = StoreWriter::acquire(&dir).unwrap();
    let mut audit = AuditLog::open_with_authority(&owner, Path::new("audit.log")).unwrap();
    let record = AuditRecord {
        recorded_at_unix_nanos: 1,
        kind: 0,
        payload: vec![1],
    };
    audit.append(&record).unwrap();
    dir.fail_at("audit.after_replace");
    assert!(matches!(
        audit.prune(
            &AuditRetentionPolicy {
                keep_n_events: Some(0),
                max_age: None
            },
            2
        ),
        Err(PersistError::Directory(
            DirectoryError::PublicationUncertain { .. }
        ))
    ));
    assert!(matches!(
        audit.append(&record),
        Err(PersistError::Directory(DirectoryError::RequiresReopen))
    ));
    drop(audit);
    let mut reopened = AuditLog::open_with_authority(&owner, Path::new("audit.log")).unwrap();
    reopened.append(&record).unwrap();
    assert_eq!(
        AuditLog::read_all_in(&dir, Path::new("audit.log")).unwrap(),
        vec![record]
    );
}
