use super::*;
use crate::{CreatePolicy, ObjectPath, SchemaPath, TransactionAccessMode};
#[path = "failure_tests.rs"]
mod failure;
#[path = "schedule_tests.rs"]
mod schedule;

fn fixture() -> (tempfile::TempDir, Database, ObjectPath) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::create(dir.path()).unwrap();
    let path = ObjectPath::regular("selene", "data", "graph").unwrap();
    db.catalog()
        .create_schema(&path.schema_path(), CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    (dir, db, path)
}

#[test]
fn public_create_checkpoint_suffix_reopen_and_session_retained_lock() {
    let dir = tempfile::tempdir().unwrap();
    let database = Database::create(dir.path()).unwrap();
    let catalog = database.catalog();
    let schema = SchemaPath::regular("selene", "memory").unwrap();
    let path = ObjectPath::regular("selene", "memory", "episodes").unwrap();
    catalog
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    catalog
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let session = database.session(&path).unwrap();
    session.execute("INSERT (:Item {n: 1})").unwrap();
    session
        .start_transaction(TransactionAccessMode::ReadWrite)
        .unwrap();
    session.execute("INSERT (:Item {n: 99})").unwrap();
    session.rollback_transaction().unwrap();
    let checkpoint = database.checkpoint().unwrap();
    assert_eq!(checkpoint.position.sequence, 3);
    session.execute("INSERT (:Item {n: 2})").unwrap();
    let identity = database.durable_status().unwrap().position;
    drop(catalog);
    drop(database);
    let error = Database::open(dir.path()).err().unwrap();
    assert_eq!(error.kind, StorageErrorKind::Contention);
    drop(session);
    let database = Database::open(dir.path()).unwrap();
    let recovered = database.durable_status().unwrap().position;
    assert_eq!(identity, recovered);
    assert_eq!(database.recovery_info().unwrap().replayed_suffix_records, 1);
    let session = database.session(&path).unwrap();
    assert_eq!(
        session.execute("MATCH (n) RETURN n").unwrap().row_count(),
        Some(2)
    );
    assert_eq!(
        session
            .execute("MATCH (n) WHERE n.n = 99 RETURN n")
            .unwrap()
            .row_count(),
        Some(0)
    );
    session.execute("INSERT (:Item {n: 3})").unwrap();
    database.checkpoint().unwrap();
    drop(session);
    drop(database);
    let database = Database::open(dir.path()).unwrap();
    assert_eq!(
        database
            .session(&path)
            .unwrap()
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .row_count(),
        Some(3)
    );
}
