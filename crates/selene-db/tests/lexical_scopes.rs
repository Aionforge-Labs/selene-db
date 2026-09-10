//! ISO working scopes and retained single-graph transaction authority.

use selene_db::{
    CreatePolicy, Database, ExecutionOutcome, GeneralParameter, GqlType, ObjectPath, Request,
    RequestParams, SchemaPath, Session, Value,
};

fn fixture() -> (Database, Session) {
    let database = Database::builder().build();
    for (schema, value) in [("red", 11), ("blue", 22)] {
        database
            .catalog()
            .create_schema(
                &SchemaPath::regular("selene", schema).unwrap(),
                CreatePolicy::Strict,
            )
            .unwrap();
        let graph = ObjectPath::regular("selene", schema, "g").unwrap();
        database
            .catalog()
            .create_graph(&graph, None, CreatePolicy::Strict)
            .unwrap();
        database
            .session(&graph)
            .unwrap()
            .execute(&format!("INSERT (:Item {{x: {value}}})"))
            .unwrap();
    }
    let session = database
        .session(&ObjectPath::regular("selene", "red", "g").unwrap())
        .unwrap();
    (database, session)
}

fn values(outcome: ExecutionOutcome) -> Vec<Value> {
    let ExecutionOutcome::Rows { result, .. } = outcome else {
        panic!("rows expected");
    };
    assert_eq!(result.rows().len(), 1);
    result.rows()[0].values().to_vec()
}

#[test]
fn request_local_graph_selection_uses_selected_data_and_does_not_set_session() {
    let (_database, session) = fixture();
    let initial = session.context().current_graph();
    session
        .set_parameter(
            "x",
            GeneralParameter::new(GqlType::Integer, Value::Int(11)).unwrap(),
        )
        .unwrap();
    let mut params = RequestParams::new();
    params
        .insert(
            "x",
            GeneralParameter::new(GqlType::Integer, Value::Int(22)).unwrap(),
        )
        .unwrap();
    let result = session
        .execute_request(Request::with_params(
            "USE /blue/g MATCH (x:Item) FILTER x.x = $x RETURN x.x",
            params,
        ))
        .into_result()
        .unwrap();
    assert_eq!(values(result), [Value::Int(22)]);
    assert_eq!(session.context().current_graph(), initial);
    assert_eq!(
        values(session.execute("MATCH (n:Item) RETURN n.x").unwrap()),
        [Value::Int(11)]
    );
}

#[test]
fn nested_schema_selection_restores_siblings_and_preserves_one_graph() {
    let (_database, session) = fixture();
    let result = session
        .execute(
            "AT /blue USE g \
        CALL { AT /red USE /blue/g RETURN 1 AS x } YIELD x \
        CALL { USE g MATCH (n:Item) RETURN n.x AS y } YIELD y RETURN x, y",
        )
        .unwrap();
    assert_eq!(values(result), [Value::Int(1), Value::Int(22)]);
    assert_eq!(
        values(session.execute("MATCH (n:Item) RETURN n.x").unwrap()),
        [Value::Int(11)]
    );
}

#[test]
fn second_graph_in_implicit_request_is_25g04_without_session_leak() {
    let (_database, session) = fixture();
    let initial = session.context().current_graph();
    let error = session
        .execute("USE /red/g { USE /blue/g RETURN 1 }")
        .unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "25G04");
    assert_eq!(session.context().current_graph(), initial);
    assert_eq!(
        values(session.execute("MATCH (n:Item) RETURN n.x").unwrap()),
        [Value::Int(11)]
    );
}

#[test]
fn second_graph_in_explicit_transaction_fails_through_normal_abort_path() {
    let (_database, session) = fixture();
    session.execute("START TRANSACTION").unwrap();
    session
        .execute("USE /red/g MATCH (n:Item) RETURN n.x")
        .unwrap();
    let error = session
        .execute("USE /blue/g MATCH (n:Item) RETURN n.x")
        .unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "25G04");
    assert_eq!(
        session
            .execute("RETURN 1")
            .unwrap_err()
            .gqlstatus()
            .unwrap()
            .as_str(),
        "25N02"
    );
    session.execute("ROLLBACK").unwrap();
    assert_eq!(
        values(session.execute("MATCH (n:Item) RETURN n.x").unwrap()),
        [Value::Int(11)]
    );
}

fn analysis_error(error: &selene_db::Error) -> &selene_gql::AnalysisError {
    use std::error::Error;
    let lower = error
        .source()
        .unwrap()
        .downcast_ref::<selene_gql::ExecutorError>()
        .unwrap();
    let selene_gql::ExecutorError::Analysis { source } = lower else {
        panic!("analysis error expected: {lower:?}");
    };
    source
}

#[test]
fn binding_precedence_is_distinct_from_parameters_and_forced_catalog_references() {
    let (_database, session) = fixture();
    session
        .set_parameter(
            "g",
            GeneralParameter::new(GqlType::Integer, Value::Int(9)).unwrap(),
        )
        .unwrap();
    assert_eq!(
        values(session.execute("USE g RETURN $g").unwrap()),
        [Value::Int(9)]
    );
    let source = "LET g = 1 CALL { USE g RETURN 2 AS x } YIELD x RETURN x";
    let error = session.execute(source).unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "22G03");
    let selene_gql::AnalysisError::TypeMismatch { span, .. } = analysis_error(&error) else {
        panic!("wrong namespace diagnostic");
    };
    assert_eq!(&source[span.byte_offset as usize..span.end() as usize], "g");
    for reference in ["./g", "\"g\""] {
        assert_eq!(
            values(
                session
                    .execute(&format!(
                        "LET g = 1 CALL {{ USE {reference} RETURN 2 AS x }} YIELD x RETURN x"
                    ))
                    .unwrap()
            ),
            [Value::Int(2)]
        );
    }
    // An explicit empty import scope makes the outer binding unavailable, so
    // the regular bare name must resolve in the catalog again (§11.1 SR2).
    assert_eq!(
        values(
            session
                .execute("LET g = 1 CALL () { USE g RETURN 2 AS x } YIELD x RETURN x")
                .unwrap()
        ),
        [Value::Int(2)]
    );
}

