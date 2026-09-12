//! F03-PR03 facade effect regressions.
//!
//! Proves that read-only transactions reject direct mutations and effectful
//! procedures before publication (publishing nothing), that multi-statement
//! writes stage in the active transaction or trigger documented rollback, and
//! that catalog/data mixing never silently splits into separate commits.

use selene_db::{
    CreatePolicy, Database, ErrorKind, ObjectPath, SchemaPath, Session, TransactionAccessMode,
    TransactionState,
};

fn schema(name: &str) -> SchemaPath {
    SchemaPath::regular("selene", name).unwrap()
}

fn graph(schema: &str, name: &str) -> ObjectPath {
    ObjectPath::regular("selene", schema, name).unwrap()
}

fn fixture(name: &str) -> (Database, ObjectPath, Session, Session) {
    let database = Database::builder().build();
    let catalog = database.catalog();
    let path = graph(name, "main");
    catalog
        .create_schema(&schema(name), CreatePolicy::Strict)
        .unwrap();
    catalog
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let first = database.session(&path).unwrap();
    let second = database.session(&path).unwrap();
    (database, path, first, second)
}

fn status(error: &selene_db::Error) -> String {
    error
        .gqlstatus()
        .expect("effect error has status")
        .as_str()
        .to_owned()
}

fn count(session: &Session, label: &str) -> usize {
    session
        .execute(&format!("MATCH (n:{label}) RETURN n"))
        .unwrap()
        .row_count()
        .unwrap()
}

#[test]
fn read_only_rejects_direct_mutation_without_publishing() {
    let (database, _path, reader, observer) = fixture("logical_read_direct");
    let before = database.catalog().snapshot();
    let before_generation = before.generation();
    reader
        .start_transaction(TransactionAccessMode::ReadOnly)
        .unwrap();
    let error = reader.execute("INSERT (:Forbidden)").unwrap_err();
    assert_eq!(status(&error), "25G03");
    assert_eq!(error.kind(), ErrorKind::ReadOnlyTransaction);
    // Nothing was published: the catalog snapshot is shared and the observer
    // sees no rows.
    assert!(database.catalog().snapshot().shares_state_with(&before));
    assert_eq!(
        database.catalog().snapshot().generation(),
        before_generation
    );
    assert_eq!(count(&observer, "Forbidden"), 0);
    // The failed statement marks the transaction failed; commit reports the
    // documented in-failed-transaction outcome and leaves nothing published.
    let error = reader.commit_transaction().unwrap_err();
    assert_eq!(status(&error), "25N02");
    assert_eq!(count(&observer, "Forbidden"), 0);
}

#[test]
fn read_only_rejects_effectful_procedure_without_publishing() {
    let (database, _path, reader, observer) = fixture("logical_read_proc");
    // Seed one item so the text index has a target property.
    observer.execute("INSERT (:Item { body: 'text' })").unwrap();
    let before = database.catalog().snapshot();
    let before_generation = before.generation();
    reader
        .start_transaction(TransactionAccessMode::ReadOnly)
        .unwrap();
    // `selene.create_text_index` is a mutation-tier procedure with catalog
    // effects resolved from registration metadata. A read-only transaction
    // must reject it before publication, publishing nothing.
    let error = reader
        .execute("CALL selene.create_text_index('Item', 'body')")
        .unwrap_err();
    assert_eq!(
        status(&error),
        "25G03",
        "read-only must reject catalog write"
    );
    assert!(database.catalog().snapshot().shares_state_with(&before));
    assert_eq!(
        database.catalog().snapshot().generation(),
        before_generation
    );
    assert_eq!(count(&observer, "Item"), 1);
    reader.rollback_transaction().unwrap();
}

#[test]
fn multi_statement_writes_stage_until_commit_or_rollback() {
    let (_database, _path, session, observer) = fixture("logical_multi");
    session.execute("START TRANSACTION").unwrap();
    session.execute("INSERT (:Staged { n: 1 })").unwrap();
    session.execute("INSERT (:Staged { n: 2 })").unwrap();
    // Staged writes are visible to the staging session through its detached
    // state but not to the observer before the single publication.
    assert_eq!(count(&session, "Staged"), 2);
    assert_eq!(count(&observer, "Staged"), 0);
    session.execute("COMMIT").unwrap();
    let committed = session.context().transaction().unwrap();
    assert_eq!(committed.state(), TransactionState::Committed);
    assert_eq!(count(&observer, "Staged"), 2);

    // A failing statement triggers the documented rollback behavior:
    // staged work is discarded and the observer never sees it.
    session.execute("START TRANSACTION").unwrap();
    session.execute("INSERT (:Discarded)").unwrap();
    let error = session.execute("RETURN 1 / 0").unwrap_err();
    assert_eq!(status(&error), "22012");
    session.execute("ROLLBACK").unwrap();
    assert_eq!(count(&observer, "Discarded"), 0);
}

#[test]
fn catalog_data_mixing_never_silently_splits() {
    let (database, _path, session, observer) = fixture("logical_mixing");
    session.execute("START TRANSACTION").unwrap();
    session.execute("INSERT (:MixedData)").unwrap();
    let error = session.execute("CREATE SCHEMA /mixed_catalog").unwrap_err();
    assert_eq!(status(&error), "25G02");
    // The unsupported mix did not split into separate commits: the observer
    // sees neither the staged data nor the catalog object.
    assert_eq!(count(&observer, "MixedData"), 0);
    assert!(
        database
            .catalog()
            .snapshot()
            .resolve_schema(&schema("mixed_catalog"))
            .is_err()
    );
    session.execute("ROLLBACK").unwrap();
    assert_eq!(count(&observer, "MixedData"), 0);
}
