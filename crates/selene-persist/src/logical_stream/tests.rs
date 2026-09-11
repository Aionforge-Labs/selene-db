use super::*;
use crate::control::{CompatibilityIdentity, EmptyStoreControl};

fn identity() -> CompatibilityIdentity {
    CompatibilityIdentity::new("commit-test", 1, [7; 32], [17, 0, 0], "binary", 1).unwrap()
}

fn fixture() -> (
    selene_testing::PersistenceTestPath,
    StoreDirectory,
    LogicalWal,
) {
    let temp = selene_testing::PersistenceTestPath::new();
    let dir = StoreDirectory::open(temp.parent().unwrap()).unwrap();
    let control = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    let wal = LogicalWal::create(control).unwrap();
    (temp, dir, wal)
}

fn bodies(dir: &StoreDirectory) -> Vec<Vec<u8>> {
    let mut reader = LogicalReader::open(dir, &identity(), 1024).unwrap();
    let mut result = Vec::new();
    while let Some(body) = reader.next_body().unwrap() {
        result.push(body);
    }
    result
}

fn commit(wal: &mut LogicalWal, bodies: &[&[u8]]) {
    let prepared = wal.prepare(bodies, Compression::Raw, 1024).unwrap();
    wal.commit(
        prepared,
        || false,
        |publication| {
            publication.mark_published();
            Ok(())
        },
    )
    .unwrap();
}

#[test]
fn acknowledged_prefix_survives_partial_group_durable_rollback() {
    let (_temp, dir, mut wal) = fixture();
    commit(&mut wal, &[b"acknowledged"]);
    let prefix = wal.progress();
    let group = wal
        .prepare(&[b"canceled-one", b"canceled-two"], Compression::Raw, 1024)
        .unwrap();
    wal.fault = Some(Fault::PartialAppend);
    let failure = wal
        .commit(group, || false, |_| panic!("must not publish"))
        .unwrap_err();
    assert_eq!(failure.durability, Durability::Canceled);
    assert_eq!(failure.phase, CommitPhase::Append);
    assert_eq!(wal.progress().synchronized, prefix.synchronized);
    assert_eq!(wal.progress().acknowledged, prefix.acknowledged);
    assert!(wal.is_fenced());
    drop(wal);
    assert_eq!(bodies(&dir), vec![b"acknowledged".to_vec()]);
}

#[test]
fn cleanup_failures_remain_uncertain_and_fence_admission() {
    for fault in [Fault::Truncate, Fault::CleanupSync] {
        let (_temp, dir, mut wal) = fixture();
        commit(&mut wal, &[b"prefix"]);
        let prepared = wal
            .prepare(&[b"one", b"two"], Compression::Raw, 1024)
            .unwrap();
        wal.fault = Some(fault);
        let error = wal
            .commit(prepared, || false, |_| panic!("must not publish"))
            .unwrap_err();
        assert_eq!(error.durability, Durability::Uncertain);
        assert!(error.cleanup.is_some());
        assert!(wal.is_fenced());
        assert!(wal.prepare(&[b"later"], Compression::Raw, 1024).is_err());
        drop(wal);
        assert_eq!(bodies(&dir)[0], b"prefix");
    }
}

#[test]
fn synchronized_record_is_not_undone_by_publication_unwind_or_cancellation() {
    for published in [false, true] {
        let (_temp, dir, mut wal) = fixture();
        let prepared = wal.prepare(&[b"whole"], Compression::Raw, 1024).unwrap();
        let error = wal
            .commit(
                prepared,
                || false,
                |publication| {
                    if published {
                        publication.mark_published();
                    }
                    panic!("publication or observer unwind")
                },
            )
            .unwrap_err();
        assert_eq!(error.durability, Durability::Committed);
        assert_eq!(error.progress.published.is_some(), published);
        assert!(error.progress.acknowledged.is_none());
        assert_eq!(error.progress.synchronized.sequence, 1);
        assert!(wal.is_fenced());
        drop(wal);
        assert_eq!(bodies(&dir), vec![b"whole".to_vec()]);
    }
}

#[test]
fn canceled_before_append_is_absent_and_writer_remains_usable() {
    let (_temp, dir, mut wal) = fixture();
    let prepared = wal.prepare(&[b"canceled"], Compression::Raw, 1024).unwrap();
    let error = wal
        .commit(prepared, || true, |_| panic!("must not publish"))
        .unwrap_err();
    assert_eq!(error.durability, Durability::Canceled);
    assert_eq!(error.progress.written.sequence, 0);
    assert!(!wal.is_fenced());
    commit(&mut wal, &[b"later"]);
    drop(wal);
    assert_eq!(bodies(&dir), vec![b"later".to_vec()]);
}

