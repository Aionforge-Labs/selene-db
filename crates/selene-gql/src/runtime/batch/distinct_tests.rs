//! F04-PR04 deduplication/trim kernel acceptance.
//!
//! Each test maps to one slice requirement over hand-built rows, so
//! expectations are derived by hand, not copied from either engine:
//!
//! - distinct keeps first occurrences (nulls deduped, cross-type numerics
//!   collapsed) and matches the independent oracle;
//! - carrier trimming truncates positionally with the row path's no-op
//!   rule;
//! - the materializing operators report lifecycle and pull counts.
//!
//! Sorting and top-K kernels live in [`super::sort_tests`]. Facade parity
//! against the row oracle lives in [`super::aggregate_differentials`].

use selene_core::{GraphId, Value};
use selene_graph::SharedGraph;

use crate::{
    EmptyProcedureRegistry, OrderDirection,
    plan::BindingTableSchema,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, TxContext},
};

use super::distinct::{BatchDistinct, distinct_rows};
use super::fixtures::{
    assert_kernel_rows, kernel_column, kernel_ctx, kernel_order_key, pair, pair_schema,
};
use super::relation_model::model_distinct;
use super::sort::{BatchSort, BatchTopK, BatchTrimCarriers};
use super::tracer::trace_operator_to_table;
use super::unit::BatchRowSource;
use super::{BatchPolicy, MemoryBudget, OperatorState, PhysicalOperator};

/// Batch sizes for the boundary matrix: single-row pulls split every
/// duplicate run; the default covers steady state.
fn policies() -> Vec<BatchPolicy> {
    [1usize, 2, 3, 1024]
        .into_iter()
        .map(|target| BatchPolicy::new(target, 1 << 20).unwrap())
        .collect()
}

/// Build the detached kernel environment most dedup tests share.
///
/// Everything borrows from test-scope locals; keep the graph, caps, and
/// registries bound in the test body while the returned context is in use.
/// Expands to `tx`, `eval` bindings plus their backing locals.
macro_rules! kernel_setup {
    ($graph:expr, $caps:expr, $expr_ids:expr, $subqueries:expr, $tx:ident, $eval:ident) => {
        let $tx = TxContext::read_only(
            $graph.read(),
            &$caps,
            &EmptyProcedureRegistry,
            $graph.index_providers(),
        );
        let $eval = EvalCtx {
            tx: &$tx,
            expr_ids: &$expr_ids,
            subqueries: &$subqueries,
        };
    };
}

/// Run one distinct kernel over hand-built rows.
fn run_distinct(
    schema: BindingTableSchema,
    rows: Vec<Binding>,
    policy: BatchPolicy,
    budget: MemoryBudget,
) -> Result<BindingTable, ExecutorError> {
    let table = BindingTable::new(schema, rows);
    let source = BatchRowSource::new(table, policy);
    let mut op = BatchDistinct::new(Box::new(source), policy);
    let mut ctx = kernel_ctx(budget);
    trace_operator_to_table(&mut op, &mut ctx)
}

#[test]
fn distinct_keeps_first_occurrences() {
    // First-seen rows survive in input order; null rows dedupe (null
    // equals null in the distinctness regime) and cross-type numerics
    // collapse.
    let rows = vec![
        pair(Value::Int(1), Value::Int(10)),
        pair(Value::Null, Value::Null),
        pair(Value::Int(1), Value::Int(10)),
        pair(Value::Float(1.0), Value::Float(10.0)),
        pair(Value::Null, Value::Null),
        pair(Value::Int(2), Value::Int(20)),
    ];
    for policy in policies() {
        let table = run_distinct(
            pair_schema(),
            rows.clone(),
            policy,
            MemoryBudget::unlimited(),
        )
        .expect("distinct runs");
        assert_kernel_rows(
            &table,
            &[
                vec![Value::Int(1), Value::Int(10)],
                vec![Value::Null, Value::Null],
                vec![Value::Int(2), Value::Int(20)],
            ],
            "first occurrences survive",
        );
        assert_eq!(
            table.schema(),
            &pair_schema(),
            "distinct keeps the child schema"
        );
    }
}

