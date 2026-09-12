#![cfg(any(target_os = "linux", target_os = "macos"))]
use crate::control::{CompatibilityIdentity, EmptyStoreControl};
use crate::logical_stream::{LogicalWal, RecoveryReader};
use crate::*;
use selene_testing::PersistenceTestPath;
use std::{fs::File, path::PathBuf};

fn fixture() -> (PersistenceTestPath, PathBuf, StoreDirectory) {
    let fixture = PersistenceTestPath::new();
    let root = fixture.parent().unwrap().join("store");
    std::fs::create_dir(&root).unwrap();
    let dir = StoreDirectory::from_file(File::open(&root).unwrap(), &root).unwrap();
    (fixture, root, dir)
}
fn identity() -> CompatibilityIdentity {
    CompatibilityIdentity::new("fixture", 1, [1; 32], [16, 0, 0], "binary", 1).unwrap()
}

#[test]
fn control_creation_requires_independent_writer_ownership() {
    let (_fixture, _root, dir) = fixture();
    let _owner = StoreWriter::acquire(&dir).unwrap();
    assert!(matches!(
        EmptyStoreControl::create_empty(&dir, identity()),
        Err(PersistError::WriterLockHeld)
    ));
    assert!(!dir.contains("CURRENT").unwrap());
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
fn empty_control_also_survives_renamed_real_parent() {
    let (_fixture, root, dir) = fixture();
    let mut store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    let id = store.manifest().store_id();
    std::fs::rename(&root, root.with_file_name("retained")).unwrap();
    std::fs::create_dir(&root).unwrap();
    store.publish_empty().unwrap();
    drop(store);
    let store = EmptyStoreControl::open(&dir, &identity()).unwrap();
    assert_eq!(store.manifest().store_id(), id);
    assert_eq!(store.manifest().generation().get(), 2);
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
}
#[test]
fn retained_directory_keeps_checkpoint_reader_and_prune_after_parent_replacement() {
    let (_fixture, root, dir) = fixture();
    let mut wal =
        LogicalWal::create(EmptyStoreControl::create_empty(&dir, identity()).unwrap()).unwrap();
    let first = wal.checkpoint(b"first image", 0).unwrap();
    let mut reader = RecoveryReader::open(&dir, &identity(), 4096).unwrap();
    std::fs::rename(&root, root.with_file_name("retained")).unwrap();
    std::fs::create_dir(&root).unwrap();
    for _ in 0..3 {
        wal.checkpoint(b"later image", 0).unwrap();
    }
    wal.prune().unwrap();
    assert!(dir.contains(&first.name).unwrap());
    assert_eq!(reader.snapshot_body().unwrap(), b"first image");
    assert!(reader.next_body().unwrap().is_none());
    drop(reader);
    wal.prune().unwrap();
    assert!(!dir.contains(&first.name).unwrap());
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
}
