//! External-style F04-PR09 consumer: only the facade, one durable database.

use selene_db::{
    CreatePolicy, Database, ExecutionOutcome, ObjectPath, SchemaPath, Session,
    TransactionSlotState, Value,
};

fn rows(session: &Session, source: &str) -> Vec<Vec<Value>> {
    let ExecutionOutcome::Rows { result, .. } = session
        .execute(source)
        .unwrap_or_else(|e| panic!("{source}: {e:?}"))
    else {
        panic!("expected rows")
    };
    result
        .rows()
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

fn verify(session: &Session) {
    assert_eq!(rows(session, "FOR k IN [2, 1, 2] CALL (k) { MATCH (m:Memory) WHERE m.k = k RETURN m.body AS body } YIELD body RETURN k, body ORDER BY k").len(), 3);
    assert_eq!(
        rows(
            session,
            "CALL selene.text_search_nodes('Memory', 'body', 'memory', 9) YIELD node_id RETURN node_id.k AS k ORDER BY k"
        ),
        [vec![Value::Int(1)], vec![Value::Int(2)]]
    );
    assert_eq!(
        rows(
            session,
            "CALL selene.vector_search_nodes('Memory', 'embedding', CAST([1, 0] AS VECTOR), 1, 'cosine') YIELD node_id RETURN node_id.k AS k"
        ),
        [vec![Value::Int(1)]]
    );
    let json = rows(
        session,
        "CALL selene.json_path_value_nodes('Memory', 'payload', CAST('[\"v\"]' AS JSON), 9) YIELD value RETURN value",
    );
    assert_eq!(json.len(), 1);
    assert!(matches!(json[0][0], Value::Json(_)));
    assert_eq!(
        rows(
            session,
            "MATCH (a:Memory {k: 1}) MATCH ALL SHORTEST p = (a)-[:LINK{1,2}]->(b) RETURN b.k AS k"
        ),
        [vec![Value::Int(2)]]
    );
    session
        .execute("CALL algo.projection_build('p', ['Memory'], ['LINK'], NULL)")
        .unwrap();
    assert_eq!(
        rows(session, "CALL algo.wcc_count('p') YIELD count"),
        [vec![Value::Uint(1)]]
    );
    session.execute("CALL algo.projection_drop('p')").unwrap();
    let ExecutionOutcome::Rows { result, .. } = session
        .execute("MATCH (m:Missing) RETURN m.k AS z, m.body AS a ORDER BY z")
        .unwrap()
    else {
        panic!("typed empty")
    };
    assert!(result.rows().is_empty());
    assert_eq!(result.descriptor().preferred_columns(), [0, 1]);
    assert_eq!(
        result
            .descriptor()
            .fields()
            .iter()
            .map(|field| field.name())
            .collect::<Vec<_>>(),
        [Some("z"), Some("a")]
    );
}

#[test]
fn one_facade_retains_named_graph_retrieval_failure_transaction_and_reopen_contracts() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create(directory.path()).unwrap();
    let schema = SchemaPath::regular("selene", "cutover").unwrap();
    let path = ObjectPath::regular("selene", "cutover", "memory").unwrap();
    db.catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let session = db.session(&path).unwrap();
    let observer = db.session(&path).unwrap();
    session.execute("START TRANSACTION").unwrap();
    session.execute("INSERT (:Memory {k: 99})").unwrap();
    assert!(session.execute("RETURN 1 / 0").is_err());
    assert_eq!(
        session.context().transaction_slot(),
        TransactionSlotState::Failed
    );
    assert!(rows(&observer, "MATCH (m:Memory) RETURN m").is_empty());
    session.execute("ROLLBACK").unwrap();
    session.execute("START TRANSACTION").unwrap();
    session.execute("INSERT (a:Memory {k: 1, body: 'memory', embedding: CAST([1, 0] AS VECTOR), payload: CAST('{\"v\":null}' AS JSON)})-[:LINK]->(:Memory {k: 2, body: 'memory', embedding: CAST([0, 1] AS VECTOR), payload: CAST('{}' AS JSON)})").unwrap();
    assert!(rows(&observer, "MATCH (m:Memory) RETURN m").is_empty());
    session.execute("COMMIT").unwrap();
    session
        .execute("CALL selene.create_text_index('Memory', 'body')")
        .unwrap();
    verify(&session);
    db.checkpoint().unwrap();
    drop(session);
    drop(observer);
    drop(db);
    let reopened = Database::open(directory.path()).unwrap();
    verify(&reopened.session(&path).unwrap());
}
