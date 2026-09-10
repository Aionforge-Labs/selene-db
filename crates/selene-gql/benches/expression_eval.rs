#![allow(missing_docs)]
//! Criterion benches for scalar expression execution throughput.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod common;
#[path = "expression_eval/structural.rs"]
mod structural;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use selene_core::{DbString, GraphId, JsonValue, Value, db_string};
use selene_gql::{
    EmptyProcedureRegistry, ExecutionPlan, Session, StatementOutput, analyze, execute_statement,
    parse, plan,
};
use selene_graph::SharedGraph;

struct ExpressionCase {
    group: &'static str,
    name: &'static str,
    source: &'static str,
    parameters: ParameterSet,
}

#[derive(Clone, Copy)]
enum ParameterSet {
    None,
    JsonPayload,
    JsonContains,
    JsonConstruct,
    JsonMergePatch,
    JsonPatch,
}

const CASES: &[ExpressionCase] = &[
    ExpressionCase {
        group: "predicate",
        name: "starts_with",
        source: "RETURN 'alphabet' STARTS WITH 'a' AS v",
        parameters: ParameterSet::None,
    },
    ExpressionCase {
        group: "predicate",
        name: "range",
        source: "RETURN 5 >= 1 AND 5 <= 10 AS v",
        parameters: ParameterSet::None,
    },
    ExpressionCase {
        group: "scalar_fn",
        name: "char_length",
        source: "RETURN char_length('alphabet') AS v",
        parameters: ParameterSet::None,
    },
    ExpressionCase {
        group: "scalar_fn",
        name: "abs",
        source: "RETURN abs(-42) AS v",
        parameters: ParameterSet::None,
    },
    ExpressionCase {
        group: "scalar_fn",
        name: "left",
        source: "RETURN left('alphabet', 4) AS v",
        parameters: ParameterSet::None,
    },
    ExpressionCase {
        group: "json",
        name: "parse_type",
        source: "RETURN json_type($payload) AS v",
        parameters: ParameterSet::JsonPayload,
    },
    ExpressionCase {
        group: "json",
        name: "nested_get_path_text",
        source: "RETURN json_get_path_text($payload, 'memory', 'facts', 1, 'title') AS v",
        parameters: ParameterSet::JsonPayload,
    },
    ExpressionCase {
        group: "json",
        name: "nested_get_path_scalar",
        source: "RETURN json_get_path_scalar($payload, 'memory', 'score') AS v",
        parameters: ParameterSet::JsonPayload,
    },
    ExpressionCase {
        group: "json",
        name: "has_path_miss",
        source: "RETURN json_has_path($payload, 'memory', 'facts', 0, 'missing') AS v",
        parameters: ParameterSet::JsonPayload,
    },
    ExpressionCase {
        group: "json",
        name: "contains_nested",
        source: "RETURN json_contains($payload, $candidate) AS v",
        parameters: ParameterSet::JsonContains,
    },
    ExpressionCase {
        group: "json",
        name: "construct_metadata",
        source: "RETURN json_object('memory', json_object('kind', $kind, 'current', $current, 'score', $score), 'tags', json_array($tag_a, $tag_b)) AS v",
        parameters: ParameterSet::JsonConstruct,
    },
    ExpressionCase {
        group: "json",
        name: "merge_patch_metadata",
        source: "RETURN json_merge_patch($payload, $patch) AS v",
        parameters: ParameterSet::JsonMergePatch,
    },
    ExpressionCase {
        group: "json",
        name: "patch_metadata",
        source: "RETURN json_patch($payload, $patch) AS v",
        parameters: ParameterSet::JsonPatch,
    },
    ExpressionCase {
        group: "case",
        name: "searched",
        source: "RETURN CASE WHEN false THEN 1 WHEN true THEN 2 ELSE 3 END AS v",
        parameters: ParameterSet::None,
    },
    ExpressionCase {
        group: "collection",
        name: "trim_tail",
        source: "RETURN trim([10, 20, 30], 1) AS v",
        parameters: ParameterSet::None,
    },
    ExpressionCase {
        group: "binary_op",
        name: "concat",
        source: "RETURN 'alpha' || 'beta' AS v",
        parameters: ParameterSet::None,
    },
    ExpressionCase {
        group: "binary_op",
        name: "starts_with",
        source: "RETURN 'alphabet' STARTS WITH 'alpha' AS v",
        parameters: ParameterSet::None,
    },
];

