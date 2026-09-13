// Runnable consumer examples: cargo nextest run -p selene-db --test release_examples --locked
// tempfile supplies only test-directory isolation, not an engine dependency.
use selene_db::{
    CreatePolicy, Database, ErrorKind, ExecutionOutcome, GqlStatus, GraphTypeDefinition,
    NodeTypeDefinition, ObjectPath, PathSegment, PropertyDefinition, RegularResult, Request,
    SchemaPath, Session, Type, Value,
};

fn graph(database: &Database) -> selene_db::Result<(ObjectPath, Session)> {
    let schema = SchemaPath::regular("selene", "memory")?;
    let path = ObjectPath::regular("selene", "memory", "episodes")?;
    database
        .catalog()
        .create_schema(&schema, CreatePolicy::Strict)?;
    database
        .catalog()
        .create_graph(&path, None, CreatePolicy::Strict)?;
    let session = database.session(&path)?;
    Ok((path, session))
}

fn rows(session: &Session, source: &str) -> RegularResult {
    let ExecutionOutcome::Rows { result, .. } = session.execute(source).unwrap() else {
        panic!("expected regular rows: {source}");
    };
    result
}

#[test]
fn mixed_graph_and_path_values_keep_edge_identity() -> selene_db::Result<()> {
    let database = Database::builder().build();
    let (_, session) = graph(&database)?;
    session.execute("INSERT (a:A {key: 1})-[:LINK]->(b:B {key: 2}), (a)~[:LINK]~(b)")?;
    let directed = rows(&session, "MATCH (a:A)-[e:LINK]->(b:B) RETURN e");
    let undirected = rows(&session, "MATCH (a:A)~[e:LINK]~(b:B) RETURN e");
    assert_eq!(directed.row_count(), 1);
    assert_eq!(undirected.row_count(), 1);
    assert_ne!(directed.rows()[0].values(), undirected.rows()[0].values());
    let paths = rows(&session, "MATCH p = (a:A)-[:LINK]->(b:B) RETURN p");
    let Value::Path(path) = &paths.rows()[0].values()[0] else {
        panic!("owned path value");
    };
    assert_eq!(path.segments().len(), 1);
    assert_eq!(
        Value::EdgeRef(path.segments()[0].edge()),
        directed.rows()[0].values()[0]
    );
    Ok(())
}

#[test]
fn native_vector_text_and_json_retrieval() -> selene_db::Result<()> {
    let database = Database::builder().build();
    let (_, session) = graph(&database)?;
    session.execute(
        "INSERT (:Memory {key: 1, body: 'graph memory', embedding: CAST([1, 0] AS VECTOR), payload: CAST('{\"kind\":\"episode\"}' AS JSON)}),
                (:Memory {key: 2, body: 'unrelated', embedding: CAST([0, 1] AS VECTOR), payload: CAST('{}' AS JSON)})",
    )?;
    session.execute("CALL selene.create_text_index('Memory', 'body', 'memory_text')")?;
    let vector = rows(
        &session,
        "CALL selene.vector_search_nodes('Memory', 'embedding', CAST([1, 0] AS VECTOR), 1, 'cosine') YIELD node_id, distance RETURN node_id.key AS key, distance",
    );
    assert_eq!(
        vector.rows()[0].values(),
        &[Value::Int(1), Value::Float(0.0)]
    );
    let text = rows(
        &session,
        "CALL selene.text_search_nodes('Memory', 'body', 'memory', 1) YIELD node_id RETURN node_id.key AS key",
    );
    assert_eq!(text.rows()[0].values(), &[Value::Int(1)]);
    let json = rows(
        &session,
        "CALL selene.json_contains_nodes('Memory', 'payload', CAST('{\"kind\":\"episode\"}' AS JSON), 10) YIELD node_id RETURN node_id.key AS key",
    );
    assert_eq!(json.row_count(), 1);
    assert_eq!(json.rows()[0].values(), &[Value::Int(1)]);
    Ok(())
}

