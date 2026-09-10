//! `LABELS(...)` value-function integration tests.

use selene_core::{DbString, EdgeId, GraphId, LabelSet, NodeId, PropertyMap, Value};
use selene_gql::{
    EmptyProcedureRegistry, PipelineStatement, Session, StatementOutput, ValueExpr,
    ast::{format_read_statement, structurally_eq},
};
use selene_graph::SharedGraph;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn rows(output: StatementOutput) -> selene_gql::BindingTable {
    match output {
        StatementOutput::Rows(table) => table,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn single_value(session: &mut Session<'_>, source: &str) -> Value {
    let table = rows(
        session
            .execute_source(source, &EmptyProcedureRegistry)
            .expect("query succeeds"),
    );
    assert_eq!(table.row_count(), 1);
    table.rows()[0].values()[0].clone()
}

#[test]
fn labels_expr_parses_as_keyword_function() {
    let statement = selene_gql::parse("MATCH (n) RETURN LABELS(n) AS \"labels\"").unwrap();
    let selene_gql::Statement::Query(pipeline) = statement else {
        panic!("expected query");
    };
    let PipelineStatement::Return(clause) = &pipeline.statements[1] else {
        panic!("expected RETURN");
    };
    let ValueExpr::FunctionCall { name, args, .. } = &clause.items[0].expr else {
        panic!("expected function call");
    };
    assert_eq!(name.first().as_str(), "labels");
    assert_eq!(args.len(), 1);
}

#[test]
fn labels_expr_formats_as_keyword_function() {
    let parsed = selene_gql::parse("MATCH (n) RETURN labels(n) AS \"labels\"").unwrap();
    let formatted = format_read_statement(&parsed).expect("formats");
    assert_eq!(formatted, "MATCH (n)\nRETURN LABELS(n) AS \"labels\"");
    let reparsed = selene_gql::parse(&formatted).expect("formatted source reparses");
    assert!(structurally_eq(&parsed, &reparsed));
}

#[test]
fn labels_returns_sorted_node_labels() {
    let graph = SharedGraph::new(GraphId::new(61_001));
    {
        let mut txn = graph.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::from_iter([db_string("Beta"), db_string("Alpha")]),
                PropertyMap::new(),
            )
            .unwrap();
        txn.commit().unwrap();
    }

    let mut session = Session::new(&graph);
    assert_eq!(
        single_value(
            &mut session,
            "MATCH (n:Alpha) RETURN LABELS(n) AS \"labels\""
        ),
        Value::List(vec![
            Value::String(db_string("Alpha")),
            Value::String(db_string("Beta")),
        ])
    );
}

#[test]
fn labels_returns_edge_label_singleton() {
    let graph = SharedGraph::new(GraphId::new(61_002));
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let a = mutator
            .create_node(LabelSet::single(db_string("A")), PropertyMap::new())
            .unwrap();
        let b = mutator
            .create_node(LabelSet::single(db_string("B")), PropertyMap::new())
            .unwrap();
        mutator
            .create_edge(db_string("KNOWS"), a, b, PropertyMap::new())
            .unwrap();
        txn.commit().unwrap();
    }

    let mut session = Session::new(&graph);
    assert_eq!(
        single_value(
            &mut session,
            "MATCH ()-[e:KNOWS]->() RETURN LABELS(e) AS \"labels\""
        ),
        Value::List(vec![Value::String(db_string("KNOWS"))])
    );
}

#[test]
fn labels_propagates_null_and_rejects_non_elements() {
    let graph = SharedGraph::new(GraphId::new(61_003));
    let mut session = Session::new(&graph);

    assert_eq!(
        single_value(&mut session, "RETURN LABELS(NULL) AS \"labels\""),
        Value::Null
    );
    let err = session
        .execute_source("RETURN LABELS(1) AS \"labels\"", &EmptyProcedureRegistry)
        .expect_err("non-element LABELS argument rejects");
    assert_eq!(err.gqlstatus().as_str(), "22G03");
}

#[test]
fn labels_rejects_absent_element_references() {
    let graph = SharedGraph::new(GraphId::new(61_004));
    let mut session = Session::new(&graph);
    for value in [
        Value::NodeRef(NodeId::new(999)),
        Value::EdgeRef(EdgeId::new(999)),
    ] {
        session.bind_parameter(db_string("element"), value);
        let error = session
            .execute_source("RETURN LABELS($element)", &EmptyProcedureRegistry)
            .unwrap_err();
        assert_eq!(error.gqlstatus().as_str(), "22G11");
    }
}
