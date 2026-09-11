use super::tests::fixture;
use super::*;
use std::sync::mpsc;

#[test]
fn verification_never_opens_writable_files_or_synchronizes_directory() {
    let (dir, db, path) = fixture();
    db.session(&path)
        .unwrap()
        .execute("INSERT (:N {id: 1})")
        .unwrap();
    let retained = StoreDirectory::open(dir.path()).unwrap();
    retained.test_forbid_writes();
    let report = Database::verify_in(&DatabaseDirectory(retained.clone())).unwrap();
    assert_eq!(report.nodes, 1);
    assert_eq!(retained.test_write_attempts(), 0);
    drop(db);
    assert!(Database::open_in(&DatabaseDirectory(retained.clone())).is_err());
    assert_eq!(
        retained.test_write_attempts(),
        1,
        "hook must detect actual writable admission"
    );
}

#[test]
fn writable_wal_admission_failure_retains_public_artifact_and_verified_boundary() {
    use std::collections::BTreeMap;
    let (dir, db, path) = fixture();
    db.session(&path)
        .unwrap()
        .execute("INSERT (:N {id: 1})")
        .unwrap();
    drop(db);
    let artifacts = || {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect::<BTreeMap<_, _>>()
    };
    let before = artifacts();
    let retained = StoreDirectory::open(dir.path()).unwrap();
    let report = Database::verify_in(&DatabaseDirectory(retained.clone())).unwrap();
    // LOCK admission succeeds; only subsequent writable admission is denied.
    // This deterministic per-capability permission seam works under privileged CI too.
    let observed = retained.clone();
    retained.test_at_phase("reader.captured", move || observed.test_forbid_writes());
    let error = Database::open_in(&DatabaseDirectory(retained.clone()))
        .err()
        .unwrap();
    assert_eq!(
        (error.phase, error.kind),
        (StoragePhase::Synchronize, StorageErrorKind::Io)
    );
    assert_eq!(error.artifact.as_deref(), Some(report.wal.as_str()));
    assert_eq!(error.offset, Some(report.recovery.position.offset));
    assert_eq!(
        error.expected_sequence,
        Some(report.recovery.position.sequence)
    );
    let mut cause: &(dyn std::error::Error + 'static) = &error;
    while cause.downcast_ref::<std::io::Error>().is_none() {
        cause = cause.source().expect("native I/O cause retained");
    }
    assert_eq!(
        cause.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(retained.test_write_attempts(), 1);
    assert_eq!(
        Database::verify_in(&DatabaseDirectory(retained.clone()))
            .unwrap()
            .nodes,
        1
    );
    assert_eq!(
        retained.test_write_attempts(),
        1,
        "verification remains read-only"
    );
    assert_eq!(artifacts(), before);
    // Failed writer establishment must release its ownership and temporary runtimes.
    drop(Database::open(dir.path()).unwrap());
    assert_eq!(artifacts(), before);
}

#[test]
fn online_verification_pins_selected_disk_state_while_append_checkpoint_and_prune_advance() {
    let (dir, db, path) = fixture();
    db.session(&path)
        .unwrap()
        .execute("INSERT (:N {id: 1})")
        .unwrap();
    let before = Database::verify(dir.path()).unwrap();
    let retained = StoreDirectory::open(dir.path()).unwrap();
    let (selected, ready) = mpsc::channel();
    let (proceed, resume) = mpsc::channel();
    retained.test_at_phase("reader.captured", move || {
        selected.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let worker = std::thread::spawn(move || Database::verify_in(&DatabaseDirectory(retained)));
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    db.session(&path)
        .unwrap()
        .execute("INSERT (:N {id: 2})")
        .unwrap();
    db.checkpoint().unwrap();
    db.checkpoint().unwrap();
    let prune = db.prune().unwrap();
    for name in [&before.manifest, &before.snapshot, &before.wal] {
        assert!(dir.path().join(name).exists());
        assert!(prune.retained.iter().any(|a| &a.artifact.name == name));
    }
    proceed.send(()).unwrap();
    let captured = worker.join().unwrap().unwrap();
    assert_eq!(captured.manifest_digest, before.manifest_digest);
    assert_eq!(captured.recovery.position, before.recovery.position);
    assert_eq!((captured.graphs, captured.nodes), (1, 1));
    assert_eq!(Database::verify(dir.path()).unwrap().nodes, 2);
    let pruned = db.prune().unwrap();
    assert!(pruned.cleanup_error.is_none());
    assert!(pruned.removed.iter().any(|a| a.name == before.manifest));
    assert!(!dir.path().join(&before.snapshot).exists());
}