#[test]
fn sync_failure_does_not_advance_successful_sync_and_rollback_keeps_prefix() {
    let (_temp, dir, mut wal) = fixture();
    commit(&mut wal, &[b"prefix"]);
    let prefix = wal.progress();
    let group = wal
        .prepare(&[b"one", b"two"], Compression::Raw, 1024)
        .unwrap();
    wal.fault = Some(Fault::Synchronize);
    let error = wal
        .commit(group, || false, |_| panic!("must not publish"))
        .unwrap_err();
    assert_eq!(error.phase, CommitPhase::Synchronize);
    assert_eq!(error.durability, Durability::Canceled);
    assert_eq!(error.progress, prefix);
    drop(wal);
    assert_eq!(bodies(&dir), vec![b"prefix".to_vec()]);
}

#[test]
fn cancellation_after_sync_retains_whole_group_without_publication() {
    let (_temp, dir, mut wal) = fixture();
    let group = wal
        .prepare(&[b"one", b"two"], Compression::Raw, 1024)
        .unwrap();
    let polls = std::cell::Cell::new(0);
    let error = wal
        .commit(
            group,
            || {
                polls.set(polls.get() + 1);
                polls.get() == 2
            },
            |_| panic!("must not publish"),
        )
        .unwrap_err();
    assert_eq!(error.phase, CommitPhase::Publish);
    assert_eq!(error.durability, Durability::Committed);
    assert_eq!(error.progress.synchronized.sequence, 2);
    assert!(error.progress.published.is_none());
    assert!(wal.is_fenced());
    drop(wal);
    assert_eq!(bodies(&dir), vec![b"one".to_vec(), b"two".to_vec()]);
}

#[test]
fn stale_or_foreign_prepared_groups_cannot_reuse_an_offset() {
    let (_temp, dir, mut wal) = fixture();
    let (_other_temp, _other_dir, mut other) = fixture();
    let foreign = wal.prepare(&[b"same"], Compression::Raw, 1024).unwrap();
    let error = other
        .commit(foreign, || false, |_| panic!("foreign publication"))
        .unwrap_err();
    assert_eq!(error.durability, Durability::Canceled);
    assert!(error.candidate.is_none());
    let stale = wal.prepare(&[b"same"], Compression::Raw, 1024).unwrap();
    commit(&mut wal, &[b"same"]);
    let error = wal
        .commit(stale, || false, |_| panic!("stale publication"))
        .unwrap_err();
    assert_eq!(error.durability, Durability::Canceled);
    assert!(error.candidate.is_none());
    assert_eq!(bodies(&dir), vec![b"same".to_vec()]);
}

#[test]
fn selected_control_is_independent_of_frame_bytes_and_unselected_ancestors() {
    let (_temp, dir, mut wal) = fixture();
    commit(&mut wal, &[b"body"]);
    assert!(StoreWriter::acquire(&dir).is_err());
    drop(wal);
    assert!(EmptyStoreControl::open(&dir, &identity()).is_err());
    assert!(dir.require_legacy().is_err());
    let owner = StoreWriter::acquire(&dir).unwrap();
    dir.remove(std::path::Path::new(
        "MANIFEST-00000000000000000001.control",
    ))
    .unwrap();
    dir.sync().unwrap();
    assert_eq!(bodies(&dir), vec![b"body".to_vec()]);
    let mut file = dir
        .open_write(std::path::Path::new(crate::control::logical::LOG_NAME))
        .unwrap();
    file.seek(SeekFrom::Start(logical_frame::HEADER_LEN as u64))
        .unwrap();
    file.write_all(b"x").unwrap();
    file.sync_all().unwrap();
    let mut reader = LogicalReader::open(&dir, &identity(), 1024).unwrap();
    assert!(matches!(reader.next_body(), Err(StreamError::Frame(_))));
    drop(reader);
    drop(owner);
}

#[test]
fn complete_corruption_is_not_salvaged_as_an_unsealed_tail() {
    let (_temp, dir, mut wal) = fixture();
    commit(&mut wal, &[b"prefix"]);
    let prefix = wal.progress().synchronized;
    commit(&mut wal, &[b"tail"]);
    let full = wal.progress().synchronized;
    drop(wal);
    let owner = StoreWriter::acquire(&dir).unwrap();
    let file = dir
        .open_write(std::path::Path::new(crate::control::logical::LOG_NAME))
        .unwrap();
    file.set_len(full.offset - 1).unwrap();
    file.sync_all().unwrap();
    let mut reader = LogicalReader::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reader.next_body().unwrap().unwrap(), b"prefix");
    assert!(reader.next_body().unwrap().is_none());
    assert!(reader.incomplete_tail());
    drop(reader);
    let mut file = file;
    file.seek(SeekFrom::Start(prefix.offset)).unwrap();
    file.write_all(b"SLDB").unwrap();
    file.sync_all().unwrap();
    let mut reader = LogicalReader::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reader.next_body().unwrap().unwrap(), b"prefix");
    assert!(reader.next_body().is_err());
    drop(reader);
    drop(owner);
}

