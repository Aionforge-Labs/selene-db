use super::*;

#[test]
fn vector_score_nodes_reranks_explicit_candidates_without_index() {
    let graph = graph(330_207);
    let registry = BuiltinProcedureRegistry::new();
    let ids = seed_vector_graph(&graph);
    {
        let mut txn = graph.begin_write();
        txn.mutator()
            .delete_node(ids[7])
            .expect("delete candidate succeeds");
        txn.commit().expect("delete commits");
    }
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[3.2, 0.0])));
    session.bind_parameter(
        db_string("nodes"),
        Value::List(vec![
            Value::NodeRef(ids[5]),
            Value::NodeRef(ids[3]),
            Value::NodeRef(ids[3]),
            Value::NodeRef(ids[0]),
            Value::NodeRef(ids[8]),
        ]),
    );

    let table = execute_rows(
        &mut session,
        "CALL selene.vector_score_nodes('embedding', $query, $nodes, 3, 'squared_euclidean') \
         YIELD node_id, distance",
        &registry,
    );

    assert_eq!(node_column(&table, "node_id"), vec![ids[3], ids[5], ids[0]]);
}

#[test]
fn vector_score_nodes_returns_no_rows_for_empty_candidate_list() {
    let graph = graph(330_208);
    let registry = BuiltinProcedureRegistry::new();
    seed_vector_graph(&graph);
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[0.0, 0.0])));
    session.bind_parameter(db_string("nodes"), Value::List(Vec::new()));

    let table = execute_rows(
        &mut session,
        "CALL selene.vector_score_nodes('embedding', $query, $nodes, 10) \
         YIELD node_id, distance",
        &registry,
    );

    assert_eq!(table.row_count(), 0);
}

#[test]
fn vector_score_nodes_rejects_non_node_candidates() {
    let graph = graph(330_209);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("query"), Value::Vector(vector(&[0.0, 0.0])));
    session.bind_parameter(db_string("nodes"), Value::List(vec![Value::Int(1)]));

    let err = session
        .execute_source(
            "CALL selene.vector_score_nodes('embedding', $query, $nodes, 10)",
            &registry,
        )
        .expect_err("non-node candidates must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("nodes[0] must be a NODE")
    ));
}

#[test]
fn vector_score_nodes_batch_reranks_per_query_candidate_sets() {
    let graph = graph(330_210);
    let registry = BuiltinProcedureRegistry::new();
    let ids = seed_vector_graph(&graph);
    let mut session = Session::new(&graph);
    session.bind_parameter(
        db_string("queries"),
        Value::List(vec![
            Value::Vector(vector(&[3.2, 0.0])),
            Value::Vector(vector(&[5.1, 0.0])),
        ]),
    );
    session.bind_parameter(
        db_string("nodes"),
        Value::List(vec![
            Value::List(vec![
                Value::NodeRef(ids[5]),
                Value::NodeRef(ids[3]),
                Value::NodeRef(ids[3]),
                Value::NodeRef(ids[0]),
            ]),
            Value::List(vec![
                Value::NodeRef(ids[7]),
                Value::NodeRef(ids[5]),
                Value::NodeRef(ids[1]),
            ]),
        ]),
    );

    let table = execute_rows(
        &mut session,
        "CALL selene.vector_score_nodes_batch('embedding', $queries, $nodes, 2, 'squared_euclidean') \
         YIELD query_index, node_id, distance",
        &registry,
    );

    assert_eq!(uint_column(&table, "query_index"), vec![0, 0, 1, 1]);
    assert_eq!(
        node_column(&table, "node_id"),
        vec![ids[3], ids[5], ids[5], ids[7]]
    );
}

#[test]
fn vector_score_nodes_batch_returns_no_rows_for_empty_queries() {
    let graph = graph(330_211);
    let registry = BuiltinProcedureRegistry::new();
    seed_vector_graph(&graph);
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("queries"), Value::List(Vec::new()));
    session.bind_parameter(db_string("nodes"), Value::List(Vec::new()));

    let table = execute_rows(
        &mut session,
        "CALL selene.vector_score_nodes_batch('embedding', $queries, $nodes, 10) \
         YIELD query_index, node_id, distance",
        &registry,
    );

    assert_eq!(table.row_count(), 0);
}

#[test]
fn vector_score_nodes_batch_rejects_mismatched_query_and_node_sets() {
    let graph = graph(330_212);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(
        db_string("queries"),
        Value::List(vec![Value::Vector(vector(&[0.0, 0.0]))]),
    );
    session.bind_parameter(db_string("nodes"), Value::List(Vec::new()));

    let err = session
        .execute_source(
            "CALL selene.vector_score_nodes_batch('embedding', $queries, $nodes, 10)",
            &registry,
        )
        .expect_err("mismatched batch arity must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("queries and nodes must have the same length")
    ));
}

#[test]
fn vector_score_nodes_batch_rejects_non_nested_node_candidates() {
    let graph = graph(330_213);
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.bind_parameter(
        db_string("queries"),
        Value::List(vec![Value::Vector(vector(&[0.0, 0.0]))]),
    );
    session.bind_parameter(
        db_string("nodes"),
        Value::List(vec![Value::List(vec![Value::Int(1)])]),
    );

    let err = session
        .execute_source(
            "CALL selene.vector_score_nodes_batch('embedding', $queries, $nodes, 10)",
            &registry,
        )
        .expect_err("non-node nested candidates must error");

    assert!(matches!(
        err,
        ExecutorError::Procedure {
            source: ProcedureError::InvalidArgument { ref detail },
            ..
        } if detail.contains("nodes[0][0] must be a NODE")
    ));
}
