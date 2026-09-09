//! Predicate truth values and forged-IR failures independent of expansion.
#![cfg(feature = "test-harness")]

mod exec_common;
use exec_common::planned;
use selene_core::Value;
use selene_gql::{
    Binding, BindingTable, EmptyProcedureRegistry, ExecutorError, TxContext, execute_pattern,
    execute_pipeline,
};
use selene_testing::mixed_orientation::MixedOrientationFixture;

fn evaluate(
    f: &MixedOrientationFixture,
    expression: &str,
    node: Value,
    edge: Value,
) -> Result<Value, ExecutorError> {
    let p = planned(&format!("MATCH (a)-[e]->() RETURN {expression} AS result"));
    let mut ctx = TxContext::read_only(
        f.graph.read(),
        &p.impl_defined_caps,
        &EmptyProcedureRegistry,
        f.graph.index_providers(),
    )
    .with_plan_metadata(&p.expr_ids, &p.subqueries);
    let schema = execute_pattern(p.pattern_plan.as_ref().unwrap(), &ctx)?
        .schema()
        .clone();
    let row = Binding::new(schema.columns.iter().map(
        |c| match c.name.as_ref().map(|n| n.as_str()) {
            Some("a") => node.clone(),
            Some("e") => edge.clone(),
            _ => Value::Null,
        },
    ));
    let table = execute_pipeline(&p.pipeline, BindingTable::new(schema, vec![row]), &mut ctx)?;
    Ok(table.rows()[0].values()[0].clone())
}

#[test]
fn endpoint_and_directed_predicates_follow_intrinsic_truth_table_and_not() {
    let f = MixedOrientationFixture::build();
    for (edge, node, expected) in [
        (0, 0, [true, true, false]),
        (0, 1, [true, false, true]),
        (1, 0, [true, false, true]),
        (1, 1, [true, true, false]),
        (2, 0, [false, false, false]),
        (2, 1, [false, false, false]),
        (3, 0, [false, false, false]),
        (3, 1, [false, false, false]),
        (4, 0, [true, true, true]),
        (5, 0, [false, false, false]),
        (0, 2, [true, false, false]),
    ] {
        for (i, predicate) in ["e IS DIRECTED", "a IS SOURCE OF e", "a IS DESTINATION OF e"]
            .iter()
            .enumerate()
        {
            for negated in [false, true] {
                let expression = if negated {
                    predicate.replace(" IS ", " IS NOT ")
                } else {
                    predicate.to_string()
                };
                assert_eq!(
                    evaluate(
                        &f,
                        &expression,
                        Value::NodeRef(f.nodes[node]),
                        Value::EdgeRef(f.edges[edge])
                    )
                    .unwrap(),
                    Value::Bool(expected[i] != negated),
                    "edge={edge} node={node} {expression}"
                );
            }
        }
    }
}

#[test]
fn null_operands_preserve_unknown_including_is_not() {
    let f = MixedOrientationFixture::build();
    for expression in [
        "a IS SOURCE OF e",
        "a IS NOT SOURCE OF e",
        "a IS DESTINATION OF e",
        "a IS NOT DESTINATION OF e",
    ] {
        for (node, edge) in [
            (Value::Null, Value::EdgeRef(f.edges[0])),
            (Value::Null, Value::EdgeRef(f.edges[2])),
            (Value::NodeRef(f.nodes[0]), Value::Null),
            (Value::Null, Value::Null),
        ] {
            assert_eq!(
                evaluate(&f, expression, node, edge).unwrap(),
                Value::Null,
                "{expression}"
            );
        }
    }
    for expression in ["e IS DIRECTED", "e IS NOT DIRECTED"] {
        assert_eq!(
            evaluate(&f, expression, Value::Null, Value::Null).unwrap(),
            Value::Null
        );
    }
}

#[test]
fn forged_runtime_operands_keep_existing_typed_data_exception() {
    let f = MixedOrientationFixture::build();
    for (expression, node, edge) in [
        ("e IS DIRECTED", Value::Null, Value::Int(1)),
        ("e IS DIRECTED", Value::Null, Value::NodeRef(f.nodes[0])),
        (
            "a IS SOURCE OF e",
            Value::Int(1),
            Value::EdgeRef(f.edges[2]),
        ),
        (
            "a IS DESTINATION OF e",
            Value::NodeRef(f.nodes[0]),
            Value::NodeRef(f.nodes[1]),
        ),
    ] {
        let error = evaluate(&f, expression, node, edge).unwrap_err();
        assert_eq!(error.gqlstatus().as_str(), "22G03", "{error}");
    }
}
