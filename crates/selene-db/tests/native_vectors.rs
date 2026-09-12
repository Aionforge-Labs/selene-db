//! F04-PR07: graph-scoped memory retrieval through the stable native boundary.

use selene_db::*;

fn graph(db: &Database, name: &str) -> (ObjectPath, Session) {
    let path = ObjectPath::regular("selene", "vectors", name).unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "vectors").unwrap(),
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
        panic!("expected rows: {source}")
    };
    result
        .rows()
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

fn search(procedure: &str, k: usize, metric: &str) -> String {
    format!(
        "CALL selene.{procedure}('Memory', 'embedding', CAST([1, 0] AS VECTOR), {k}, '{metric}') YIELD node_id, distance RETURN node_id.key AS key, distance"
    )
}

fn seed(session: &Session) {
    session.execute("INSERT (:Memory {key: 1, namespace: 'agent', embedding: CAST([1, 0] AS VECTOR)}), (:Memory {key: 2, namespace: 'other', embedding: CAST([0, 1] AS VECTOR)}), (:Memory {key: 3, namespace: 'agent', embedding: CAST([0, -1] AS VECTOR)}), (:Memory {key: 4, namespace: 'agent', embedding: CAST([-1, 0] AS VECTOR)})").unwrap();
}

fn create_index(session: &Session, kind: &str, metric: &str) {
    let metric = if kind == "flat" {
        "NULL".to_owned()
    } else {
        format!("'{metric}'")
    };
    session.execute(&format!("CALL selene.create_vector_index('Memory', 'embedding', 2, '{kind}', 'memory_vectors', {metric})")).unwrap();
}

#[test]
fn exact_metrics_ties_limits_and_native_value_types_have_independent_answers() {
    let db = Database::builder().build();
    let (_, session) = graph(&db, "numeric");
    seed(&session);
    session
        .execute("INSERT (:Memory {key: 5}), (:Memory {key: 6, embedding: 'not a vector'})")
        .unwrap();
    // Query (1,0), corpus (1,0),(0,1),(0,-1),(-1,0). Expected
    // squared norms, dot products and unit-vector cosines are hand-derived,
    // not calculated using the engine or its SIMD/reference metric kernels.
    for (metric, distances) in [
        ("squared_euclidean", [0.0, 2.0, 2.0, 4.0]),
        ("cosine", [0.0, 1.0, 1.0, 2.0]),
        ("negative_inner_product", [-1.0, 0.0, 0.0, 1.0]),
    ] {
        for k in [0, 1, 3, 99] {
            let expected: Vec<_> = distances
                .iter()
                .enumerate()
                .take(k)
                .map(|(i, d)| vec![Value::Int(i as i64 + 1), Value::Float(*d)])
                .collect();
            assert_eq!(
                rows(&session, &search("vector_search_nodes", k, metric)),
                expected
            );
        }
        let batch = format!(
            "CALL selene.vector_search_nodes_batch('Memory', 'embedding', [CAST([1, 0] AS VECTOR), CAST([1, 0] AS VECTOR)], 99, '{metric}') YIELD query_index, node_id, distance RETURN query_index, node_id.key AS key, distance"
        );
        let expected: Vec<_> = (0..2)
            .flat_map(|q| {
                distances.iter().enumerate().map(move |(i, d)| {
                    vec![Value::Uint(q), Value::Int(i as i64 + 1), Value::Float(*d)]
                })
            })
            .collect();
        assert_eq!(rows(&session, &batch), expected);
    }
    let native = rows(
        &session,
        "CALL selene.vector_search_nodes('Memory', 'embedding', CAST([1, 0] AS VECTOR), 1) YIELD node_id, distance",
    );
    assert!(matches!(
        native[0].as_slice(),
        [Value::NodeRef(_), Value::Float(0.0)]
    ));
}

#[test]
fn vector_argument_errors_are_explicit_and_do_not_publish_staged_writes() {
    let db = Database::builder().build();
    let (path, session) = graph(&db, "invalid");
    seed(&session);
    let observer = db.session(&path).unwrap();
    for source in [
        "CALL selene.vector_search_nodes('Memory', 'embedding', CAST([1] AS VECTOR), 1)",
        "CALL selene.vector_search_nodes('Memory', 'embedding', CAST([1, 0] AS VECTOR), 1, 'manhattan')",
        "CALL selene.vector_search_nodes('Memory', 'embedding', CAST([1, 0] AS VECTOR), -1)",
        "CALL selene.vector_search_nodes_ann('Memory', 'embedding', CAST([1, 0] AS VECTOR), 1)",
    ] {
        session.execute("START TRANSACTION").unwrap();
        session.execute("INSERT (:Unpublished)").unwrap();
        let error = session.execute(source).unwrap_err();
        assert_eq!(error.gqlstatus().unwrap().as_str(), "22G03", "{source}");
        assert_eq!(
            session.context().transaction_slot(),
            TransactionSlotState::Failed
        );
        assert!(session.execute("COMMIT").is_err());
        assert!(rows(&observer, "MATCH (n:Unpublished) RETURN n").is_empty());
    }
    for source in [
        "RETURN CAST([] AS VECTOR)",
        "RETURN CAST([1e100] AS VECTOR)",
    ] {
        let error = session.execute(source).unwrap_err();
        assert!(
            error.gqlstatus().unwrap().as_str().starts_with("22"),
            "{error:?}"
        );
    }
}

