//! Independent indexed/scan fixtures for F05-PR06 and issue #1097.
use selene_db::*;

fn graph(db: &Database, name: &str) -> (ObjectPath, Session) {
    let path = ObjectPath::regular("selene", "expressions", name).unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "expressions").unwrap(),
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
        panic!("rows")
    };
    result.rows().iter().map(|r| r.values().to_vec()).collect()
}

fn index(db: &Database, path: &ObjectPath, name: &str, source: &str, kind: ScalarIndexKind) {
    db.catalog()
        .create_expression_index(
            path,
            &PathSegment::regular(name).unwrap(),
            "Doc",
            source,
            kind,
        )
        .unwrap();
}

const KEY: &str = "json_get_path_scalar(n.body, 'format', 'kind')";
const QUERY: &str = "MATCH (n:Doc) WHERE json_get_path_scalar(n.body, 'format', 'kind') = 'jsonl' RETURN n.id ORDER BY n.id";
const SEED: &str = r#"INSERT (:Doc {id: 1, body: CAST('{"format":{"kind":"jsonl"}}' AS JSON)}), (:Doc {id: 2, body: CAST('{"format":{"kind":"JSONL"}}' AS JSON)}), (:Doc {id: 3, body: CAST('{"format":{"kind":null}}' AS JSON)}), (:Doc {id: 4, body: CAST('{}' AS JSON)}), (:Doc {id: 5})"#;

