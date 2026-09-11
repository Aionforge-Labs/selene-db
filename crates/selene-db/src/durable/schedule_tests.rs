use super::*;
use std::{sync::mpsc, thread, time::Duration};

#[test]
fn checkpoint_pins_one_publication_blocks_writer_and_preserves_held_reader() {
    let (dir, db, path) = fixture();
    let reader = db.session(&path).unwrap();
    reader.execute("INSERT (:Item {n: 1})").unwrap();
    reader
        .start_transaction(TransactionAccessMode::ReadOnly)
        .unwrap();
    let held = db.catalog().snapshot();
    let (pinned_tx, pinned_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    *db.inner.checkpoint_pause.lock() = Some((pinned_tx, resume_rx));
    let checkpoint_db = db.clone();
    let checkpoint = thread::spawn(move || checkpoint_db.checkpoint().unwrap());
    let publication = pinned_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (blocked_tx, blocked_rx) = mpsc::channel();
    *db.inner.mutation_blocked.lock() = Some(blocked_tx);
    let writer_db = db.clone();
    let writer_path = path.clone();
    let writer = thread::spawn(move || {
        writer_db
            .session(&writer_path)
            .unwrap()
            .execute("INSERT (:Item {n: 2})")
            .unwrap()
    });
    blocked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(db.catalog().snapshot().shares_state_with(&held));
    assert_eq!(
        reader.execute("MATCH (n) RETURN n").unwrap().row_count(),
        Some(1)
    );
    resume_tx.send(()).unwrap();
    let outcome = checkpoint.join().unwrap();
    writer.join().unwrap();
    assert_eq!(outcome.publication, publication);
    assert_eq!(outcome.position.sequence, publication);
    assert_eq!(
        reader.execute("MATCH (n) RETURN n").unwrap().row_count(),
        Some(1)
    );
    assert_eq!(
        db.session(&path)
            .unwrap()
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .row_count(),
        Some(2)
    );
    drop(reader);
    drop(held);
    drop(db);
    let reopened = Database::open(dir.path()).unwrap();
    assert_eq!(reopened.recovery_info().unwrap().replayed_suffix_records, 1);
    assert_eq!(
        reopened
            .session(&path)
            .unwrap()
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .row_count(),
        Some(2)
    );
}

#[test]
fn detached_precheckpoint_write_can_commit_after_checkpoint_without_id_floor_drift() {
    let (dir, db, path) = fixture();
    let s = db.session(&path).unwrap();
    s.start_transaction(TransactionAccessMode::ReadWrite)
        .unwrap();
    s.execute("INSERT (:Pending)").unwrap();
    db.checkpoint().unwrap(); // must not invent a live publication or reserve its unpublished IDs
    s.commit_transaction().unwrap();
    drop(s);
    drop(db);
    let reopened = Database::open(dir.path()).unwrap();
    assert_eq!(
        reopened
            .session(&path)
            .unwrap()
            .execute("MATCH (n:Pending) RETURN n")
            .unwrap()
            .row_count(),
        Some(1)
    );
}
