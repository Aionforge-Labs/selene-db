//! Text/JSON calls finish in typed physical batches for every policy window.

use super::{BatchPolicy, query::execute_with_test_policy};
use crate::{runtime::TxContext, *};
use selene_core::{GraphId, Value};
use selene_graph::SharedGraph;

#[test]
fn text_json_calls_have_no_row_suffix_and_preserve_result_schemas() {
    let graph = SharedGraph::new(GraphId::new(408));
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.execute_source("INSERT (:Memory {body: 'memory', payload: CAST('{\"v\":null}' AS JSON)}), (:Memory {body: 'memory', payload: CAST('{\"v\":1}' AS JSON)})", &registry).unwrap();
    session
        .execute_source("CALL selene.create_text_index('Memory', 'body')", &registry)
        .unwrap();
    for (call, yields, expected, json) in [
        (
            "text_search_nodes('Memory', 'body', 'memory', 9)",
            "node_id, score",
            4,
            false,
        ),
        (
            "text_score_nodes('Memory', 'body', 'memory', [m], 9)",
            "node_id, score",
            2,
            false,
        ),
        (
            "text_score_nodes_batch('Memory', 'body', ['memory', 'memory'], [[m], [m]], 9)",
            "query_index, node_id, score",
            4,
            false,
        ),
        (
            "json_path_value_nodes('Memory', 'payload', CAST('[\"v\"]' AS JSON), 9)",
            "node_id, value",
            4,
            true,
        ),
        (
            "json_path_value_candidate_nodes('Memory', 'payload', CAST('[\"v\"]' AS JSON), [m], 9)",
            "node_id, value",
            2,
            true,
        ),
    ] {
        let source = format!("MATCH (m:Memory) CALL selene.{call} YIELD {yields} RETURN {yields}");
        let analyzed = analyze(parse(&source).unwrap(), &registry, None).unwrap();
        let plan = plan(&analyzed, &registry).unwrap();
        let ctx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &registry,
            graph.index_providers(),
        );
        for size in [1, 2, 3, 7, 1024] {
            let prefix =
                execute_with_test_policy(&plan, &ctx, BatchPolicy::new(size, 1 << 20).unwrap())
                    .unwrap()
                    .expect("physical CALL");
            assert_eq!(prefix.suffix_from, plan.pipeline.len());
            assert_eq!(prefix.table.rows().len(), expected);
            for row in prefix.table.rows() {
                match row.values() {
                    [Value::NodeRef(_), Value::Json(_)] if json => {}
                    [Value::NodeRef(_), Value::Float(v)]
                    | [Value::Uint(_), Value::NodeRef(_), Value::Float(v)]
                        if !json =>
                    {
                        assert!(*v > 0.0)
                    }
                    other => panic!("wrong schema: {other:?}"),
                }
            }
        }
    }
}
