//! Implementation-defined JSON value and scalar function coverage.

#![cfg(feature = "test-harness")]

mod exec_common;

use exec_common::{column_values, db_string, execute_read, execute_read_result};
use selene_core::{GraphId, JsonValue as CoreJsonValue, PropertyValueType, Value};
use selene_gql::{
    EmptyProcedureRegistry, ExecutorError, GqlType, PipelineStatement, QueryPipeline, ReturnClause,
    ReturnItem, Session, SourceSpan, Statement, StatementOutput, ValueExpr, feature_walk, parse,
};
use selene_graph::{GraphTypeDef, SharedGraph};
use selene_profile::FeatureId;

fn single_value(source: &str, column: &str) -> Value {
    let table = execute_read(source);
    let mut values = column_values(&table, column);
    assert_eq!(values.len(), 1);
    values.pop().expect("one row")
}

fn json_value(source: &str) -> CoreJsonValue {
    match single_value(source, "value") {
        Value::Json(value) => value,
        other => panic!("expected JSON value, got {other:?}"),
    }
}

fn string_value(source: &str) -> String {
    match single_value(source, "value") {
        Value::String(value) => value.to_string(),
        other => panic!("expected STRING value, got {other:?}"),
    }
}

fn int_value(source: &str) -> i64 {
    match single_value(source, "value") {
        Value::Int(value) => value,
        other => panic!("expected INTEGER value, got {other:?}"),
    }
}

fn string_list_value(source: &str) -> Vec<String> {
    match single_value(source, "value") {
        Value::List(values) => values
            .into_iter()
            .map(|value| match value {
                Value::String(value) => value.to_string(),
                other => panic!("expected STRING list item, got {other:?}"),
            })
            .collect(),
        other => panic!("expected LIST value, got {other:?}"),
    }
}

fn bool_value(source: &str) -> bool {
    match single_value(source, "value") {
        Value::Bool(value) => value,
        other => panic!("expected BOOLEAN value, got {other:?}"),
    }
}

fn assert_status(source: &str, expected: &str) {
    let err = execute_read_result(source).expect_err("query should fail");
    assert_eq!(err.gqlstatus().as_str(), expected, "source: {source}");
}

fn assert_feature_recorded(source: &str) {
    let statement = parse(source).expect(source);
    let observed = feature_walk(&statement)
        .into_iter()
        .map(|feature| feature.feature_id)
        .collect::<Vec<_>>();
    assert!(
        observed.contains(&FeatureId::IM_JSON),
        "{source} should record IM_JSON, observed {observed:?}"
    );
}

fn typed_parameter_statement(name: selene_core::DbString) -> Statement {
    let span = SourceSpan::new(0, 4);
    Statement::Query(QueryPipeline {
        working_scopes: Vec::new(),
        select_origin: None,
        statements: vec![PipelineStatement::Return(ReturnClause {
            distinct: false,
            star: false,
            items: vec![ReturnItem {
                expr: ValueExpr::Parameter {
                    name: name.clone(),
                    declared_type: Some(GqlType::Json),
                    span,
                },
                alias: Some(name),
                span,
            }],
            group_by: None,
            having: None,
            span,
        })],
        span,
    })
}

fn empty_closed_graph(id: u64) -> SharedGraph {
    SharedGraph::builder(GraphId::new(id))
        .bound_to(GraphTypeDef {
            name: db_string("json.test.graph"),
            node_types: Vec::new(),
            edge_types: Vec::new(),
        })
        .expect("graph type binds")
        .build()
        .expect("graph builds")
}

fn rows_from_output(output: StatementOutput) -> selene_gql::BindingTable {
    let StatementOutput::Rows(table) = output else {
        panic!("expected row output");
    };
    table
}

#[path = "scalar_functions_json/functions.rs"]
mod functions;
#[path = "scalar_functions_json/types_and_catalog.rs"]
mod types_and_catalog;