#[test]
fn blocked_reader_cannot_escape_before_publication_and_ack_ordering() {
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;
    let (_temp, dir, wal) = fixture();
    let wal = Arc::new(Mutex::new(wal));
    let writer = Arc::clone(&wal);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut wal = writer.lock().unwrap();
        let prepared = wal
            .prepare(&[b"one", b"two"], Compression::Raw, 1024)
            .unwrap();
        wal.commit(
            prepared,
            || false,
            |publication| {
                entered_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                publication.mark_published();
                Ok(())
            },
        )
        .unwrap()
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        crate::manifest_lock::set_contention_hook(move || {
            blocked_tx.send(()).unwrap();
        });
        done_tx.send(bodies(&dir)).unwrap();
    });
    blocked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(done_rx.try_recv().is_err());
    resume_tx.send(()).unwrap();
    let acknowledged = worker.join().unwrap();
    assert_eq!(acknowledged.sequence, 2);
    assert_eq!(
        wal.lock().unwrap().progress().acknowledged,
        Some(acknowledged)
    );
    assert_eq!(
        done_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        vec![b"one".to_vec(), b"two".to_vec()]
    );
    reader.join().unwrap();
}

#[test]
fn cancellation_after_publication_is_committed_unacknowledged_and_fences_waiters() {
    let (_temp, dir, mut wal) = fixture();
    let group = wal
        .prepare(&[b"one", b"two"], Compression::Raw, 1024)
        .unwrap();
    let waiting = wal.prepare(&[b"later"], Compression::Raw, 1024).unwrap();
    let polls = std::cell::Cell::new(0);
    let error = wal
        .commit(
            group,
            || {
                polls.set(polls.get() + 1);
                polls.get() == 3
            },
            |publication| {
                publication.mark_published();
                Ok(())
            },
        )
        .unwrap_err();
    assert_eq!(error.durability, Durability::Committed);
    assert_eq!(error.phase, CommitPhase::Acknowledge);
    assert_eq!(error.progress.published, error.candidate);
    assert!(error.progress.acknowledged.is_none());
    let error = wal
        .commit(waiting, || false, |_| panic!("fenced waiter published"))
        .unwrap_err();
    assert_eq!(error.durability, Durability::Canceled);
    assert_eq!(error.phase, CommitPhase::Prepare);
    drop(wal);
    assert_eq!(bodies(&dir), vec![b"one".to_vec(), b"two".to_vec()]);
}

#[test]
fn dropping_prepared_group_has_no_unowned_work_and_cannot_append() {
    let (_temp, dir, mut wal) = fixture();
    drop(wal.prepare(&[b"never"], Compression::Raw, 1024).unwrap());
    assert_eq!(wal.progress().written.sequence, 0);
    commit(&mut wal, &[b"owned"]);
    drop(wal);
    assert_eq!(bodies(&dir), vec![b"owned".to_vec()]);
}

#[test]
fn selected_manifest_does_not_trust_a_foreign_self_consistent_log() {
    use std::io::Read;
    let (_temp, dir, mut wal) = fixture();
    let (_other_temp, other, mut foreign) = fixture();
    commit(&mut wal, &[b"local"]);
    commit(&mut foreign, &[b"foreign"]);
    drop(wal);
    drop(foreign);
    let mut bytes = Vec::new();
    other
        .open_read(crate::control::logical::LOG_NAME)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let _owner = StoreWriter::acquire(&dir).unwrap();
    let mut file = dir
        .open_write(std::path::Path::new(crate::control::logical::LOG_NAME))
        .unwrap();
    file.set_len(0).unwrap();
    file.write_all(&bytes).unwrap();
    file.sync_all().unwrap();
    let mut reader = LogicalReader::open(&dir, &identity(), 1024).unwrap();
    assert!(matches!(
        reader.next_body(),
        Err(StreamError::Frame(logical_frame::FrameError::Context))
    ));
}

#[test]
fn failed_log_bootstrap_never_selects_incomplete_control_or_guesses_empty_state() {
    for point in [
        "manifest.write",
        "manifest.file_sync",
        "manifest.publish",
        "current.write",
        "current.replace",
        "current.dir_sync",
    ] {
        let temp = selene_testing::PersistenceTestPath::new();
        let dir = StoreDirectory::open(temp.parent().unwrap()).unwrap();
        let empty = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
        dir.fail_at(point);
        assert!(LogicalWal::create(empty).is_err(), "{point}");
        let reader = LogicalReader::open(&dir, &identity(), 1024);
        if point == "current.dir_sync" {
            assert!(reader.unwrap().next_body().unwrap().is_none());
        } else {
            assert!(reader.is_err());
        }
        assert!(EmptyStoreControl::open(&dir, &identity()).is_err());
        assert!(dir.require_legacy().is_err());
    }
}
