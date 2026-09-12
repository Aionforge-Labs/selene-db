//! F04-PR08 memory documents through catalog-resolved typed native calls.

use selene_db::*;

fn graph(db: &Database, name: &str) -> (ObjectPath, Session) {
    let path = ObjectPath::regular("selene", "retrieval", name).unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "retrieval").unwrap(),
            CreatePolicy::IfNotExists,
        )
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let session = db.session(&path).unwrap();
    (path, session)
}

fn rows(session: &Session, source: &str) -> Vec<Vec<Value>> {
    let ExecutionOutcome::Rows { result, .. } = session
        .execute(source)
        .unwrap_or_else(|e| panic!("{source}: {e:?}"))
    else {
        panic!("expected rows")
    };
    result.rows().iter().map(|r| r.values().to_vec()).collect()
}

fn state(label: &str, lifecycle: DeclarationState) -> DeclarationDefinition {
    DeclarationDefinition::Native(NativeDeclaration {
        metadata: DeclarationMetadata::new(lifecycle),
        binding: NativeBinding::CandidateState(NativeCandidateState {
            required_label: Some(label.into()),
            require_outgoing: vec![],
            require_incoming: vec![],
            exclude_outgoing: vec!["SUPERSEDED_BY".into()],
            exclude_incoming: vec![],
        }),
    })
}

fn seed(session: &Session) {
    session.execute("INSERT (:Memory {key: 1, scope: 'agent', body: 'memory', payload: CAST('{\"v\":null}' AS JSON)}), (:Memory {key: 2, scope: 'other', body: 'memory', payload: CAST('{}' AS JSON)}), (:Memory {key: 3, scope: 'agent', body: 'graph', payload: CAST('{\"v\":1}' AS JSON)}), (:Memory {key: 4, scope: 'agent', body: '', payload: CAST('{\"v\":\"1\"}' AS JSON)}), (:Memory {key: 5, payload: CAST('{\"v\":true}' AS JSON)})").unwrap();
}

const SEARCH: &str = "CALL selene.text_search_nodes('Memory', 'body', 'memory', 99) YIELD node_id, score RETURN node_id.key AS key, score";
const STATE: &str = "CALL selene.text_score_candidate_state('Memory', 'body', 'memory', 'current', 99) YIELD node_id, score RETURN node_id.key AS key, score";
const CREATE: &str = "CALL selene.create_text_index('Memory', 'body', 'memory_text')";
const DROP: &str = "CALL selene.drop_text_index('Memory', 'body')";

#[test]
fn text_global_statistics_ties_and_filters_match_scan_and_rebuild() {
    let db = Database::builder().build();
    let (path, s) = graph(&db, "statistics");
    seed(&s);
    s.execute("CALL selene.create_index('Memory', 'scope', 'string')")
        .unwrap();
    let exact = rows(&s, SEARCH);
    assert_eq!(exact.len(), 2);
    for (row, key) in exact.iter().zip([1, 2]) {
        assert_eq!(row[0], Value::Int(key));
        let Value::Float(score) = row[1] else {
            panic!("typed score")
        };
        // Three nonempty single-token documents; df(memory)=2. tf/length
        // normalization is one, so BM25 = ln(1 + 1.5/2.5), independently derived.
        assert!((score - 0.470_003_629_245_735_63).abs() < 1e-14);
    }
    let filtered = "CALL selene.text_search_nodes('Memory', 'body', 'memory', 99, 'scope', ['agent']) YIELD node_id, score RETURN node_id.key AS key, score";
    assert_eq!(rows(&s, filtered), exact[..1]);
    s.execute(CREATE).unwrap();
    assert_eq!(rows(&s, SEARCH), exact);
    assert_eq!(rows(&s, filtered), exact[..1]);
    let before = db.catalog().snapshot();
    let score = "MATCH (m:Memory) WHERE m.key = 1 CALL selene.text_score_nodes('Memory', 'body', 'memory', [m, m], 99) YIELD node_id, score RETURN node_id.key AS key, score";
    assert_eq!(rows(&s, score), exact[..1]);
    assert!(before.shares_state_with(&db.catalog().snapshot()));
    assert!(before.declarations(&path).unwrap().iter().any(|d|
        matches!(&d.definition, DeclarationDefinition::Index(i) if i.configuration == IndexConfiguration::Text)));
    for mutation in [
        "MATCH (m:Memory) WHERE m.key = 1 SET m.body = 'graph memory memory'",
        "MATCH (m:Memory) WHERE m.key = 2 REMOVE m.body",
        "MATCH (m:Memory) WHERE m.key = 3 DELETE m",
        "INSERT (:Memory {key: 6, body: 'MEMORY! memory'})",
    ] {
        s.execute(mutation).unwrap();
        let maintained = rows(&s, SEARCH);
        s.execute(DROP).unwrap();
        assert_eq!(rows(&s, SEARCH), maintained, "scan after {mutation}");
        s.execute(CREATE).unwrap();
        assert_eq!(rows(&s, SEARCH), maintained, "rebuild after {mutation}");
    }
    let before = rows(&s, SEARCH);
    s.execute("START TRANSACTION").unwrap();
    s.execute("MATCH (m:Memory) SET m.body = 'nothing'")
        .unwrap();
    assert!(rows(&s, SEARCH).is_empty());
    s.execute("ROLLBACK").unwrap();
    assert_eq!(rows(&s, SEARCH), before);
}