#[test]
fn rust_schema_constraints_reject_without_partial_publication() -> selene_db::Result<()> {
    let database = Database::builder().build();
    let (path, _) = graph(&database)?;
    let shape_path = ObjectPath::regular("selene", "memory", "shape")?;
    let name = PathSegment::regular("Item")?;
    let shape = GraphTypeDefinition::builder()
        .with_node_type(
            NodeTypeDefinition::new(name.clone(), vec![name])?.with_property(
                PropertyDefinition::new(PathSegment::regular("key")?, Type::INT64)?.unique(),
            ),
        )
        .build()?;
    database
        .catalog()
        .create_graph_type(&shape_path, shape, CreatePolicy::Strict)?;
    database
        .catalog()
        .create_graph(&path, Some(&shape_path), CreatePolicy::OrReplace)?;
    let session = database.session(&path)?;
    session.execute("INSERT (:Item {key: 1})")?;
    let before = database.catalog().snapshot();
    let error = session
        .execute("INSERT (:Item {key: 2}), (:Item {key: 1})")
        .unwrap_err();
    assert_eq!(error.gqlstatus(), Some(GqlStatus::GRAPH_TYPE_VIOLATION));
    assert!(database.catalog().snapshot().shares_state_with(&before));
    assert_eq!(
        rows(&session, "MATCH (n:Item) RETURN n.key").rows()[0].values(),
        &[Value::Int(1)]
    );
    assert_eq!(rows(&session, "MATCH (n:Item) RETURN n").row_count(), 1);
    Ok(())
}

#[test]
fn negative_requests_preserve_catalog_data_and_session_state() -> selene_db::Result<()> {
    let database = Database::builder().build();
    let (_, session) = graph(&database)?;
    session.execute("INSERT (:Kept {key: 1})")?;
    session.execute("SESSION SET VALUE $answer INTEGER = 42")?;
    for (source, status) in [
        ("NOT A GQL STATEMENT", "42001"),
        ("CREATE GRAPH rejected AS COPY OF episodes", "42N01"),
        ("SESSION SET VALUE $answer STRING = 99", "22G03"),
        ("INSERT (:Discarded) RETURN 1 / 0", "22012"),
    ] {
        let before = database.catalog().snapshot();
        let outcome = session.execute_request(Request::new(source));
        assert!(outcome.error().is_some(), "{source}");
        assert_eq!(
            outcome.diagnostics().primary().status().as_str(),
            status,
            "{source}"
        );
        assert!(
            database.catalog().snapshot().shares_state_with(&before),
            "{source}"
        );
        assert_eq!(rows(&session, "MATCH (n) RETURN n").row_count(), 1);
        assert_eq!(
            rows(&session, "RETURN $answer").rows()[0].values(),
            &[Value::Int(42)]
        );
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn transaction_checkpoint_reopen_and_handle_provenance() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let database = Database::create(directory.path())?;
    let (path, session) = graph(&database)?;
    session.execute("START TRANSACTION")?;
    session.execute("INSERT (:Kept {key: 1})")?;
    assert_eq!(
        rows(&database.session(&path)?, "MATCH (n) RETURN n").row_count(),
        0
    );
    session.execute("COMMIT")?;
    session.execute("START TRANSACTION")?;
    session.execute("INSERT (:Discarded)")?;
    session.execute("ROLLBACK")?;
    let reference = database.graph_reference(&path)?;
    database.checkpoint()?;
    session.execute("INSERT (:Kept {key: 2})")?;
    drop(session); // Sessions retain the database and its writer lock.
    drop(database);
    let reopened = Database::open(directory.path())?;
    let fresh = reopened.graph_reference(&path)?;
    assert_eq!(reference.graph_id(), fresh.graph_id());
    assert_ne!(reference.database_id(), fresh.database_id());
    assert_eq!(
        reopened
            .resolve_graph_reference(reference)
            .unwrap_err()
            .kind(),
        ErrorKind::RuntimeInvalidReference
    );
    assert_eq!(
        rows(&reopened.session(&path)?, "MATCH (n:Kept) RETURN n").row_count(),
        2
    );
    Ok(())
}
