//! BRIEF-115 parameter binding integration tests.

use std::{collections::HashMap, num::NonZeroUsize, sync::Arc, thread};

use selene_core::{DbString, GraphId, LabelSet, PropertyMap, Value};
use selene_gql::{
    AnalysisError, AnalyzedStatement, AnalyzedType, DataExceptionSubclass, EmptyProcedureRegistry,
    ExecutionPlan, ExecutorError, ExpectedType, GqlStatus, GqlType, LimitValue, OptimizeContext,
    PipelineStatement, Session, Statement, StatementOutput, TypeMismatchContext, ValueExpr,
    analyze, execute_statement, optimize, parse, plan,
};
use selene_graph::SharedGraph;
use serde_json::Value as JsonValue;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn props<const N: usize>(pairs: [(DbString, Value); N]) -> PropertyMap {
    PropertyMap::from_pairs(pairs).expect("test properties fit caps")
}

fn execute(session: &mut Session<'_>, source: &str) -> Result<StatementOutput, ExecutorError> {
    session.execute_source(source, &EmptyProcedureRegistry)
}

fn planned(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("test source parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test source analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test source plans")
}

fn analyze_one(source: &str) -> AnalyzedStatement {
    let statement = parse(source).expect("test source parses");
    analyze(statement, &EmptyProcedureRegistry, None).expect("test source analyzes")
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
            PipelineStatement::With(clause) => Some(&clause.items),
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

fn optimized_plan(source: &str) -> ExecutionPlan {
    optimize(planned(source), &OptimizeContext::default())
}

fn rows(output: StatementOutput) -> selene_gql::BindingTable {
    match output {
        StatementOutput::Rows(table) => table,
        StatementOutput::Written(outcome) => outcome.rows.expect("write returned rows"),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn single_value(session: &mut Session<'_>, source: &str) -> Value {
    let table = rows(execute(session, source).expect("query succeeds"));
    assert_eq!(table.row_count(), 1);
    table.rows()[0].values()[0].clone()
}

fn sample_uuid_value() -> Value {
    Value::ALL
        .iter()
        .map(|make| make())
        .find(|value| matches!(value, Value::Uuid(_)))
        .expect("Value::ALL includes uuid")
}

fn graph_with_sensors(id: u64) -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(id));
    let sensor = db_string("Sensor");
    let id_key = db_string("id");
    let value_key = db_string("value");
    let name_key = db_string("name");
    let uuid_value = sample_uuid_value();
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        for index in 0_i64..5 {
            mutator
                .create_node(
                    LabelSet::single(sensor.clone()),
                    props([
                        (id_key.clone(), Value::Int(index)),
                        (value_key.clone(), Value::Int(index * 10)),
                        (
                            name_key.clone(),
                            Value::String(db_string(&format!("sensor-{index}"))),
                        ),
                    ]),
                )
                .expect("sensor inserts");
        }
        mutator
            .create_node(
                LabelSet::single(sensor.clone()),
                props([
                    (id_key.clone(), Value::String(db_string("string-id"))),
                    (value_key.clone(), Value::Int(60)),
                ]),
            )
            .expect("string-id sensor inserts");
        mutator
            .create_node(
                LabelSet::single(sensor),
                props([(id_key, uuid_value), (value_key, Value::Int(70))]),
            )
            .expect("uuid-id sensor inserts");
        txn.commit().expect("fixture commits");
    }
    graph
}

fn bind_json_params(session: &mut Session<'_>, params: HashMap<&str, JsonValue>) {
    for (name, value) in params {
        session.bind_parameter(db_string(name), json_to_value(value));
    }
}

fn json_to_value(value: JsonValue) -> Value {
    match value {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(value) => Value::Bool(value),
        JsonValue::Number(value) => {
            if let Some(value) = value.as_i64() {
                Value::Int(value)
            } else if let Some(value) = value.as_u64() {
                Value::Uint(value)
            } else {
                Value::Float(value.as_f64().expect("json number is finite"))
            }
        }
        JsonValue::String(value) => Value::String(db_string(&value)),
        JsonValue::Array(values) => Value::List(values.into_iter().map(json_to_value).collect()),
        JsonValue::Object(_) => Value::Null,
    }
}

#[path = "parameters/binding.rs"]
mod binding;
#[path = "parameters/cache_and_shapes.rs"]
mod cache_and_shapes;
#[path = "parameters/session.rs"]
mod session;
#[path = "parameters/typed.rs"]
mod typed;
