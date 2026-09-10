#![cfg(unix)]

use std::cell::RefCell;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{Change, HlcTimestamp, NodeId, Origin};

use crate::wal_path::set_after_rotation_preflight_hook;
use crate::{
    DEFAULT_WAL_FILE_NAME, MANIFEST_FILE_NAME, MANIFEST_LOCK_FILE_NAME, Manifest, PersistError,
    SectionCompression, SnapshotBuilder, SnapshotConfig, SyncPolicy, WalConfig, WalReader,
    WalWriter, snapshot_path,
};

fn temp_root(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "selene-wal-anchor-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    root.canonicalize().unwrap()
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
    assert_eq!(
        writer
            .append(
                HlcTimestamp::new(sequence, 0),
                Origin::Local,
                None,
                &[Change::NodeDeleted {
                    id: NodeId::new(sequence),
                }],
            )
            .unwrap(),
        sequence
    );
}

fn retarget(alias: &Path, target: &Path) {
    std::fs::remove_file(alias).unwrap();
    symlink(target, alias).unwrap();
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
fn post_validation_alias_retarget_cannot_redirect_rotation() {
    let root = temp_root("post-validation");
    let first_dir = root.join("first");
    let second_dir = root.join("second");
    let alias = root.join("live");
    std::fs::create_dir(&first_dir).unwrap();
    std::fs::create_dir(&second_dir).unwrap();
    symlink(&first_dir, &alias).unwrap();
    let mut writer =
        WalWriter::open(&alias.join(DEFAULT_WAL_FILE_NAME), WalConfig::default()).unwrap();
    append(&mut writer, 1);

    let second_writer = Rc::new(RefCell::new(None));
    let hook_writer = Rc::clone(&second_writer);
    let hook_alias = alias.clone();
    let hook_first = first_dir.clone();
    let hook_second = second_dir.clone();
    set_after_rotation_preflight_hook(move || {
        assert!(!hook_first.join(MANIFEST_FILE_NAME).exists());
        assert!(!hook_first.join(MANIFEST_LOCK_FILE_NAME).exists());
        assert!(!snapshot_path(&hook_first, 1).exists());
        assert!(!hook_first.join("wal.1.archive").exists());
        retarget(&hook_alias, &hook_second);
        let writer = WalWriter::open(
            &hook_alias.join(DEFAULT_WAL_FILE_NAME),
            WalConfig {
                snapshot_seq: 40,
                ..WalConfig::default()
            },
        )
        .unwrap();
        *hook_writer.borrow_mut() = Some(writer);
    });
    let outcome = writer
        .rotate_with_manifest(builder(&alias, 1, b"anchored-snapshot"))
        .unwrap();
    let mut second_writer = second_writer
        .borrow_mut()
        .take()
        .expect("preflight hook opened the second-directory writer");

    let active = first_dir.join(DEFAULT_WAL_FILE_NAME);
    let second_active = second_dir.join(DEFAULT_WAL_FILE_NAME);
    assert_eq!(
        alias.canonicalize().unwrap(),
        second_dir.canonicalize().unwrap()
    );
    assert_eq!(writer.path(), active);
    assert_eq!(outcome.active_path(), active);
    assert_eq!(
        outcome.archived_path(),
        Some(first_dir.join("wal.1.archive").as_path())
    );
    let manifest = Manifest::read(&first_dir).unwrap().unwrap();
    assert_eq!(manifest.live_snapshot_seq, 1);
    assert_eq!(manifest.archived_wal_seqs, vec![1]);
    assert!(first_dir.join(MANIFEST_LOCK_FILE_NAME).is_file());
    assert!(snapshot_path(&first_dir, 1).is_file());
    assert_eq!(WalReader::open(&active).unwrap().snapshot_seq(), 1);
    assert!(!second_dir.join(MANIFEST_FILE_NAME).exists());
    assert!(!second_dir.join(MANIFEST_LOCK_FILE_NAME).exists());
    assert!(!snapshot_path(&second_dir, 1).exists());
    assert!(!second_dir.join("wal.1.archive").exists());
    assert_eq!(WalReader::open(&second_active).unwrap().snapshot_seq(), 40);
    assert!(matches!(
        WalWriter::open(&active, WalConfig::default()),
        Err(PersistError::WriterLockHeld)
    ));
    assert!(matches!(
        WalWriter::open(&second_active, WalConfig::default()),
        Err(PersistError::WriterLockHeld)
    ));
    append(&mut writer, 2);
    writer.flush().unwrap();
    append(&mut second_writer, 41);
    second_writer.flush().unwrap();
    assert_eq!(sequences(&active), vec![2]);
    assert_eq!(sequences(&second_active), vec![41]);

    drop(writer);
    let reopened_first = WalWriter::open(&active, WalConfig::default()).unwrap();
    assert!(matches!(
        WalWriter::open(&second_active, WalConfig::default()),
        Err(PersistError::WriterLockHeld)
    ));
    drop(reopened_first);
    drop(second_writer);
    drop(WalWriter::open(&second_active, WalConfig::default()).unwrap());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn builder_alias_retargeted_before_preflight_keeps_its_opened_directory() {
    let root = temp_root("preflight-mismatch");
    let first_dir = root.join("first");
    let second_dir = root.join("second");
    let alias = root.join("live");
    std::fs::create_dir(&first_dir).unwrap();
    std::fs::create_dir(&second_dir).unwrap();
    symlink(&first_dir, &alias).unwrap();
    let mut writer = WalWriter::open(
        &alias.join(DEFAULT_WAL_FILE_NAME),
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();
    append(&mut writer, 1);
    let stale_builder = builder(&alias, 1, b"stale-alias");
    retarget(&alias, &second_dir);

    writer.rotate_with_manifest(stale_builder).unwrap();
    assert_eq!(writer.entries_since_fsync(), 0);
    assert!(first_dir.join(MANIFEST_FILE_NAME).exists());
    assert!(snapshot_path(&first_dir, 1).exists());
    assert!(first_dir.join("wal.1.archive").exists());
    assert!(!second_dir.join(MANIFEST_FILE_NAME).exists());
    assert!(!snapshot_path(&second_dir, 1).exists());

    writer
        .rotate_with_manifest(builder(&first_dir, 1, b"stale-alias"))
        .unwrap();
    assert_eq!(
        Manifest::read(&first_dir)
            .unwrap()
            .unwrap()
            .live_snapshot_seq,
        1
    );
    assert!(!second_dir.join(MANIFEST_FILE_NAME).exists());

    drop(writer);
    std::fs::remove_dir_all(root).unwrap();
}