#[test]
fn decoded_keys_negative_indexes_unicode_and_document_selectors_agree() {
    let db = Database::builder().build();
    let (path, indexed) = graph(&db, "indexed");
    let (_, scan) = graph(&db, "scan");
    for s in [&indexed, &scan] {
        s.execute(r#"INSERT (:Doc {id: 1, body: CAST('{"a.b":["other","É"]}' AS JSON), name: 'É'}), (:Doc {id: 2, body: CAST('{"a.b":["é"]}' AS JSON), name: 'é'}), (:Doc {id: 3, body: CAST('{"a.b":[]}' AS JSON), name: 'é'}), (:Doc {id: 4, body: CAST('{"a.b":null}' AS JSON), name: 'other'})"#).unwrap();
    }
    index(
        &db,
        &path,
        "last",
        "json_get_path_scalar(n.body, 'a.b', -1)",
        ScalarIndexKind::String,
    );
    index(&db, &path, "fold", "lower(n.name)", ScalarIndexKind::String);
    for expr in [
        "json_get_path_scalar(n.body, 'a.b', -1)",
        "json_get_path_scalar(n.body, json_parse('[\"a.b\",-1]'))",
        "json_get_path_scalar(n.body, CAST('[\"a.b\",-1]' AS JSON))",
    ] {
        let query = format!("MATCH (n:Doc) WHERE {expr} = 'É' RETURN n.id");
        assert_eq!(rows(&indexed, &query), [vec![Value::Int(1)]]);
        assert_eq!(rows(&indexed, &query), rows(&scan, &query));
    }
    let query = "MATCH (n:Doc) WHERE lower(n.name) = 'é' RETURN n.id ORDER BY n.id";
    // Binary collation: decomposed e + combining acute is not normalized to é.
    assert_eq!(
        rows(&indexed, query),
        [vec![Value::Int(1)], vec![Value::Int(3)]]
    );
    assert_eq!(rows(&indexed, query), rows(&scan, query));
    for selector in [
        "json_parse('[]')",
        "json_parse('[true]')",
        "json_parse('{}')",
        "1.5",
    ] {
        let query = format!(
            "MATCH (n:Doc) WHERE json_get_path_scalar(n.body, {selector}) = 'É' RETURN n.id"
        );
        let a = indexed.execute(&query).unwrap_err();
        assert_eq!(a.gqlstatus().unwrap().as_str(), "22G03");
        assert_eq!(a.gqlstatus(), scan.execute(&query).unwrap_err().gqlstatus());
    }
    for s in [&indexed, &scan] {
        s.execute(r#"MATCH (n:Doc) WHERE n.id = 4 SET n.body = CAST('{"a''b":"quoted"}' AS JSON)"#)
            .unwrap();
    }
    index(
        &db,
        &path,
        "escaped",
        "json_get_path_scalar(n.body, 'a''b')",
        ScalarIndexKind::String,
    );
    let escaped = "MATCH (n:Doc) WHERE json_get_path_scalar(n.body, 'a''b') = 'quoted' RETURN n.id";
    assert_eq!(rows(&indexed, escaped), [vec![Value::Int(4)]]);
    assert_eq!(rows(&indexed, escaped), rows(&scan, escaped));
    let path64 = std::iter::repeat_n("'x'", 64).collect::<Vec<_>>().join(",");
    index(
        &db,
        &path,
        "deep",
        &format!("json_get_path_scalar(n.body, {path64})"),
        ScalarIndexKind::String,
    );
    assert!(
        db.catalog()
            .create_expression_index(
                &path,
                &PathSegment::regular("too_deep").unwrap(),
                "Doc",
                &format!("json_get_path_scalar(n.body, {path64}, 'x')"),
                ScalarIndexKind::String
            )
            .is_err()
    );
}

#[test]
fn json_scalar_and_explicit_text_lookup_match_scan_across_lifecycle() {
    let db = Database::builder().build();
    let (path, indexed) = graph(&db, "indexed");
    let (_, scan) = graph(&db, "scan");
    for s in [&indexed, &scan] {
        s.execute(SEED).unwrap();
    }
    index(&db, &path, "kind", KEY, ScalarIndexKind::String);
    index(
        &db,
        &path,
        "folded",
        &format!("lower({KEY})"),
        ScalarIndexKind::String,
    );
    index(
        &db,
        &path,
        "text",
        "json_get_path_text(n.body, 'format', 'kind')",
        ScalarIndexKind::String,
    );
    assert_eq!(rows(&indexed, QUERY), [vec![Value::Int(1)]]);
    for mutation in [
        r#"MATCH (n:Doc) WHERE n.id = 2 SET n.body = CAST('{"format":{"kind":"jsonl"}}' AS JSON)"#,
        "MATCH (n:Doc) WHERE n.id = 1 REMOVE n.body",
        "MATCH (n:Doc) WHERE n.id = 2 REMOVE n:Doc",
        r#"INSERT (:Doc {id: 6, body: CAST('{"format":{"kind":"jsonl"}}' AS JSON)})"#,
        "MATCH (n:Doc) WHERE n.id = 6 DELETE n",
    ] {
        for s in [&indexed, &scan] {
            s.execute(mutation).unwrap();
        }
        for query in [
            QUERY.to_owned(),
            QUERY.replace(KEY, &format!("lower({KEY})")),
            QUERY.replace("_scalar", "_text"),
        ] {
            assert_eq!(rows(&indexed, &query), rows(&scan, &query), "{mutation}");
        }
    }
    let before = rows(&indexed, QUERY);
    indexed.execute("START TRANSACTION").unwrap();
    indexed.execute(SEED).unwrap();
    assert!(!rows(&indexed, QUERY).is_empty());
    indexed.execute("ROLLBACK").unwrap();
    assert_eq!(rows(&indexed, QUERY), before);
    db.catalog()
        .drop_declaration(
            &path,
            &PathSegment::regular("kind").unwrap(),
            DropPolicy::Strict,
        )
        .unwrap();
    assert_eq!(rows(&indexed, QUERY), rows(&scan, QUERY));
}

#[test]
fn heterogeneous_numbers_nulls_and_errors_are_not_silent_nonmatches() {
    let db = Database::builder().build();
    let (path, indexed) = graph(&db, "indexed");
    let (_, scan) = graph(&db, "scan");
    for s in [&indexed, &scan] {
        s.execute(r#"INSERT (:Doc {id: 1, body: CAST('{"v":1}' AS JSON)}), (:Doc {id: 2, body: CAST('{"v":1.0}' AS JSON)}), (:Doc {id: 3, body: CAST('{"v":1e0}' AS JSON)}), (:Doc {id: 4, body: CAST('{"v":null}' AS JSON)}), (:Doc {id: 5, body: CAST('{}' AS JSON)})"#).unwrap();
    }
    index(
        &db,
        &path,
        "number",
        "json_get_path_scalar(n.body, 'v')",
        ScalarIndexKind::I64,
    );
    let query =
        "MATCH (n:Doc) WHERE json_get_path_scalar(n.body, 'v') = 1 RETURN n.id ORDER BY n.id";
    assert_eq!(
        rows(&indexed, query),
        [
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)]
        ]
    );
    assert_eq!(rows(&indexed, query), rows(&scan, query));
    for body in [
        "'wrong'",
        "CAST('{\"v\":[]}' AS JSON)",
        "CAST('[1]' AS JSON)",
    ] {
        for s in [&indexed, &scan] {
            s.execute(&format!("MATCH (n:Doc) WHERE n.id = 1 SET n.body = {body}"))
                .unwrap();
        }
        let a = indexed.execute(query).unwrap_err();
        let b = scan.execute(query).unwrap_err();
        assert_eq!(a.gqlstatus(), b.gqlstatus());
        assert_eq!(a.gqlstatus().unwrap().as_str(), "22G03");
    }
    for selector in [
        "json_parse('[]')",
        "json_parse('[true]')",
        "json_parse('{}')",
        "1.5",
    ] {
        let query =
            format!("MATCH (n:Doc) WHERE json_get_path_scalar(n.body, {selector}) = 1 RETURN n.id");
        assert_eq!(
            indexed.execute(&query).unwrap_err().gqlstatus(),
            scan.execute(&query).unwrap_err().gqlstatus()
        );
    }
}

#[test]
fn rejected_target_classes_do_not_publish_declarations() {
    let db = Database::builder().build();
    let (path, _) = graph(&db, "targets");
    let before = db.catalog().snapshot();
    for source in [
        "lower($p)",
        "random()",
        "current_timestamp()",
        "json_object_keys(n.body)",
        "[n.id]",
        "n.other.id",
        "other.id",
        "selene.unknown(n.id)",
        "n.id RETURN 1",
        "json_get_path_scalar(n.body, json_parse('[]'))",
    ] {
        let error = db
            .catalog()
            .create_expression_index(
                &path,
                &PathSegment::regular("invalid").unwrap(),
                "Doc",
                source,
                ScalarIndexKind::String,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("expression"),
            "{source}: {error}"
        );
        assert!(db.catalog().snapshot().shares_state_with(&before));
    }
}

#[test]
fn declarations_and_keys_rebuild_after_wal_and_checkpoint_reopen() {
    for checkpoint in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::create(directory.path()).unwrap();
        let (path, session) = graph(&db, "durable");
        session.execute(SEED).unwrap();
        index(&db, &path, "kind", KEY, ScalarIndexKind::String);
        let declarations = db.catalog().snapshot().declarations(&path).unwrap();
        if checkpoint {
            db.checkpoint().unwrap();
        }
        session
            .execute("MATCH (n:Doc) WHERE n.id = 1 SET n.id = 11")
            .unwrap();
        drop(session);
        drop(db);
        let db = Database::open(directory.path()).unwrap();
        assert_eq!(db.recovery_info().unwrap().rebuilt_indexes, 1);
        assert_eq!(
            db.catalog().snapshot().declarations(&path).unwrap(),
            declarations
        );
        let session = db.session(&path).unwrap();
        assert_eq!(rows(&session, QUERY), [vec![Value::Int(11)]]);
        session.execute("MATCH (n:Doc) DELETE n").unwrap();
        drop(session);
        db.catalog()
            .create_graph(&path, None, CreatePolicy::OrReplace)
            .unwrap();
        assert!(
            db.catalog()
                .snapshot()
                .declarations(&path)
                .unwrap()
                .is_empty()
        );
        assert!(rows(&db.session(&path).unwrap(), QUERY).is_empty());
    }
}