#[test]
fn json_missing_null_scalar_types_paths_and_candidate_scans_remain_distinct() {
    let db = Database::builder().build();
    let (_, s) = graph(&db, "json");
    seed(&s);
    let values = rows(
        &s,
        "CALL selene.json_path_value_nodes('Memory', 'payload', CAST('[\"v\"]' AS JSON), 99) YIELD node_id, value RETURN node_id.key AS key, value",
    );
    assert_eq!(values.len(), 4);
    assert_eq!(
        values.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        [Value::Int(1), Value::Int(3), Value::Int(4), Value::Int(5)]
    );
    assert!(values.iter().all(|r| matches!(r[1], Value::Json(_))));
    for (literal, key) in [("null", 1), ("1", 3), ("\"1\"", 4), ("true", 5)] {
        let source = format!(
            "CALL selene.json_path_contains_nodes('Memory', 'payload', CAST('[\"v\"]' AS JSON), CAST('{literal}' AS JSON), 99) YIELD node_id RETURN node_id.key AS key"
        );
        assert_eq!(rows(&s, &source), [vec![Value::Int(key)]]);
        let candidate = format!(
            "MATCH (m:Memory) CALL selene.json_path_contains_candidate_nodes('Memory', 'payload', CAST('[\"v\"]' AS JSON), CAST('{literal}' AS JSON), [m, m], 99) YIELD node_id RETURN node_id.key AS key"
        );
        assert_eq!(rows(&s, &candidate), [vec![Value::Int(key)]]);
    }
    for path in ["{}", "[true]", "[1.5]", "[]"] {
        let source = format!(
            "CALL selene.json_path_exists_nodes('Memory', 'payload', CAST('{path}' AS JSON), 99)"
        );
        assert_eq!(
            s.execute(&source)
                .unwrap_err()
                .gqlstatus()
                .unwrap()
                .as_str(),
            "22G03"
        );
    }
    assert_eq!(
        rows(
            &s,
            "MATCH (m:Memory) WHERE m.key = 2 CALL selene.json_path_exists_candidate_nodes('Memory', 'payload', CAST('[\"v\"]' AS JSON), [m], 99) YIELD node_id RETURN node_id"
        ),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn catalog_candidate_lifecycle_is_graph_scoped_and_transaction_local() {
    let db = Database::builder().build();
    let (path, s) = graph(&db, "states");
    let (_, other) = graph(&db, "other");
    seed(&s);
    seed(&other);
    s.execute(CREATE).unwrap();
    other.execute(CREATE).unwrap();
    let name = PathSegment::delimited("current").unwrap();
    db.catalog()
        .declare(
            &path,
            &name,
            state("Memory", DeclarationState::Inactive),
            CreatePolicy::Strict,
        )
        .unwrap();
    assert_eq!(
        s.execute(STATE).unwrap_err().gqlstatus().unwrap().as_str(),
        "22G03"
    );
    db.catalog()
        .declare(
            &path,
            &name,
            state("Memory", DeclarationState::Ready),
            CreatePolicy::OrReplace,
        )
        .unwrap();
    assert_eq!(rows(&s, STATE), rows(&s, SEARCH));
    assert!(other.execute(STATE).is_err());
    s.execute("START TRANSACTION").unwrap();
    s.execute("MATCH (a:Memory), (b:Memory) WHERE a.key = 1 AND b.key = 2 INSERT (a)-[:SUPERSEDED_BY]->(b)").unwrap();
    assert_eq!(rows(&s, STATE).len(), 1);
    assert_eq!(rows(&db.session(&path).unwrap(), STATE).len(), 2);
    s.execute("ROLLBACK").unwrap();
    assert_eq!(rows(&s, STATE).len(), 2);
    s.execute("MATCH (a:Memory), (b:Memory) WHERE a.key = 1 AND b.key = 2 INSERT (a)-[:SUPERSEDED_BY]->(b)").unwrap();
    assert_eq!(rows(&s, STATE)[0][0], Value::Int(2));
    s.execute("MATCH (:Memory)-[e:SUPERSEDED_BY]->(:Memory) DELETE e")
        .unwrap();
    assert_eq!(rows(&s, STATE).len(), 2);
    db.catalog()
        .declare(
            &path,
            &name,
            state("Absent", DeclarationState::Ready),
            CreatePolicy::OrReplace,
        )
        .unwrap();
    assert!(rows(&s, STATE).is_empty());
    db.catalog()
        .drop_declaration(&path, &name, DropPolicy::Strict)
        .unwrap();
    assert!(s.execute(STATE).is_err());
    let before = db.catalog().snapshot();
    assert!(
        db.catalog()
            .declare(
                &path,
                &name,
                state("", DeclarationState::Ready),
                CreatePolicy::Strict
            )
            .is_err()
    );
    assert!(before.shares_state_with(&db.catalog().snapshot()));
}

#[test]
fn declarations_and_authoritative_values_rebuild_after_wal_and_checkpoint_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::create(dir.path()).unwrap();
    let (path, s) = graph(&db, "durable");
    seed(&s);
    s.execute(CREATE).unwrap();
    db.catalog()
        .declare(
            &path,
            &PathSegment::delimited("current").unwrap(),
            state("Memory", DeclarationState::Ready),
            CreatePolicy::Strict,
        )
        .unwrap();
    let expected = rows(&s, STATE);
    drop(s);
    drop(db);
    for checkpoint in [false, true] {
        let db = Database::open(dir.path()).unwrap();
        let s = db.session(&path).unwrap();
        assert_eq!(rows(&s, SEARCH), expected);
        assert_eq!(rows(&s, STATE), expected);
        assert_eq!(rows(&s, "CALL selene.json_path_exists_nodes('Memory', 'payload', CAST('[\"v\"]' AS JSON), 99) YIELD node_id RETURN node_id").len(), 4);
        assert_eq!(Database::verify(dir.path()).unwrap().nodes, 5);
        if !checkpoint {
            db.checkpoint().unwrap();
        }
    }
}

#[test]
fn graph_replacement_drops_candidate_and_text_registrations() {
    let db = Database::builder().build();
    let (path, s) = graph(&db, "replacement");
    seed(&s);
    s.execute(CREATE).unwrap();
    db.catalog()
        .declare(
            &path,
            &PathSegment::delimited("current").unwrap(),
            state("Memory", DeclarationState::Ready),
            CreatePolicy::Strict,
        )
        .unwrap();
    let before = db.catalog().snapshot();
    s.execute("MATCH (m:Memory) DETACH DELETE m").unwrap();
    assert!(rows(&s, STATE).is_empty());
    db.catalog()
        .create_graph(&path, None, CreatePolicy::OrReplace)
        .unwrap();
    let new = db.session(&path).unwrap();
    assert!(
        db.catalog()
            .snapshot()
            .declarations(&path)
            .unwrap()
            .is_empty()
    );
    assert_eq!(before.declarations(&path).unwrap().len(), 2);
    assert!(new.execute(STATE).is_err());
    assert!(rows(&new, SEARCH).is_empty());
}
