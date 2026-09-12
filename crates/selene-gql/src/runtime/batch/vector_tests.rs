//! Vector retrieval must finish in the typed physical prefix, not a row suffix.

use selene_core::{GraphId, Value};
use selene_graph::SharedGraph;

use super::{BatchPolicy, query::execute_with_test_policy};
use crate::{runtime::TxContext, *};

#[test]
fn vector_calls_preserve_typed_results_across_input_and_output_windows() {
    let graph = SharedGraph::new(GraphId::new(407));
    let registry = BuiltinProcedureRegistry::new();
    let mut session = Session::new(&graph);
    session.execute_source("INSERT (:Memory {embedding: CAST([1, 0] AS VECTOR)}), (:Memory {embedding: CAST([0, 1] AS VECTOR)}), (:Memory {embedding: CAST([0, -1] AS VECTOR)})", &registry).unwrap();
    session
        .execute_source(
            "CALL selene.create_vector_index('Memory', 'embedding', 2, 'hnsw', NULL, 'cosine')",
            &registry,
        )
        .unwrap();
    for (name, query, yields, return_values) in [
        (
            "vector_search_nodes",
            "CAST([1, 0] AS VECTOR)",
            "node_id, distance",
            "node_id, distance",
        ),
        (
            "vector_search_nodes_ann",
            "CAST([1, 0] AS VECTOR)",
            "node_id, distance",
            "node_id, distance",
        ),
        (
            "vector_search_nodes_batch",
            "[CAST([1, 0] AS VECTOR)]",
            "query_index, node_id, distance",
            "node_id, distance",
        ),
        (
            "vector_search_nodes_ann_batch",
            "[CAST([1, 0] AS VECTOR)]",
            "query_index, node_id, distance",
            "node_id, distance",
        ),
    ] {
        let source = format!(
            "MATCH (m:Memory) CALL selene.{name}('Memory', 'embedding', {query}, 3, 'cosine') YIELD {yields} RETURN {return_values}"
        );
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
                    .expect("vector call must be physically routed");
            assert_eq!(prefix.suffix_from, plan.pipeline.len());
            assert_eq!(prefix.table.rows().len(), 9);
            for chunk in prefix.table.rows().chunks_exact(3) {
                for (row, distance) in chunk.iter().zip([0.0, 1.0, 1.0]) {
                    assert!(
                        matches!(row.values(), [Value::NodeRef(_), Value::Float(d)] if *d == distance)
                    );
                }
            }
        }
    }
}
