use std::path::PathBuf;
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use selene_core::{Change, HlcTimestamp, NodeId, Origin};

use super::*;
use crate::{PersistenceReadGuard, WalConfig, WalReader, WalWriter};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-wal-path-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn append_one(writer: &mut WalWriter) {
    writer
        .append(
            HlcTimestamp::new(1, 0),
            Origin::Local,
            None,
            &[Change::NodeDeleted { id: NodeId::new(1) }],
        )
        .unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn replaced_real_parent_cannot_redirect_wal_initialization() {
    let root = temp_dir("real-parent-replacement");
    let original = root.join("store");
    let retained = root.join("retained");
    std::fs::create_dir(&original).unwrap();
    let hook_original = original.clone();
    let hook_retained = retained.clone();
    set_after_parent_anchor_hook(move || {
        std::fs::rename(&hook_original, &hook_retained).unwrap();
        std::fs::create_dir(&hook_original).unwrap();
    });

    let mut writer = WalWriter::open(&original.join("wal.log"), WalConfig::default()).unwrap();
    append_one(&mut writer);
    writer.flush().unwrap();
    drop(writer);
    let replacement_entries = std::fs::read_dir(&original).unwrap().count();
    let anchored_wal_exists = retained.join("wal.log").is_file();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(
        replacement_entries, 0,
        "replacement directory must remain untouched"
    );
    assert!(
        anchored_wal_exists,
        "WAL must be created in the retained directory"
    );
}

#[cfg(unix)]
#[test]
fn parent_alias_is_anchored_before_the_final_path_opens() {
    use std::os::unix::fs::symlink;

    let root = temp_dir("parent-retarget");
    let first_dir = root.join("first");
    let second_dir = root.join("second");
    let alias = root.join("live");
    std::fs::create_dir(&first_dir).unwrap();
    std::fs::create_dir(&second_dir).unwrap();
    symlink(&first_dir, &alias).unwrap();

    let hook_alias = alias.clone();
    let hook_second = second_dir.clone();
    set_after_parent_anchor_hook(move || {
        std::fs::remove_file(&hook_alias).unwrap();
        symlink(&hook_second, &hook_alias).unwrap();
    });
    let mut first = WalWriter::open(&alias.join("wal.log"), WalConfig::default()).unwrap();
    let mut second = WalWriter::open(&alias.join("wal.log"), WalConfig::default()).unwrap();

    assert_eq!(first.path(), first_dir.join("wal.log"));
    assert_eq!(second.path(), second_dir.join("wal.log"));
    assert!(matches!(
        WalWriter::open(&first_dir.join("wal.log"), WalConfig::default()),
        Err(PersistError::WriterLockHeld)
    ));
    assert!(matches!(
        WalWriter::open(&second_dir.join("wal.log"), WalConfig::default()),
        Err(PersistError::WriterLockHeld)
    ));
    append_one(&mut first);
    append_one(&mut second);
    first.flush().unwrap();
    second.flush().unwrap();
    drop(first);
    drop(second);
    let first = WalWriter::open(&first_dir.join("wal.log"), WalConfig::default()).unwrap();
    let second = WalWriter::open(&second_dir.join("wal.log"), WalConfig::default()).unwrap();
    assert_eq!(first.last_sequence(), 1);
    assert_eq!(second.last_sequence(), 1);
    drop(first);
    drop(second);

    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn final_symlink_is_rejected_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("final-symlink");
    let backing = dir.join("backing.log");
    drop(WalWriter::open(&backing, WalConfig::default()).unwrap());
    let before = std::fs::read(&backing).unwrap();
    let active = dir.join("wal.log");
    symlink(&backing, &active).unwrap();

    let error = match WalWriter::open(&active, WalConfig::default()) {
        Ok(_) => panic!("final symlink must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        PersistError::WalPathNotRegular { path } if path == active
    ));
    assert_eq!(std::fs::read(&backing).unwrap(), before);
    drop(WalWriter::open(&backing, WalConfig::default()).unwrap());

    std::fs::remove_file(&active).unwrap();
    symlink(dir.join("missing.log"), &active).unwrap();
    assert!(matches!(
        WalWriter::open(&active, WalConfig::default()),
        Err(PersistError::WalPathNotRegular { path }) if path == active
    ));

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn final_directory_is_rejected_without_blocking_on_open() {
    let dir = temp_dir("final-directory");
    let active = dir.join("wal.log");
    std::fs::create_dir(&active).unwrap();

    assert!(matches!(
        WalWriter::open(&active, WalConfig::default()),
        Err(PersistError::WalPathNotRegular { path }) if path == active
    ));

    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn hard_link_alias_still_contends_on_the_open_inode() {
    let dir = temp_dir("hard-link");
    let active = dir.join("wal.log");
    let alias = dir.join("wal.alias");
    let writer = WalWriter::open(&active, WalConfig::default()).unwrap();
    std::fs::hard_link(&active, &alias).unwrap();

    assert!(matches!(
        WalWriter::open(&alias, WalConfig::default()),
        Err(PersistError::WriterLockHeld)
    ));

    drop(writer);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn new_wal_is_absent_until_its_complete_header_is_published() {
    let dir = temp_dir("atomic-publish");
    let active = dir.join("wal.log");
    let guard = PersistenceReadGuard::acquire(&dir).unwrap();
    let (staged_tx, staged_rx) = sync_channel(0);
    let (release_tx, release_rx) = sync_channel(0);
    let (done_tx, done_rx) = sync_channel(1);
    let worker_active = active.clone();
    let worker = thread::spawn(move || {
        set_before_wal_publish_hook(move || {
            staged_tx.send(()).unwrap();
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("WAL publication release arrives before timeout");
        });
        done_tx
            .send(
                WalWriter::open(
                    &worker_active,
                    WalConfig {
                        snapshot_seq: 42,
                        ..WalConfig::default()
                    },
                )
                .map(|writer| writer.snapshot_seq()),
            )
            .unwrap();
    });
    staged_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("new WAL reaches pre-publication hook");

    assert!(!active.exists());
    assert!(guard.read_manifest().unwrap().is_none());
    let staged_paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("wal.log.init."))
        })
        .collect();
    assert_eq!(staged_paths.len(), 1);
    assert_eq!(
        WalReader::open(&staged_paths[0]).unwrap().snapshot_seq(),
        42
    );

    release_tx.send(()).unwrap();
    assert_eq!(
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("new WAL publishes after release")
            .unwrap(),
        42
    );
    worker.join().unwrap();
    assert_eq!(WalReader::open(&active).unwrap().snapshot_seq(), 42);
    assert!(staged_paths.iter().all(|path| !path.exists()));

    drop(guard);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn store_writer_excludes_a_racing_initializer_before_wal_publication() {
    let dir = temp_dir("atomic-publish-race");
    let active = dir.join("wal.log");
    let (first_staged_tx, first_staged_rx) = sync_channel(0);
    let (release_first_tx, release_first_rx) = sync_channel(0);
    let (first_done_tx, first_done_rx) = sync_channel(1);
    let first_active = active.clone();
    let first = thread::spawn(move || {
        set_before_wal_publish_hook(move || {
            first_staged_tx.send(()).unwrap();
            release_first_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("first initializer release arrives before timeout");
        });
        first_done_tx
            .send(WalWriter::open(
                &first_active,
                WalConfig {
                    snapshot_seq: 11,
                    ..WalConfig::default()
                },
            ))
            .unwrap();
    });
    first_staged_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first initializer stages its header");

    assert!(matches!(
        WalWriter::open(
            &active,
            WalConfig {
                snapshot_seq: 22,
                ..WalConfig::default()
            }
        ),
        Err(PersistError::WriterLockHeld)
    ));
    assert!(
        !active.exists(),
        "contender cannot publish before the owner"
    );
    release_first_tx.send(()).unwrap();
    let writer = first_done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    first.join().unwrap();
    assert_eq!(WalReader::open(&active).unwrap().snapshot_seq(), 11);
    drop(writer);

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn initialization_collision_preserves_the_complete_existing_header() {
    let dir = temp_dir("exclusive-publish-collision");
    let active = dir.join("wal.log");
    let hook_path = active.clone();
    set_before_wal_publish_hook(move || {
        // Inject an independent publisher at the no-overwrite boundary.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&hook_path)
            .unwrap();
        WalFileHeader::new(22).write_to(&mut file).unwrap();
        file.sync_all().unwrap();
    });
    let writer = WalWriter::open(
        &active,
        WalConfig {
            snapshot_seq: 11,
            ..WalConfig::default()
        },
    )
    .unwrap();
    assert_eq!(writer.snapshot_seq(), 22);
    assert_eq!(WalReader::open(&active).unwrap().snapshot_seq(), 22);
    drop(writer);
    std::fs::remove_dir_all(dir).unwrap();
}