#[test]
fn next_table_columns_are_not_graph_expression_working_record_bindings() {
    let (_database, session) = fixture();
    // §11.1 SR2 consults the incoming working record, not the working table
    // supplied by NEXT. The same name still denotes a column in RETURN.
    assert_eq!(
        values(
            session
                .execute("USE /red/g RETURN 1 AS g NEXT USE g RETURN g")
                .unwrap()
        ),
        [Value::Int(1)]
    );
}

#[test]
fn lexical_errors_have_original_spans_and_do_not_leak_session_state() {
    let (_database, session) = fixture();
    session
        .execute("CREATE GRAPH TYPE /blue/notgraph AS { NODE TYPE Item () }")
        .unwrap();
    for (source, needle) in [
        ("USE /blue/missing RETURN 1", "/blue/missing"),
        ("USE /blue/notgraph RETURN 1", "/blue/notgraph"),
        ("AT /missing USE g RETURN 1", "/missing"),
    ] {
        let error = session.execute(source).unwrap_err();
        assert_eq!(error.gqlstatus().unwrap().as_str(), "42002", "{source}");
        let selene_gql::AnalysisError::InvalidReference { span, .. } = analysis_error(&error)
        else {
            panic!("reference error expected");
        };
        assert_eq!(
            &source[span.byte_offset as usize..span.end() as usize],
            needle
        );
    }
    let source = "USE /red/g { USE /blue/g RETURN 1 }";
    let error = session.execute(source).unwrap_err();
    let selene_gql::AnalysisError::MultipleGraphs { span, .. } = analysis_error(&error) else {
        panic!("GT03 error expected");
    };
    assert_eq!(
        &source[span.byte_offset as usize..span.end() as usize],
        "/blue/g"
    );
    assert_eq!(
        values(session.execute("USE g MATCH (n:Item) RETURN n.x").unwrap()),
        [Value::Int(11)]
    );
}

#[test]
fn same_graph_nesting_and_persistent_session_controls_are_independent() {
    let (_database, session) = fixture();
    assert_eq!(
        values(
            session
                .execute("USE /blue/g { AT /red USE CURRENT_GRAPH MATCH (n:Item) RETURN n.x }")
                .unwrap()
        ),
        [Value::Int(22)]
    );
    session.execute("SESSION SET SCHEMA /blue").unwrap();
    session.execute("SESSION SET GRAPH g").unwrap();
    assert_eq!(
        values(
            session
                .execute("AT /red USE g MATCH (n:Item) RETURN n.x")
                .unwrap()
        ),
        [Value::Int(11)]
    );
    assert_eq!(
        values(session.execute("MATCH (n:Item) RETURN n.x").unwrap()),
        [Value::Int(22)]
    );
}

#[test]
fn named_graph_replacement_invalidates_dependent_cache_without_stale_id_rebinding() {
    let (database, session) = fixture();
    let blue = ObjectPath::regular("selene", "blue", "g").unwrap();
    let old = database.session(&blue).unwrap();
    old.execute("MATCH (n) DETACH DELETE n").unwrap();
    let query = "USE /blue/g MATCH (n:Item) RETURN count(n)";
    for _ in 0..2 {
        assert_eq!(values(session.execute(query).unwrap()), [Value::Int(0)]);
    }
    database
        .catalog()
        .create_graph(&blue, None, CreatePolicy::OrReplace)
        .unwrap();
    database
        .session(&blue)
        .unwrap()
        .execute("INSERT (:Item {x: 33})")
        .unwrap();
    assert_eq!(values(session.execute(query).unwrap()), [Value::Int(1)]);
    assert_eq!(
        values(
            session
                .execute("USE /blue/g MATCH (n:Item) RETURN n.x")
                .unwrap()
        ),
        [Value::Int(33)]
    );
    assert!(
        old.execute("RETURN 1").is_err(),
        "existing stable session identities must remain stale"
    );
}

#[test]
fn explicit_same_graph_queries_keep_the_pinned_data_snapshot() {
    let (database, session) = fixture();
    session
        .start_transaction(selene_db::TransactionAccessMode::ReadOnly)
        .unwrap();
    let writer = database
        .session(&ObjectPath::regular("selene", "red", "g").unwrap())
        .unwrap();
    writer.execute("INSERT (:Item {x: 99})").unwrap();
    assert_eq!(
        values(
            session
                .execute("AT /blue USE /red/g { USE CURRENT_GRAPH MATCH (n:Item) RETURN count(n) }")
                .unwrap()
        ),
        [Value::Int(1)]
    );
    session.commit_transaction().unwrap();
    assert_eq!(
        values(session.execute("MATCH (n:Item) RETURN count(n)").unwrap()),
        [Value::Int(2)]
    );
}

#[test]
fn an_unused_ambient_default_is_not_an_executed_graph_access() {
    let (_database, session) = fixture();
    assert_eq!(
        values(
            session
                .execute("CALL { USE /blue/g MATCH (n:Item) RETURN n.x AS x } YIELD x RETURN x")
                .unwrap()
        ),
        [Value::Int(22)]
    );
    let error = session
        .execute("MATCH (n:Item) CALL { USE /blue/g RETURN 1 AS x } YIELD x RETURN x")
        .unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "25G04");
}
