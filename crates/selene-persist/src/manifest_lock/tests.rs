use std::path::PathBuf;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::control::EmptyStoreControl;
fn identity() -> crate::control::CompatibilityIdentity {
    crate::control::CompatibilityIdentity::new(
        "fixture-profile",
        1,
        [7; 32],
        [16, 0, 0],
        "binary",
        1,
    )
    .unwrap()
}

const CHILD_DIR_ENV: &str = "SELENE_MANIFEST_LOCK_CHILD_DIR";

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-manifest-lock-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    dir
}

fn wait_for_path(path: &Path, phase: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {phase}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_child_exit(child: &mut std::process::Child, phase: &str) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let kill = child.kill();
            let reap = child.wait();
            panic!("timed out waiting for {phase}; kill={kill:?}, reap={reap:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn same_directory_guards_serialize_and_lock_file_persists() {
    let dir = temp_dir("same-dir");
    let owner = StoreWriter::acquire(&StoreDirectory::open(&dir).unwrap()).unwrap();
    let first = ManifestEpochGuard::acquire(&owner).unwrap();
    let (contended_tx, contended_rx) = sync_channel(0);
    let (acquired_tx, acquired_rx) = sync_channel(0);
    let worker_owner = owner.clone();
    let worker = thread::spawn(move || {
        set_contention_hook(move || contended_tx.send(()).unwrap());
        let _second = ManifestEpochGuard::acquire(&worker_owner).unwrap();
        acquired_tx.send(()).unwrap();
    });

    contended_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second guard reaches real lock contention");
    assert_eq!(
        acquired_rx.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    );
    drop(first);
    acquired_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second guard acquires after release");
    worker.join().unwrap();

    assert!(dir.join(MANIFEST_LOCK_FILE_NAME).is_file());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn shared_read_guards_coexist_and_block_epoch_mutation() {
    let dir = temp_dir("shared-readers");
    let first = PersistenceReadGuard::acquire(&dir).unwrap();
    let (reader_acquired_tx, reader_acquired_rx) = sync_channel(0);
    let (release_reader_tx, release_reader_rx) = sync_channel(0);
    let reader_dir = dir.clone();
    let reader = thread::spawn(move || {
        let _second = PersistenceReadGuard::acquire(&reader_dir).unwrap();
        reader_acquired_tx.send(()).unwrap();
        release_reader_rx.recv().unwrap();
    });
    reader_acquired_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second shared reader acquires concurrently");

    let (contended_tx, contended_rx) = sync_channel(0);
    let (writer_acquired_tx, writer_acquired_rx) = sync_channel(0);
    let writer_dir = dir.clone();
    let writer = thread::spawn(move || {
        set_contention_hook(move || contended_tx.send(()).unwrap());
        let owner = StoreWriter::acquire(&StoreDirectory::open(&writer_dir).unwrap()).unwrap();
        let _exclusive = ManifestEpochGuard::acquire(&owner).unwrap();
        writer_acquired_tx.send(()).unwrap();
    });
    contended_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("exclusive epoch mutation reaches shared-lock contention");
    assert_eq!(
        writer_acquired_rx.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    );

    drop(first);
    assert_eq!(
        writer_acquired_rx.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout),
        "all shared readers must release before exclusive acquisition"
    );
    release_reader_tx.send(()).unwrap();
    reader.join().unwrap();
    writer_acquired_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("exclusive epoch mutation acquires after readers release");
    writer.join().unwrap();

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn exclusive_epoch_mutation_blocks_shared_reader() {
    let dir = temp_dir("exclusive-blocks-reader");
    let owner = StoreWriter::acquire(&StoreDirectory::open(&dir).unwrap()).unwrap();
    let exclusive = ManifestEpochGuard::acquire(&owner).unwrap();
    let (contended_tx, contended_rx) = sync_channel(0);
    let (acquired_tx, acquired_rx) = sync_channel(0);
    let worker_dir = dir.clone();
    let worker = thread::spawn(move || {
        set_contention_hook(move || contended_tx.send(()).unwrap());
        let _reader = PersistenceReadGuard::acquire(&worker_dir).unwrap();
        acquired_tx.send(()).unwrap();
    });

    contended_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("shared reader reaches exclusive-lock contention");
    assert_eq!(
        acquired_rx.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    );
    drop(exclusive);
    acquired_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("shared reader acquires after exclusive mutation releases");
    worker.join().unwrap();

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn different_directories_do_not_contend() {
    let first_dir = temp_dir("first-dir");
    let second_dir = temp_dir("second-dir");
    let owner = StoreWriter::acquire(&StoreDirectory::open(&first_dir).unwrap()).unwrap();
    let first = ManifestEpochGuard::acquire(&owner).unwrap();

    let (acquired_tx, acquired_rx) = sync_channel(0);
    let worker_dir = second_dir.clone();
    let worker = thread::spawn(move || {
        let owner = StoreWriter::acquire(&StoreDirectory::open(&worker_dir).unwrap()).unwrap();
        let _second = ManifestEpochGuard::acquire(&owner).unwrap();
        acquired_tx.send(()).unwrap();
    });
    acquired_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("independent directory lock acquires immediately");
    worker.join().unwrap();

    drop(first);
    std::fs::remove_dir_all(first_dir).unwrap();
    std::fs::remove_dir_all(second_dir).unwrap();
}

#[test]
fn separate_process_reader_contends_with_exclusive_epoch() {
    let dir = temp_dir("separate-process");
    let owner = StoreWriter::acquire(&StoreDirectory::open(&dir).unwrap()).unwrap();
    let first = ManifestEpochGuard::acquire(&owner).unwrap();
    let contended = dir.join("child-contended");
    let acquired = dir.join("child-acquired");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "manifest_lock::tests::manifest_lock_child_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_DIR_ENV, &dir)
        .env("SELENE_MANIFEST_LOCK_CHILD_READ", "1")
        .spawn()
        .unwrap();

    wait_for_path(&contended, "child-process lock contention");
    assert!(child.try_wait().unwrap().is_none());
    drop(first);
    assert!(child.wait().unwrap().success());
    assert!(acquired.is_file());

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn separate_process_epoch_mutation_contends_with_shared_reader() {
    let dir = temp_dir("separate-process-reader");
    let reader = PersistenceReadGuard::acquire(&dir).unwrap();
    let contended = dir.join("child-contended");
    let acquired = dir.join("child-acquired");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "manifest_lock::tests::manifest_lock_child_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_DIR_ENV, &dir)
        .spawn()
        .unwrap();

    wait_for_path(&contended, "child-process shared-lock contention");
    assert!(child.try_wait().unwrap().is_none());
    drop(reader);
    assert!(wait_for_child_exit(&mut child, "child-process lock release").success());
    assert!(acquired.is_file());

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "helper invoked by the separate-process epoch tests"]
fn manifest_lock_child_helper() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV).map(PathBuf::from) else {
        return;
    };
    let contended = dir.join("child-contended");
    let acquired = dir.join("child-acquired");
    set_contention_hook(move || std::fs::write(contended, []).unwrap());
    if std::env::var_os("SELENE_MANIFEST_LOCK_CHILD_READ").is_some() {
        let _guard = PersistenceReadGuard::acquire(&dir).unwrap();
        std::fs::write(acquired, []).unwrap();
    } else {
        let owner = StoreWriter::acquire(&StoreDirectory::open(&dir).unwrap()).unwrap();
        let _guard = ManifestEpochGuard::acquire(&owner).unwrap();
        std::fs::write(acquired, []).unwrap();
    }
}

#[test]
fn direct_manifest_publication_uses_the_epoch_lock() {
    let dir = temp_dir("manifest-write");
    let cap = StoreDirectory::open(&dir).unwrap();
    let mut control = EmptyStoreControl::create_empty(&cap, identity()).unwrap();
    let first = PersistenceReadGuard::acquire_in(&cap).unwrap();
    let (contended_tx, contended_rx) = sync_channel(0);
    let before = std::fs::read(dir.join("CURRENT")).unwrap();
    let worker = thread::spawn(move || {
        set_contention_hook(move || contended_tx.send(()).unwrap());
        control.publish_empty().unwrap();
    });

    contended_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("MANIFEST writer reaches the held epoch lock");
    assert_eq!(std::fs::read(dir.join("CURRENT")).unwrap(), before);
    drop(first);
    worker.join().unwrap();
    assert_eq!(
        EmptyStoreControl::open(&cap, &identity())
            .unwrap()
            .manifest()
            .generation()
            .get(),
        2
    );

    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn guard_keeps_publication_on_the_resolved_directory_after_alias_retarget() {
    use std::os::unix::fs::symlink;

    let root = temp_dir("alias-retarget").canonicalize().unwrap();
    let first_dir = root.join("first");
    let second_dir = root.join("second");
    let alias = root.join("live");
    std::fs::create_dir(&first_dir).unwrap();
    std::fs::create_dir(&second_dir).unwrap();
    symlink(&first_dir, &alias).unwrap();
    let cap = StoreDirectory::open(&alias).unwrap();
    let mut control = EmptyStoreControl::create_empty(&cap, identity()).unwrap();
    let guard = PersistenceReadGuard::acquire_in(&cap).unwrap();
    assert_eq!(guard.dir(), first_dir);

    std::fs::remove_file(&alias).unwrap();
    symlink(&second_dir, &alias).unwrap();
    drop(guard);
    control.publish_empty().unwrap();
    drop(control);
    assert_eq!(
        EmptyStoreControl::open(&StoreDirectory::open(&first_dir).unwrap(), &identity())
            .unwrap()
            .manifest()
            .generation()
            .get(),
        2
    );
    assert!(!second_dir.join("CURRENT").exists());
    std::fs::remove_dir_all(root).unwrap();
}
