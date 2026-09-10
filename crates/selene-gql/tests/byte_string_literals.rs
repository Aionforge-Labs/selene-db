//! Byte-string literal coverage for the native BYTES value type.

use std::sync::Arc;

use selene_core::{ByteStringType, DbString, GraphId, PropertyValueType, Record, Value};
use selene_gql::{
    AnalyzedStatement, AnalyzedType, EmptyProcedureRegistry, GqlType, ImplDefinedCaps, ParserError,
    PipelineStatement, Session, Statement, StatementOutput,
    ast::{format_read_statement, structurally_eq},
    feature_walk, parse,
};
use selene_graph::{GraphTypeDef, PropertyElementType, RecordFieldType, SharedGraph};
use selene_profile::FeatureId;
use smallvec::smallvec;

fn first_value(source: &str) -> Value {
    let graph = SharedGraph::new(GraphId::new(14_200));
    let mut session = Session::new(&graph);
    let output = session
        .execute_source(source, &EmptyProcedureRegistry)
        .unwrap_or_else(|err| panic!("execute failed for `{source}`: {err:?}"));
    let StatementOutput::Rows(table) = output else {
        panic!("`{source}` produced non-row output");
    };
    table.rows()[0].values()[0].clone()
}

fn first_status(source: &str) -> String {
    let graph = SharedGraph::new(GraphId::new(14_201));
    let mut session = Session::new(&graph);
    session
        .execute_source(source, &EmptyProcedureRegistry)
        .expect_err("statement errors")
        .gqlstatus()
        .as_str()
        .to_owned()
}

fn first_value_with_caps(source: &str, caps: ImplDefinedCaps) -> Value {
    let graph = SharedGraph::new(GraphId::new(14_203));
    let mut session = Session::new(&graph).with_impl_defined_caps(caps);
    let output = session
        .execute_source(source, &EmptyProcedureRegistry)
        .unwrap_or_else(|err| panic!("execute failed for `{source}`: {err:?}"));
    let StatementOutput::Rows(table) = output else {
        panic!("`{source}` produced non-row output");
    };
    table.rows()[0].values()[0].clone()
}

fn first_value_with_parameter(source: &str, name: &str, value: Value) -> Value {
    let graph = SharedGraph::new(GraphId::new(14_205));
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string(name), value);
    let output = session
        .execute_source(source, &EmptyProcedureRegistry)
        .unwrap_or_else(|err| panic!("execute failed for `{source}`: {err:?}"));
    let StatementOutput::Rows(table) = output else {
        panic!("`{source}` produced non-row output");
    };
    table.rows()[0].values()[0].clone()
}

fn first_status_with_parameter(source: &str, name: &str, value: Value) -> String {
    let graph = SharedGraph::new(GraphId::new(14_206));
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string(name), value);
    session
        .execute_source(source, &EmptyProcedureRegistry)
        .expect_err("statement errors")
        .gqlstatus()
        .as_str()
        .to_owned()
}

fn first_status_with_caps(source: &str, caps: ImplDefinedCaps) -> String {
    let graph = SharedGraph::new(GraphId::new(14_204));
    let mut session = Session::new(&graph).with_impl_defined_caps(caps);
    session
        .execute_source(source, &EmptyProcedureRegistry)
        .expect_err("statement errors")
        .gqlstatus()
        .as_str()
        .to_owned()
}

fn bytes(values: &[u8]) -> Value {
    Value::Bytes(Arc::<[u8]>::from(values))
}

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn empty_closed_graph(id: u64) -> SharedGraph {
    SharedGraph::builder(GraphId::new(id))
        .bound_to(GraphTypeDef {
            name: db_string("byte.string.graph"),
            node_types: Vec::new(),
            edge_types: Vec::new(),
        })
        .unwrap()
        .build()
        .unwrap()
}

fn analyze_one(source: &str) -> AnalyzedStatement {
    let statement = parse(source).expect("test source parses");
    selene_gql::analyze(statement, &EmptyProcedureRegistry, None).expect("test source analyzes")
}

fn assert_feature_recorded(source: &str, expected: FeatureId) {
    let statement = parse(source).expect(source);
    let observed = feature_walk(&statement)
        .into_iter()
        .map(|feature| feature.feature_id)
        .collect::<Vec<_>>();
    assert!(
        observed.contains(&expected),
        "{source} should record {expected:?}, observed {observed:?}"
    );
}

fn projection_type(analyzed: &AnalyzedStatement, name: &str) -> AnalyzedType {
    let Statement::Query(query) = analyzed.source() else {
        panic!("expected query statement");
    };
    let item = query
        .statements
        .iter()
        .filter_map(|statement| match statement {
            PipelineStatement::Return(clause) => Some(&clause.items),
            _ => None,
        })
        .flatten()
        .find(|item| {
            item.alias
                .clone()
                .is_some_and(|alias| alias.as_str() == name)
        })
        .unwrap_or_else(|| panic!("projection {name} exists"));
    let id = analyzed
        .expr_ids
        .get(&item.expr)
        .unwrap_or_else(|| panic!("projection {name} has an ExprId"));
    analyzed.expr_types.get(id).clone()
}

#[path = "byte_string_literals/catalog.rs"]
mod catalog;
#[path = "byte_string_literals/literals.rs"]
mod literals;
#[path = "byte_string_literals/operations.rs"]
mod operations;
#[path = "byte_string_literals/parse_errors.rs"]
mod parse_errors;
