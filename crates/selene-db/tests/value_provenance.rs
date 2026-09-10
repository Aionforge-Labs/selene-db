//! Facade ownership and referent lifetime at the value boundary.

use selene_db::{
    CreatePolicy, Database, ExecutionOutcome, GeneralParameter, ObjectPath, Request, RequestParams,
    SchemaPath, Session, Type, Value,
};

fn fixture(name: &str) -> Session {
    let database = Database::builder().build();
    let schema = SchemaPath::regular("selene", name).unwrap();
    let path = ObjectPath::regular("selene", name, "graph").unwrap();
    database
        .catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    database
        .catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    database.session(&path).unwrap()
}

fn node(session: &Session) -> Value {
    let ExecutionOutcome::Written {
        result: Some(result),
        ..
    } = session
        .execute("INSERT (n:Item { name: 'alive' }) RETURN n")
        .unwrap()
    else {
        panic!("rows")
    };
    result.rows()[0].values()[0].clone()
}

fn run(session: &Session, source: &str, value: Value) -> selene_db::Result<ExecutionOutcome> {
    let mut params = RequestParams::new();
    params
        .insert("n", GeneralParameter::new(Type::NODE, value).unwrap())
        .unwrap();
    session
        .execute_request(Request::with_params(source, params))
        .into_result()
}

#[test]
fn equal_numeric_ids_from_foreign_databases_do_not_resolve() {
    let first = fixture("first");
    let second = fixture("second");
    let foreign = node(&first);
    let _local = node(&second);
    let error = run(&second, "RETURN $n.name", foreign).unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "42002");
    assert!(error.message().contains("another database"));
}

#[test]
fn deleted_reference_can_be_copied_but_dereference_is_22g11() {
    let session = fixture("deleted");
    let reference = node(&session);
    session.execute("MATCH (n:Item) DELETE n FINISH").unwrap();
    let copied = run(&session, "RETURN $n", reference.clone()).unwrap();
    assert_eq!(copied.row_count(), Some(1));
    let error = run(&session, "RETURN $n.name", reference).unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "22G11");
}

#[test]
fn deleted_references_fail_graph_access_but_identity_only_procedures_can_copy() {
    let session = fixture("procedure_access");
    let reference = node(&session);
    session.execute("MATCH (n:Item) DELETE n FINISH").unwrap();
    let fused = run(
        &session,
        "CALL selene.reciprocal_rank_fusion([[$n]], 1) YIELD node_id RETURN node_id",
        reference.clone(),
    )
    .unwrap();
    let ExecutionOutcome::Rows { result, .. } = fused else {
        panic!("rows")
    };
    assert_eq!(result.rows()[0].values(), std::slice::from_ref(&reference));
    for source in [
        "RETURN labels($n)",
        "RETURN PATH[$n]",
        "CALL selene.vector_score_nodes('v', CAST([1.0, 0.0] AS VECTOR), [$n], 1) YIELD node_id RETURN node_id",
        "CALL selene.vector_score_nodes_batch('v', [CAST([1.0, 0.0] AS VECTOR)], [[$n]], 1) YIELD node_id RETURN node_id",
        "CALL selene.reachable_nodes([$n], 'E', 1) YIELD node_id RETURN node_id",
        "CALL algo.dijkstra('absent', $n, $n) YIELD cost RETURN cost",
        "CALL algo.sssp('absent', $n) YIELD cost RETURN cost",
        "CALL algo.pagerank('absent', NULL, NULL, NULL, NULL, NULL, [{node_id: $n, weight: 1.0D}]) YIELD node_id RETURN node_id",
        "CALL algo.pagerank('absent', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, [$n]) YIELD node_id RETURN node_id",
    ] {
        let error = run(&session, source, reference.clone()).unwrap_err();
        assert_eq!(
            error.gqlstatus().unwrap().as_str(),
            "22G11",
            "{source}: {error}"
        );
    }
}

#[test]
fn accessing_a_deleted_mutation_target_rolls_back_with_22g11() {
    let session = fixture("mutation_access");
    node(&session);
    for tail in [
        "SET n.name = 'changed'",
        "REMOVE n.name",
        "INSERT (n)-[:E]->(:Other)",
    ] {
        let source = format!("MATCH (n:Item) DELETE n {tail} FINISH");
        let error = session.execute(&source).unwrap_err();
        assert_eq!(
            error.gqlstatus().unwrap().as_str(),
            "22G11",
            "{source}: {error}"
        );
        assert_eq!(
            session.execute("MATCH (n) RETURN n").unwrap().row_count(),
            Some(1)
        );
    }
}