#[test]
fn all_ann_modes_keep_catalog_identity_and_live_values_across_rollback_and_restart() {
    for (kind, metric) in [
        ("hnsw", "cosine"),
        ("ivf", "cosine"),
        ("turbo_quant", "cosine"),
        ("hnsw", "squared_euclidean"),
        ("ivf", "squared_euclidean"),
        ("hnsw", "negative_inner_product"),
        ("ivf", "negative_inner_product"),
    ] {
        for checkpoint in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let db = Database::create(directory.path()).unwrap();
            let (path, session) = graph(&db, "memory");
            seed(&session);
            create_index(&session, kind, metric);
            let declarations = db.catalog().snapshot().declarations(&path).unwrap();
            assert_eq!(declarations.len(), 1);
            let exact = search("vector_search_nodes", 99, metric);
            let ann = search("vector_search_nodes_ann", 99, metric);
            let original = rows(&session, &exact);
            assert_eq!(rows(&session, &ann), original, "{kind}/{metric}");
            session.execute("START TRANSACTION").unwrap();
            session
                .execute("MATCH (n:Memory {key: 1}) DELETE n")
                .unwrap();
            assert_eq!(rows(&session, &ann).len(), 3);
            session.execute("ROLLBACK").unwrap();
            assert_eq!(rows(&session, &ann), original);
            session
                .execute("MATCH (n:Memory {key: 1}) DELETE n")
                .unwrap();
            session
                .execute("MATCH (n:Memory {key: 2}) SET n.embedding = CAST([1, 0] AS VECTOR)")
                .unwrap();
            session.execute("INSERT (:Memory {key: 7, namespace: 'agent', embedding: CAST([-1, 0] AS VECTOR)})").unwrap();
            let expected = rows(&session, &exact);
            assert_eq!(expected.len(), 4);
            assert_eq!(expected[0][0], Value::Int(2));
            assert!(expected.iter().all(|row| row[0] != Value::Int(1)));
            assert_eq!(rows(&session, &ann), expected);
            if checkpoint {
                db.checkpoint().unwrap();
            }
            drop(session);
            drop(db);
            let db = Database::open(directory.path()).unwrap();
            assert_eq!(
                db.catalog().snapshot().declarations(&path).unwrap(),
                declarations
            );
            let session = db.session(&path).unwrap();
            assert_eq!(
                rows(&session, &ann),
                expected,
                "reopen {kind}/{metric}, checkpoint={checkpoint}"
            );
            assert_eq!(rows(&session, &exact), expected);
            let batch = format!(
                "CALL selene.vector_search_nodes_ann_batch('Memory', 'embedding', [CAST([1, 0] AS VECTOR)], 99, '{metric}') YIELD query_index, node_id, distance RETURN node_id.key AS key, distance"
            );
            assert_eq!(rows(&session, &batch), expected);
        }
    }
}

#[test]
fn filtered_ann_admits_before_top_k_and_graph_replacement_cannot_reuse_an_index() {
    for kind in ["hnsw", "ivf", "turbo_quant"] {
        let db = Database::builder().build();
        let (path, session) = graph(&db, "selected");
        let (_, other) = graph(&db, "other");
        seed(&session);
        other
            .execute("INSERT (:Memory {key: 99, embedding: CAST([1, 0] AS VECTOR)})")
            .unwrap();
        create_index(&session, kind, "cosine");
        create_index(&other, kind, "cosine");
        session
            .execute("CALL selene.create_index('Memory', 'namespace', 'string')")
            .unwrap();
        // The global nearest hit is not in this allowlist. Admission is not
        // implemented as global top-1 followed by filtering (which returns 0).
        let filtered = "CALL selene.vector_search_nodes_ann('Memory', 'embedding', CAST([1, 0] AS VECTOR), 1, 'cosine', 64, 'namespace', ['other']) YIELD node_id, distance RETURN node_id.key AS key, distance";
        assert_eq!(
            rows(&session, filtered),
            vec![vec![Value::Int(2), Value::Float(1.0)]]
        );
        assert_eq!(
            rows(
                &session,
                &format!(
                    "USE /vectors/other {}",
                    search("vector_search_nodes_ann", 99, "cosine")
                )
            ),
            vec![vec![Value::Int(99), Value::Float(0.0)]]
        );
        let old = db.catalog().snapshot().declarations(&path).unwrap();
        // OR REPLACE retains the existing RESTRICT contract: empty primary
        // data first, leaving the old derived index and its declaration behind.
        session.execute("MATCH (n:Memory) DELETE n").unwrap();
        db.catalog()
            .create_graph(&path, None, CreatePolicy::OrReplace)
            .unwrap();
        drop(session);
        let session = db.session(&path).unwrap();
        assert!(
            db.catalog()
                .snapshot()
                .declarations(&path)
                .unwrap()
                .is_empty()
        );
        assert!(!old.is_empty());
        assert!(rows(&session, &search("vector_search_nodes", 99, "cosine")).is_empty());
        assert_eq!(
            session
                .execute(&search("vector_search_nodes_ann", 99, "cosine"))
                .unwrap_err()
                .gqlstatus()
                .unwrap()
                .as_str(),
            "22G03"
        );
        create_index(&session, kind, "cosine");
        assert!(rows(&session, &search("vector_search_nodes_ann", 99, "cosine")).is_empty());
        assert_ne!(
            db.catalog().snapshot().declarations(&path).unwrap()[0].id,
            old[0].id
        );
    }
}
