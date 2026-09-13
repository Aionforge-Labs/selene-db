//! F04-PR09 batch-only entry paths and independent expected outcomes.

use super::{
    BatchBuffer, BatchPolicy, BindingBatch,
    fixtures::{person_graph, plan_source},
    query::execute_with_test_policy,
};
use crate::{
    EmptyProcedureRegistry, PipelineOp,
    runtime::{Binding, BindingTable, EvalCtx, TxContext},
};
use selene_core::{NodeId, Value};

#[test]
fn extensions_correlations_and_empty_results_have_complete_batch_entry_paths() {
    let graph = person_graph();
    for (source, expected) in [
        (
            "LET a = 2, b = a + 3 RETURN b, a",
            vec![vec![Value::Int(5), Value::Int(2)]],
        ),
        (
            "FOR n IN [1, 1, 2] RETURN n",
            vec![
                vec![Value::Int(1)],
                vec![Value::Int(1)],
                vec![Value::Int(2)],
            ],
        ),
        ("FOR n IN [] RETURN n", vec![]),
        ("RETURN NULL AS n", vec![vec![Value::Null]]),
        (
            "CALL { RETURN 1 AS n } YIELD n RETURN n",
            vec![vec![Value::Int(1)]],
        ),
        (
            "OPTIONAL CALL { RETURN 1 AS n LIMIT 0 } YIELD n RETURN n",
            vec![vec![Value::Null]],
        ),
        (
            "FOR n IN [2, 3] RETURN n NEXT LET m = n + 1 RETURN m",
            vec![vec![Value::Int(3)], vec![Value::Int(4)]],
        ),
        (
            "RETURN 1 AS n NEXT RETURN 2 AS n",
            vec![vec![Value::Int(2)]],
        ),
    ] {
        let plan = plan_source(source);
        let ctx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            graph.index_providers(),
        );
        for size in [1, 2, 7, 1024] {
            let table =
                execute_with_test_policy(&plan, &ctx, BatchPolicy::new(size, 1 << 20).unwrap())
                    .unwrap_or_else(|error| panic!("{source}: {error:?}"));
            assert_eq!(
                super::collect_rows(&table),
                expected,
                "{source}, size {size}"
            );
            assert!(
                !table.schema().columns.is_empty(),
                "empty results retain declared columns"
            );
        }
    }
}

#[test]
fn mutation_sites_follow_selections_pages_and_zero_column_calls() {
    let graph = person_graph();
    let registry = crate::BuiltinProcedureRegistry::new();
    let analyzed = crate::analyze(
        crate::parse("FILTER true LIMIT 2 OFFSET 1 CALL selene.health() YIELD graph_id").unwrap(),
        &registry,
        None,
    )
    .unwrap();
    let plan = crate::plan(&analyzed, &registry).unwrap();
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &registry,
        graph.index_providers(),
    );
    let rows: Vec<_> = (1..=3)
        .map(|id| {
            Binding::with_insert_sites(
                [],
                [(crate::InsertSiteId::new(0), NodeId::new(id))]
                    .into_iter()
                    .collect(),
            )
        })
        .collect();
    let table = super::query::execute_pipeline(
        &plan.pipeline,
        BindingTable::new(crate::BindingTableSchema { columns: vec![] }, rows),
        &mut ctx,
        &plan.expr_ids,
        &plan.subqueries,
        BatchPolicy::new(1, 4096).unwrap(),
    )
    .unwrap();
    assert_eq!(table.row_count(), 2);
    assert_eq!(table.rows()[0].insert_sites()[0].1, NodeId::new(2));
    assert_eq!(table.rows()[1].insert_sites()[0].1, NodeId::new(3));

    let rows: Vec<_> = (1..=3)
        .map(|id| {
            Binding::with_insert_sites(
                [Value::Int(id)],
                [(crate::InsertSiteId::new(0), NodeId::new(id as u64))]
                    .into_iter()
                    .collect(),
            )
        })
        .collect();
    let plan = plan_source("RETURN 1 AS n");
    let mut batch = BindingBatch::from_columns(
        plan.output_schema,
        vec![vec![Value::Int(1), Value::Int(2), Value::Int(3)]],
    )
    .unwrap()
    .with_binding_sites(&rows)
    .unwrap();
    let mut buffer = BatchBuffer::new();
    batch.select(&[true, false, true], &mut buffer).unwrap();
    batch.select(&[false, true], &mut buffer).unwrap();
    assert_eq!(batch.logical_binding(0).insert_sites()[0].1, NodeId::new(3));
}

#[test]
fn extension_errors_and_budget_failures_never_publish_partial_output() {
    let graph = person_graph();
    let plan = plan_source("FOR n IN [1, 0] LET x = 1 / n RETURN x LIMIT 1");
    let ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );
    for size in [1, 2, 1024] {
        assert!(
            execute_with_test_policy(&plan, &ctx, BatchPolicy::new(size, 4096).unwrap()).is_err()
        );
    }
    for source in [
        "FOR n IN [1, 0] RETURN 1 / n AS value LIMIT 1",
        "FOR n IN [1, 0] FILTER 1 / n > 0 RETURN n LIMIT 1",
        "FOR n IN [0] RETURN 1 / n AS value LIMIT 0",
    ] {
        let plan = plan_source(source);
        for size in [1, 2, 1024] {
            assert!(
                execute_with_test_policy(&plan, &ctx, BatchPolicy::new(size, 4096).unwrap())
                    .is_err(),
                "{source}: policy {size} suppressed a preceding error"
            );
        }
    }
    let plan = plan_source("FOR n IN [1, 2, 3] RETURN n");
    let eval = EvalCtx {
        tx: &ctx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let mut operator = super::extend::BatchExtend::new(
        Box::new(super::unit::BatchSeedRow::unit()),
        &plan.pipeline[0],
        eval,
        BatchPolicy::new(1, 4096).unwrap(),
    )
    .unwrap();
    let mut exec = super::BatchExecutionContext::borrowed(
        ctx.snapshot(),
        ctx.batch_cancel(),
        super::MemoryBudget::new(1),
    );
    let error = super::tracer::trace_operator_to_table(&mut operator, &mut exec).unwrap_err();
    assert_eq!(error.gqlstatus().as_str(), "5GQL1");
    assert_eq!(exec.budget_used(), 0);
}

#[test]
fn read_only_authorization_happens_before_empty_input() {
    let graph = person_graph();
    for source in ["INSERT (:N)", "CREATE NODE TYPE :N ()"] {
        let plan = plan_source(source);
        let ctx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            graph.index_providers(),
        );
        let empty = BindingTable::new(crate::BindingTableSchema { columns: vec![] }, vec![]);
        let error = super::query::execute_read_only(
            &plan,
            Some(empty),
            &ctx,
            BatchPolicy::default_policy(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            crate::ExecutorError::InvalidTransactionState { .. }
        ));
    }
    let mut plan = plan_source("MATCH (n) RETURN n");
    let pattern = plan.pattern_plan.as_mut().unwrap();
    let inner = pattern.join_tree.clone();
    pattern.join_tree = crate::JoinTree::WorstCaseOptimal {
        intersection: vec![inner],
        node_id_ordering: vec![],
    };
    let ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );
    assert_eq!(
        execute_with_test_policy(&plan, &ctx, BatchPolicy::default_policy())
            .unwrap()
            .row_count(),
        10
    );
    assert!(matches!(plan.pipeline[0], PipelineOp::Project(_)));
}
