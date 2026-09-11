//! Every supported vector registration is query-ready on successful public open.
use selene_db::*;

fn result(s: &Session, query: &str) -> Vec<Vec<Value>> {
    let ExecutionOutcome::Rows { result, .. } = s.execute(query).unwrap() else {
        panic!("rows");
    };
    result.rows().iter().map(|r| r.values().to_vec()).collect()
}

#[test]
fn eager_reconstruction_limit_returns_no_database_and_preserves_every_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::create(dir.path()).unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "memory").unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    for graph in 0..17 {
        let path = ObjectPath::regular("selene", "memory", format!("graph_{graph}")).unwrap();
        db.catalog()
            .create_graph(&path, None, CreatePolicy::Strict)
            .unwrap();
        let s = db.session(&path).unwrap();
        let input = (0..32)
            .map(|i| format!("(:Doc {{v: CAST([1, {}] AS VECTOR)}})", i as f64 / 32.0))
            .collect::<Vec<_>>()
            .join(",");
        s.execute(&format!("INSERT {input}")).unwrap();
        // Valid native configuration; aggregate conservative reconstruction charge
        // exceeds the all-index budget. It must not silently defer these indexes.
        s.execute(
            "CALL selene.create_vector_index('Doc', 'v', 2, 'hnsw', NULL, 'cosine', 512, 512)",
        )
        .unwrap();
    }
    db.checkpoint().unwrap();
    drop(db);
    let artifacts = || {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = artifacts();
    for _ in 0..2 {
        let error = Database::open(dir.path())
            .err()
            .expect("must not return a Database");
        assert_eq!(error.kind, StorageErrorKind::ResourceLimit);
        assert_eq!(error.phase, StoragePhase::Rebuild);
        assert_eq!(artifacts(), before);
        // The second attempt must reach the same error, not a leaked writer lock.
    }
}

#[test]
fn all_vector_families_rebuild_with_exact_native_search_guards_and_descriptor_identity() {
    for (kind, metric, config) in [
        ("flat", "cosine", "NULL, NULL, NULL"),
        ("hnsw", "cosine", "8, 32, NULL"),
        ("hnsw", "squared_euclidean", "8, 32, NULL"),
        ("hnsw", "negative_inner_product", "8, 32, NULL"),
        ("ivf", "cosine", "NULL, NULL, 2"),
        ("ivf", "squared_euclidean", "NULL, NULL, 2"),
        ("ivf", "negative_inner_product", "NULL, NULL, 2"),
        ("turbo_quant", "cosine", "NULL, NULL, NULL"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path()).unwrap();
        let path = ObjectPath::regular("selene", "memory", "data").unwrap();
        db.catalog()
            .create_schema(
                &SchemaPath::regular("selene", "memory").unwrap(),
                CreatePolicy::Strict,
            )
            .unwrap();
        db.catalog()
            .create_graph(&path, None, CreatePolicy::Strict)
            .unwrap();
        let s = db.session(&path).unwrap();
        s.execute("INSERT (:Doc {v: CAST([1, 0] AS VECTOR)}), (:Doc {v: CAST([0, 1] AS VECTOR)})")
            .unwrap();
        let create_metric = if kind == "flat" {
            "NULL".into()
        } else {
            format!("'{metric}'")
        };
        s.execute(&format!("CALL selene.create_vector_index('Doc', 'v', 2, '{kind}', 'vectors', {create_metric}, {config})")).unwrap();
        let declarations = db.catalog().snapshot().declarations(&path).unwrap();
        let exact = format!(
            "CALL selene.vector_search_nodes('Doc', 'v', CAST([1, 0] AS VECTOR), 2, '{metric}') YIELD node_id, distance RETURN distance"
        );
        let query = if kind == "flat" {
            exact.clone()
        } else {
            format!(
                "CALL selene.vector_search_nodes_ann('Doc', 'v', CAST([1, 0] AS VECTOR), 2, '{metric}', 32) YIELD node_id, distance RETURN distance"
            )
        };
        let expected = result(&s, &exact);
        assert_eq!(expected.len(), 2);
        assert_eq!(result(&s, &query), expected, "live {kind}/{metric}");
        db.checkpoint().unwrap();
        drop(s);
        drop(db);
        let db = Database::open(dir.path()).unwrap();
        assert_eq!(db.recovery_info().unwrap().rebuilt_indexes, 1);
        assert_eq!(
            db.catalog().snapshot().declarations(&path).unwrap(),
            declarations
        );
        let s = db.session(&path).unwrap();
        assert_eq!(result(&s, &query), expected, "reopened {kind}/{metric}");
        assert_eq!(result(&s, &exact), expected);
    }
}