#[test]
fn distinct_matches_independent_oracle() {
    // The native kernel and the separately written first-seen model agree
    // on a mixed fixture (cross-type numerics plus nulls, one family per
    // column).
    let rows = vec![
        vec![Value::Int(1), Value::Null],
        vec![Value::Float(1.0), Value::Null],
        vec![Value::Null, Value::Int(2)],
        vec![Value::Int(1), Value::Null],
        vec![Value::Int(2), Value::Null],
        vec![Value::Null, Value::Int(2)],
        vec![Value::Float(2.0), Value::Int(10)],
        vec![Value::Int(2), Value::Int(10)],
    ];
    let expected = model_distinct(&rows);
    assert_eq!(expected.len(), 4, "oracle dedups to four rows");
    let bindings = rows.into_iter().map(Binding::new).collect::<Vec<_>>();
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    let (deduped, reserved) = distinct_rows(bindings, 2, &mut ctx).expect("native distinct runs");
    ctx.budget_mut().release(reserved);
    let actual = deduped
        .iter()
        .map(|row| row.values().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "native kernel matches the oracle");
}

#[test]
fn trim_carriers_truncates_positionally() {
    // Carriers appended after the projected columns drop positionally; a
    // width at or beyond the schema is a no-op, never an error.
    let schema = BindingTableSchema {
        columns: vec![kernel_column("a"), kernel_column("b"), kernel_column("c")],
    };
    let rows = vec![
        Binding::new([Value::Int(1), Value::Int(2), Value::Int(3)]),
        Binding::new([Value::Int(4), Value::Int(5), Value::Int(6)]),
    ];
    for policy in policies() {
        let table = BindingTable::new(schema.clone(), rows.clone());
        let mut op = BatchTrimCarriers::new(Box::new(BatchRowSource::new(table, policy)), 2);
        let mut ctx = kernel_ctx(MemoryBudget::unlimited());
        let trimmed = trace_operator_to_table(&mut op, &mut ctx).expect("trim runs");
        assert_eq!(trimmed.schema().columns.len(), 2);
        assert_kernel_rows(
            &trimmed,
            &[
                vec![Value::Int(1), Value::Int(2)],
                vec![Value::Int(4), Value::Int(5)],
            ],
            "carriers drop",
        );
        // No-op width keeps every column.
        let table = BindingTable::new(schema.clone(), rows.clone());
        let mut op = BatchTrimCarriers::new(Box::new(BatchRowSource::new(table, policy)), 7);
        let mut ctx = kernel_ctx(MemoryBudget::unlimited());
        let kept = trace_operator_to_table(&mut op, &mut ctx).expect("wide trim runs");
        assert_eq!(kept.schema().columns.len(), 3);
        assert_eq!(kept.row_count(), 2);
    }
}

