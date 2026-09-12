//! F04-PR05: batch writes preserve the facade's statement/transaction authority.

use selene_db::{
    CreatePolicy, Database, GraphTypeDefinition, NodeTypeDefinition, ObjectPath, PathSegment,
    PropertyDefinition, RequestSlotState, SchemaPath, Session, TransactionAccessMode,
    TransactionSlotState, Type, WriteSummary,
};

fn name(text: &str) -> PathSegment {
    PathSegment::regular(text).unwrap()
}

fn fixture(db: &Database) -> (ObjectPath, Session) {
    let path = ObjectPath::regular("selene", "batch", "data").unwrap();
    let ty = ObjectPath::regular("selene", "batch", "shape").unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "batch").unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    let shape = GraphTypeDefinition::builder()
        .with_node_type(
            NodeTypeDefinition::new(name("Seed"), vec![name("Seed")])
                .unwrap()
                .with_property(PropertyDefinition::new(name("k"), Type::INT64).unwrap()),
        )
        .with_node_type(
            NodeTypeDefinition::new(name("Item"), vec![name("Item")])
                .unwrap()
                .with_property(
                    PropertyDefinition::new(name("id"), Type::INT64)
                        .unwrap()
                        .unique(),
                )
                .with_property(PropertyDefinition::new(name("text"), Type::STRING).unwrap()),
        )
        .build()
        .unwrap();
    db.catalog()
        .create_graph_type(&ty, shape, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, Some(&ty), CreatePolicy::Strict)
        .unwrap();
    let session = db.session(&path).unwrap();
    session
        .execute("CREATE INDEX item_id ON :Item(id)")
        .unwrap();
    session
        .execute("CALL selene.create_text_index('Item', 'text')")
        .unwrap();
    (path, session)
}

fn seeds(session: &Session) {
    session.execute("INSERT (:Seed {k: 0})").unwrap();
    for shift in 0..11 {
        session
            .execute(&format!(
                "MATCH (s:Seed) INSERT (:Seed {{k: s.k + {}}})",
                1 << shift,
            ))
            .unwrap();
    }
}

fn count(session: &Session, label: &str) -> usize {
    session
        .execute(&format!("MATCH (n:{label}) RETURN n"))
        .unwrap()
        .row_count()
        .unwrap()
}

const INSERT: &str = "MATCH (s:Seed) INSERT (:Item {id: s.k, text: 'batch memory'})";

#[test]
fn last_batch_constraint_failure_publishes_none_and_reopen_preserves_indexes() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create(directory.path()).unwrap();
    let (path, session) = fixture(&db);
    seeds(&session);
    // The duplicate arrives after two full default-sized mutation batches.
    session.execute("INSERT (:Seed {k: 0})").unwrap();
    let before = db.catalog().snapshot();
    let error = session.execute(INSERT).unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "G2000");
    assert!(db.catalog().snapshot().shares_state_with(&before));
    assert_eq!(
        session.context().transaction_slot(),
        TransactionSlotState::RolledBack
    );
    assert_eq!(session.context().request_slot(), RequestSlotState::Vacant);
    assert_eq!(count(&session, "Item"), 0);
    // A fresh writer works; failed input neither leaks a reservation nor an index entry.
    session
        .execute("INSERT (:Item {id: 0, text: 'batch memory'})")
        .unwrap();
    drop(before);
    drop(session);
    drop(db);
    let db = Database::open(directory.path()).unwrap();
    let session = db.session(&path).unwrap();
    assert_eq!(count(&session, "Item"), 1);
    assert_eq!(
        session
            .execute("MATCH (n:Item) WHERE n.id = 0 RETURN n")
            .unwrap()
            .row_count(),
        Some(1)
    );
    assert_eq!(
        session
            .execute("CALL selene.text_search_nodes('Item', 'text', 'memory', 10) YIELD node_id RETURN node_id")
            .unwrap()
            .row_count(),
        Some(1)
    );
}

#[test]
fn explicit_batches_read_earlier_writes_and_failed_executed_call_clears_draft() {
    let db = Database::builder().build();
    let (path, session) = fixture(&db);
    seeds(&session);
    let observer = db.session(&path).unwrap();
    session.execute("START TRANSACTION").unwrap();
    let output = session.execute(INSERT).unwrap();
    assert_eq!(output.write_summary(), Some(WriteSummary::new(2048, None)));
    assert_eq!(count(&session, "Item"), 2048);
    assert_eq!(count(&observer, "Item"), 0);
    // This registered graph-tier call reaches execution; its negative k fails there.
    let error = session
        .execute("CALL selene.text_search_nodes('Item', 'text', 'memory', -1)")
        .unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "22G03");
    assert_eq!(
        session.context().transaction_slot(),
        TransactionSlotState::Failed
    );
    assert_eq!(session.context().request_slot(), RequestSlotState::Vacant);
    assert_eq!(count(&observer, "Item"), 0);
    assert_eq!(
        session
            .execute("COMMIT")
            .unwrap_err()
            .gqlstatus()
            .unwrap()
            .as_str(),
        "25N02"
    );
    assert_eq!(
        session.context().transaction_slot(),
        TransactionSlotState::RolledBack
    );
    session.execute("START TRANSACTION").unwrap();
    session.execute(INSERT).unwrap();
    session.execute("COMMIT").unwrap();
    assert_eq!(count(&observer, "Item"), 2048);
}

#[test]
fn read_only_rejects_direct_nested_and_catalog_writes_but_allows_session_controls() {
    let db = Database::builder().build();
    let (_path, session) = fixture(&db);
    for (source, status) in [
        ("INSERT (:Item {id: 9})", "25G03"),
        ("CREATE SCHEMA /forbidden", "25G03"),
        ("CALL selene.create_text_index('Seed', 'k')", "25G03"),
        // These nested forms are rejected even earlier by existing analysis
        // policy. Physical routing must not weaken or replace those diagnostics.
        (
            "RETURN 1 AS x NEXT CALL selene.create_text_index('Seed', 'k')",
            "25G02",
        ),
        (
            "CALL { CALL selene.create_text_index('Seed', 'k') }",
            "42N01",
        ),
    ] {
        session
            .start_transaction(TransactionAccessMode::ReadOnly)
            .unwrap();
        session.execute("SESSION SET VALUE $p = 1").unwrap();
        session.execute("SESSION RESET PARAMETERS").unwrap();
        let before = db.catalog().snapshot();
        let error = session.execute(source).unwrap_err();
        assert_eq!(
            error.gqlstatus().unwrap().as_str(),
            status,
            "{source}: {error}"
        );
        assert!(db.catalog().snapshot().shares_state_with(&before));
        session.execute("ROLLBACK").unwrap();
    }
}

#[test]
fn session_close_discards_all_batches_and_releases_request_state() {
    let db = Database::builder().build();
    let (path, session) = fixture(&db);
    seeds(&session);
    session.execute("START TRANSACTION").unwrap();
    session.execute(INSERT).unwrap();
    let before = db.catalog().snapshot();
    session.execute("SESSION CLOSE").unwrap();
    assert!(db.catalog().snapshot().shares_state_with(&before));
    assert_eq!(session.context().request_slot(), RequestSlotState::Vacant);
    assert_eq!(
        session.context().transaction_slot(),
        TransactionSlotState::Vacant
    );
    let writer = db.session(&path).unwrap();
    assert_eq!(count(&writer, "Item"), 0);
    writer.execute(INSERT).unwrap();
    assert_eq!(count(&writer, "Item"), 2048);
}