fn bench_expression_eval(c: &mut Criterion) {
    let empty = EmptyProcedureRegistry;
    let graph = SharedGraph::new(GraphId::new(116));
    let plans = CASES
        .iter()
        .map(|case| {
            (
                case,
                plan_expression(case.source),
                parameter_values(case.parameters),
            )
        })
        .collect::<Vec<_>>();

    let mut group = c.benchmark_group("gql_expression_eval");
    for (case, plan, parameters) in &plans {
        if case.group == "predicate" && case.name == "range" {
            let mut session = Session::new(&graph);
            let StatementOutput::Rows(table) = execute_statement(plan, &mut session, &empty)
                .expect("range correctness guard executes")
            else {
                panic!("range correctness guard must produce rows");
            };
            assert_eq!(table.row_count(), 1);
            assert_eq!(table.rows()[0].values(), &[Value::Bool(true)]);
        }
        group.bench_function(BenchmarkId::new(case.group, case.name), |b| {
            let mut session = Session::new(&graph);
            bind_parameters(&mut session, parameters);
            b.iter(|| {
                let output = execute_statement(std::hint::black_box(plan), &mut session, &empty)
                    .expect("expression statement executes");
                std::hint::black_box(output_rows(output));
            });
        });
    }
    group.finish();
}

fn plan_expression(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("expression bench source parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None)
        .expect("expression bench source analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("expression bench source plans")
}

fn parameter_values(set: ParameterSet) -> Vec<(DbString, Value)> {
    match set {
        ParameterSet::None => Vec::new(),
        ParameterSet::JsonPayload => vec![param(
            "payload",
            json(
                r#"{"memory":{"facts":[{"kind":"semantic","title":"old"},{"kind":"episodic","title":"current"}]},"kind":"episodic","score":7,"tags":["agent","graph"]}"#,
            ),
        )],
        ParameterSet::JsonContains => vec![
            param(
                "payload",
                json(
                    r#"{"memory":{"kind":"episodic","current":true,"score":7},"tags":["agent","graph"]}"#,
                ),
            ),
            param(
                "candidate",
                json(r#"{"memory":{"current":true},"tags":["graph"]}"#),
            ),
        ],
        ParameterSet::JsonConstruct => vec![
            param("kind", string("episodic")),
            param("current", Value::Bool(true)),
            param("score", Value::Int(7)),
            param("tag_a", string("agent")),
            param("tag_b", string("graph")),
        ],
        ParameterSet::JsonMergePatch => vec![
            param(
                "payload",
                json(
                    r#"{"memory":{"kind":"episodic","current":false,"score":7},"tags":["agent"]}"#,
                ),
            ),
            param(
                "patch",
                json(r#"{"memory":{"current":true,"source":"graph"},"tags":["agent","graph"]}"#),
            ),
        ],
        ParameterSet::JsonPatch => vec![
            param(
                "payload",
                json(
                    r#"{"memory":{"kind":"episodic","current":false,"score":7},"tags":["agent"]}"#,
                ),
            ),
            param(
                "patch",
                json(
                    r#"[{"op":"replace","path":"/memory/current","value":true},{"op":"add","path":"/memory/source","value":"graph"},{"op":"add","path":"/tags/-","value":"graph"}]"#,
                ),
            ),
        ],
    }
}

fn bind_parameters(session: &mut Session<'_>, parameters: &[(DbString, Value)]) {
    for (name, value) in parameters {
        session.bind_parameter(name.clone(), value.clone());
    }
}

fn param(name: &'static str, value: Value) -> (DbString, Value) {
    (
        db_string(name).expect("expression bench parameter name fits"),
        value,
    )
}

fn json(source: &str) -> Value {
    Value::Json(JsonValue::parse_str(source).expect("expression bench JSON parameter parses"))
}

fn string(source: &'static str) -> Value {
    Value::String(db_string(source).expect("expression bench string parameter fits"))
}

fn output_rows(output: StatementOutput) -> usize {
    match output {
        StatementOutput::Rows(table) => table.row_count(),
        StatementOutput::Written(outcome) => {
            outcome.rows.as_ref().map_or(0, |table| table.row_count())
        }
        StatementOutput::Empty => 0,
        _ => panic!("unexpected expression bench output"),
    }
}

criterion_group! {
    name = expression_eval_group;
    config = common::criterion_config();
    targets = bench_expression_eval, structural::bench_structural_types
}
criterion_main!(expression_eval_group);