#[test]
fn materializing_operators_report_lifecycle_and_counts() {
    // Direct BatchSort/BatchTopK/BatchDistinct/BatchTrimCarriers over a
    // two-row policy: Created -> Open -> Exhausted -> Closed with exact
    // pull counts and declared schemas.
    let graph = SharedGraph::new(GraphId::new(45_012));
    let caps = crate::plan::ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Int(2), Value::Int(20)),
        pair(Value::Int(1), Value::Int(10)),
        pair(Value::Int(3), Value::Int(30)),
    ];
    let keys = vec![kernel_order_key("k", 1, OrderDirection::Asc, None)];
    let policy = BatchPolicy::new(2, 1 << 20).unwrap();
    let mut buffer = super::BatchBuffer::new();
    // Full sort over three rows splits across two pulls the same way.
    let mut sort = BatchSort::new(
        Box::new(BatchRowSource::new(
            BindingTable::new(pair_schema(), rows.clone()),
            policy,
        )),
        &keys,
        eval,
        policy,
    );
    assert_eq!(sort.state(), OperatorState::Created);
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    sort.init(&mut ctx).expect("sort inits");
    let mut pulled = 0;
    while sort
        .next_batch(&mut ctx, &mut buffer)
        .expect("pulls succeed")
        .is_some()
    {
        pulled += 1;
    }
    assert_eq!(pulled, 2, "three rows split across two pulls");
    assert_eq!(sort.batches_produced(), 2);
    assert_eq!(sort.state(), OperatorState::Exhausted);
    sort.close(&mut ctx);
    assert_eq!(sort.state(), OperatorState::Closed);
    // Top-K keeps the full three-row window across two pulls.
    let mut top_k = BatchTopK::new(
        Box::new(BatchRowSource::new(
            BindingTable::new(pair_schema(), rows.clone()),
            policy,
        )),
        &keys,
        0,
        3,
        eval,
        policy,
    );
    assert_eq!(top_k.state(), OperatorState::Created);
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    top_k.init(&mut ctx).expect("top-k inits");
    assert_eq!(top_k.state(), OperatorState::Open);
    let mut pulled = 0;
    while top_k
        .next_batch(&mut ctx, &mut buffer)
        .expect("pulls succeed")
        .is_some()
    {
        pulled += 1;
    }
    assert_eq!(pulled, 2, "three rows split across two pulls");
    assert_eq!(top_k.batches_produced(), 2);
    assert_eq!(top_k.state(), OperatorState::Exhausted);
    top_k.close(&mut ctx);
    assert_eq!(top_k.state(), OperatorState::Closed);
    // Distinct over a duplicated row keeps two rows in one pull.
    let dupes = vec![
        pair(Value::Int(1), Value::Int(10)),
        pair(Value::Int(1), Value::Int(10)),
        pair(Value::Int(2), Value::Int(20)),
    ];
    let mut distinct = BatchDistinct::new(
        Box::new(BatchRowSource::new(
            BindingTable::new(pair_schema(), dupes),
            policy,
        )),
        policy,
    );
    assert_eq!(distinct.state(), OperatorState::Created);
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    distinct.init(&mut ctx).expect("distinct inits");
    let mut pulled = 0;
    let mut kept = 0;
    while let Some(batch) = distinct
        .next_batch(&mut ctx, &mut buffer)
        .expect("pulls succeed")
    {
        pulled += 1;
        kept += batch.logical_rows();
        ctx.budget_mut().release(batch.estimated_bytes());
        batch.recycle(&mut buffer);
    }
    assert_eq!((pulled, kept), (1, 2), "two rows survive in one pull");
    assert_eq!(distinct.batches_produced(), 1);
    assert_eq!(distinct.state(), OperatorState::Exhausted);
    distinct.close(&mut ctx);
    assert_eq!(distinct.state(), OperatorState::Closed);
    // Trim streams one batch per child batch with the truncated schema.
    let mut trim = BatchTrimCarriers::new(
        Box::new(BatchRowSource::new(
            BindingTable::new(pair_schema(), rows),
            policy,
        )),
        1,
    );
    assert_eq!(trim.state(), OperatorState::Created);
    assert_eq!(trim.output_schema().columns.len(), 1);
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    trim.init(&mut ctx).expect("trim inits");
    assert_eq!(trim.state(), OperatorState::Open);
    let mut pulled = 0;
    while trim
        .next_batch(&mut ctx, &mut buffer)
        .expect("pulls succeed")
        .is_some()
    {
        pulled += 1;
    }
    assert_eq!(pulled, 2, "trim streams child batches");
    assert_eq!(trim.state(), OperatorState::Exhausted);
    trim.close(&mut ctx);
    assert_eq!(trim.state(), OperatorState::Closed);
}
