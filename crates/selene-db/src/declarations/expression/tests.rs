use super::*;

#[test]
fn selective_expression_executes_with_one_candidate_and_failure_preserves_state() {
    let db = crate::Database::builder().build();
    let path = ObjectPath::regular("selene", "expr", "data").unwrap();
    db.catalog()
        .create_schema(
            &crate::SchemaPath::regular("selene", "expr").unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let session = db.session(&path).unwrap();
    let values = (0..128)
        .map(|i| format!("(:Doc {{id: {i}, body: CAST('{{\"key\":\"value{i}\"}}' AS JSON)}})"))
        .collect::<Vec<_>>()
        .join(",");
    session.execute(&format!("INSERT {values}")).unwrap();
    let source = "json_get_path_scalar(n.body, 'key')";
    let name = PathSegment::regular("key").unwrap();
    let before = db.catalog().snapshot();
    *db.inner.failure.lock() = Some(crate::catalog::FailurePoint::BeforePublication);
    assert!(
        db.catalog()
            .create_expression_index(&path, &name, "Doc", source, ScalarIndexKind::String)
            .is_err()
    );
    assert!(db.catalog().snapshot().shares_state_with(&before));
    db.catalog()
        .create_expression_index(&path, &name, "Doc", source, ScalarIndexKind::String)
        .unwrap();
    let state = db.inner.state.load_full();
    let snapshot = state.graphs.values().next().unwrap().graph.read();
    let native = selene_graph::SharedGraph::try_from_graph(snapshot.as_ref().clone()).unwrap();
    for descriptor in snapshot.catalog_bound_indexes() {
        if let CatalogPayload::Index(declaration) = descriptor.payload() {
            assert!(snapshot.matches_index_declaration(declaration));
        }
    }
    let query = "MATCH (n:Doc) WHERE json_get_path_scalar(n.body, 'key') = 'value7' RETURN n.id";
    let mut indexed = selene_gql::Session::new(&native).with_max_nodes_scanned(1);
    indexed.execute_source(query, &db.inner.procedures).unwrap();
    let mut none = selene_gql::Session::new(&native).with_max_nodes_scanned(0);
    assert_eq!(
        none.execute_source(query, &db.inner.procedures)
            .unwrap_err()
            .gqlstatus()
            .as_str(),
        "5GQL1"
    );
    let mut scan = selene_gql::Session::new(&native)
        .without_index_selection()
        .with_max_nodes_scanned(1);
    assert_eq!(
        scan.execute_source(query, &db.inner.procedures)
            .unwrap_err()
            .gqlstatus()
            .as_str(),
        "5GQL1"
    );
    // Planner evidence is separate from the exact 0/1 scan-budget boundary.
    let mut host = selene_gql::Session::new(&native);
    let explain = host
        .execute_source(&format!("EXPLAIN {query}"), &db.inner.procedures)
        .unwrap();
    assert!(
        format!("{explain:?}").contains("ExpressionLookup"),
        "{explain:?}"
    );
    for alternative in [
        query.replace("_scalar", "_text"),
        query.replace(
            "json_get_path_scalar(n.body, 'key')",
            "upper(json_get_path_scalar(n.body, 'key'))",
        ),
        query.replace(" = 'value7'", " = 'value7' AND 1 / n.id > 0"),
    ] {
        let explain = host
            .execute_source(&format!("EXPLAIN {alternative}"), &db.inner.procedures)
            .unwrap();
        assert!(!format!("{explain:?}").contains("ExpressionLookup"));
    }
    host.execute_source(query, &db.inner.procedures).unwrap();
    host.execute_source(
        "INSERT (:Doc {id: 1000, body: 'wrong type'})",
        &db.inner.procedures,
    )
    .unwrap();
    assert_eq!(
        host.execute_source(query, &db.inner.procedures)
            .unwrap_err()
            .gqlstatus()
            .as_str(),
        "22G03"
    );
    host.execute_source(
        "MATCH (n:Doc) WHERE n.id = 1000 DELETE n",
        &db.inner.procedures,
    )
    .unwrap();
    indexed.execute_source(query, &db.inner.procedures).unwrap();
    let equivalent = query.replace("n.body, 'key'", "n.body, CAST('[\"key\"]' AS JSON)");
    indexed
        .execute_source(&equivalent, &db.inner.procedures)
        .unwrap();
    host.execute_source(
        "MATCH (n:Doc) WHERE n.id = 0 DELETE n",
        &db.inner.procedures,
    )
    .unwrap();
    let compacted = selene_graph::compact_core(&native.read()).unwrap();
    assert!(compacted.report.reclaimed_nodes > 0);
    let compacted = selene_graph::SharedGraph::try_from_graph(compacted.graph).unwrap();
    selene_gql::Session::new(&compacted)
        .with_max_nodes_scanned(1)
        .execute_source(query, &db.inner.procedures)
        .unwrap();
    let mut wrong_owner = compacted.read().as_ref().clone();
    wrong_owner.meta.graph_id = selene_core::GraphId::new(999);
    assert_eq!(
        wrong_owner
            .scalar_expression_indexes(&selene_core::db_string("Doc").unwrap())
            .count(),
        0
    );
    db.catalog()
        .create_expression_index(
            &path,
            &PathSegment::regular("text").unwrap(),
            "Doc",
            "json_get_path_text(n.body, 'key')",
            ScalarIndexKind::String,
        )
        .unwrap();
    let state = db.inner.state.load_full();
    let snapshot = state.graphs.values().next().unwrap().graph.read();
    let text = selene_graph::SharedGraph::try_from_graph(snapshot.as_ref().clone()).unwrap();
    selene_gql::Session::new(&text)
        .with_max_nodes_scanned(1)
        .execute_source(&query.replace("_scalar", "_text"), &db.inner.procedures)
        .unwrap();
}
