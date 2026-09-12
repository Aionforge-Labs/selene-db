//! F04-PR04 grouping/aggregation kernel acceptance.
//!
//! Each test maps to one slice requirement over hand-built `(k, v)` rows,
//! so expectations are derived by hand, not copied from either engine:
//!
//! - empty ungrouped aggregation, empty grouped input, and one all-null
//!   group yield their three distinct required results;
//! - `COUNT(*)`, `COUNT(value)`, and `DISTINCT` aggregates exercise
//!   separate paths, with duplicate groups across batch boundaries sharing
//!   one state;
//! - numeric overflow promotes (`i64` to `i128`) then fails typed, and
//!   signed-zero/NaN keys group per the documented not-distinct regime
//!   while incompatible key families error;
//! - memory exhaustion and cancellation fail typed without presenting a
//!   truncated-complete group set;
//! - lifecycle, output schemas, and batch splitting are observable.
//!
//! Facade parity against the row oracle and the independent relation model
//! lives in [`super::aggregate_differentials`].

use selene_core::{CancellationToken, GraphId, Value, db_string};
use selene_graph::SharedGraph;

use crate::{
    Aggregate, EmptyProcedureRegistry, ProjectExpr,
    plan::ImplDefinedCaps,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, TxContext},
};

use super::aggregate::BatchGroupBy;
use super::fixtures::{assert_kernel_rows, kernel_agg, kernel_ctx, kernel_key, pair, pair_schema};
use super::relation_model::{assert_same_multiset, groups_of};
use super::tracer::trace_operator_to_table;
use super::unit::BatchRowSource;
use super::{
    BatchCancel, BatchExecutionContext, BatchPolicy, MemoryBudget, OperatorState, PhysicalOperator,
};

/// Batch sizes for the boundary matrix: single-row pulls force every
/// duplicate group across a pull boundary; the default covers steady state.
fn policies() -> Vec<BatchPolicy> {
    [1usize, 2, 3, 1024]
        .into_iter()
        .map(|t| BatchPolicy::new(t, 1 << 20).unwrap())
        .collect()
}

/// Build the detached kernel environment most grouping tests share.
///
/// Everything borrows from test-scope locals, so keep the graph, caps, and
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

/// Run one grouping kernel over hand-built rows, returning the materialized
/// table (schema included, even at zero rows).
fn run_kernel(
    rows: Vec<Binding>,
    keys: &[ProjectExpr],
    aggregates: &[Aggregate],
    eval: EvalCtx<'_, '_, '_, '_>,
    policy: BatchPolicy,
    budget: MemoryBudget,
) -> Result<BindingTable, ExecutorError> {
    let table = BindingTable::new(pair_schema(), rows);
    let source = BatchRowSource::new(table, policy);
    let mut op = BatchGroupBy::new(Box::new(source), keys, aggregates, eval, policy);
    let mut ctx = kernel_ctx(budget);
    trace_operator_to_table(&mut op, &mut ctx)
}

#[test]
fn empty_ungrouped_aggregation_yields_specified_results() {
    // The three empty-input shapes must stay distinct: ungrouped over empty
    // yields one row with the specified per-function results (COUNT zero,
    // SUM zero, AVG/MIN/MAX null, COLLECT empty).
    let graph = SharedGraph::new(GraphId::new(44_001));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let aggregates = vec![
        kernel_agg("count", None, true, false, 1),
        kernel_agg("count", Some("v"), false, false, 2),
        kernel_agg("sum", Some("v"), false, false, 3),
        kernel_agg("avg", Some("v"), false, false, 4),
        kernel_agg("min", Some("v"), false, false, 5),
        kernel_agg("max", Some("v"), false, false, 6),
        kernel_agg("collect_list", Some("v"), false, false, 7),
    ];
    let expected = vec![vec![
        Value::Null,
        Value::Null,
        Value::Int(0),
        Value::Int(0),
        Value::Int(0),
        Value::Null,
        Value::Null,
        Value::Null,
        Value::List(Vec::new()),
    ]];
    for policy in policies() {
        let table = run_kernel(
            Vec::new(),
            &[],
            &aggregates,
            eval,
            policy,
            MemoryBudget::unlimited(),
        )
        .expect("empty ungrouped groups");
        assert_kernel_rows(&table, &expected, "empty ungrouped");
        assert_eq!(
            table.schema().columns.len(),
            2 + aggregates.len(),
            "empty ungrouped keeps the full descriptor"
        );
        assert_eq!(
            table
                .schema()
                .columns
                .iter()
                .skip(2)
                .map(|column| column.name.clone())
                .collect::<Vec<_>>(),
            aggregates
                .iter()
                .map(|aggregate| Some(aggregate.output_name.clone()))
                .collect::<Vec<_>>(),
            "aggregate columns keep discovery order"
        );
    }
}

#[test]
fn empty_grouped_input_yields_no_rows_with_full_schema() {
    // Grouped aggregation over an empty input yields no rows (never the
    // ungrouped single row), but the descriptor still carries the input
    // columns plus the aggregate columns.
    let graph = SharedGraph::new(GraphId::new(44_002));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let keys = vec![kernel_key("k", 1)];
    let aggregates = vec![kernel_agg("count", None, true, false, 2)];
    for policy in policies() {
        let table = run_kernel(
            Vec::new(),
            &keys,
            &aggregates,
            eval,
            policy,
            MemoryBudget::unlimited(),
        )
        .expect("empty grouped groups");
        assert_eq!(table.row_count(), 0, "empty grouped yields no rows");
        assert_eq!(
            table.schema().columns.len(),
            3,
            "empty grouped keeps input plus aggregate columns"
        );
    }
}

#[test]
fn all_null_keys_form_one_group() {
    // Nulls-together is grouping equivalence, not a comparison predicate:
    // every all-null key lands in one group even with differing values.
    let graph = SharedGraph::new(GraphId::new(44_003));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Null, Value::Int(1)),
        pair(Value::Null, Value::Int(2)),
        pair(Value::Null, Value::Null),
    ];
    let keys = vec![kernel_key("k", 1)];
    let aggregates = vec![
        kernel_agg("count", None, true, false, 2),
        kernel_agg("count", Some("v"), false, false, 3),
    ];
    // The representative is the first member's values; COUNT(*) sees three
    // rows while COUNT(v) eliminates the null.
    let expected = vec![vec![
        Value::Null,
        Value::Int(1),
        Value::Int(3),
        Value::Int(2),
    ]];
    for policy in policies() {
        let table = run_kernel(
            rows.clone(),
            &keys,
            &aggregates,
            eval,
            policy,
            MemoryBudget::unlimited(),
        )
        .expect("null keys group");
        assert_kernel_rows(&table, &expected, "all-null group");
    }
}

#[test]
fn count_star_count_value_and_distinct_take_separate_paths() {
    // COUNT(*) counts rows, COUNT(v) skips nulls, COUNT(DISTINCT v)
    // deduplicates first: one fixture exercises all three paths with
    // hand-derived per-group results.
    let graph = SharedGraph::new(GraphId::new(44_004));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let a = Value::String(db_string("a").unwrap());
    let b = Value::String(db_string("b").unwrap());
    let rows = vec![
        pair(a.clone(), Value::Int(1)),
        pair(a.clone(), Value::Null),
        pair(a.clone(), Value::Int(1)),
        pair(b.clone(), Value::Int(2)),
    ];
    let keys = vec![kernel_key("k", 1)];
    let aggregates = vec![
        kernel_agg("count", None, true, false, 2),
        kernel_agg("count", Some("v"), false, false, 3),
        kernel_agg("count", Some("v"), false, true, 4),
    ];
    let expected = vec![
        vec![
            a.clone(),
            Value::Int(1),
            Value::Int(3),
            Value::Int(2),
            Value::Int(1),
        ],
        vec![
            b.clone(),
            Value::Int(2),
            Value::Int(1),
            Value::Int(1),
            Value::Int(1),
        ],
    ];
    for policy in policies() {
        let table = run_kernel(
            rows.clone(),
            &keys,
            &aggregates,
            eval,
            policy,
            MemoryBudget::unlimited(),
        )
        .expect("counts group");
        assert_kernel_rows(&table, &expected, "count paths");
    }
}

#[test]
fn distinct_dedups_before_accumulating_across_types() {
    // SUM(DISTINCT v) over [1, 1.0, 2] is 3, not 4: the integer and the
    // float collapse under runtime equality before accumulation. One
    // ungrouped group keeps the first row as its representative.
    let graph = SharedGraph::new(GraphId::new(44_005));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let x = Value::String(db_string("x").unwrap());
    let rows = vec![
        pair(x.clone(), Value::Int(1)),
        pair(x.clone(), Value::Float(1.0)),
        pair(x.clone(), Value::Int(2)),
    ];
    let aggregates = vec![
        kernel_agg("sum", Some("v"), false, true, 1),
        kernel_agg("count", Some("v"), false, true, 2),
        kernel_agg("collect_list", Some("v"), false, true, 3),
    ];
    let expected = vec![vec![
        x,
        Value::Int(1),
        Value::Int(3),
        Value::Int(2),
        Value::List(vec![Value::Int(1), Value::Int(2)]),
    ]];
    for policy in policies() {
        let table = run_kernel(
            rows.clone(),
            &[],
            &aggregates,
            eval,
            policy,
            MemoryBudget::unlimited(),
        )
        .expect("distinct aggregates");
        assert_kernel_rows(&table, &expected, "distinct dedups across types");
    }
}

#[test]
fn duplicate_groups_across_batches_share_one_state() {
    // Single-row pulls force every duplicate key across a pull boundary;
    // the shared state still yields one row per group with full sums.
    let graph = SharedGraph::new(GraphId::new(44_006));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let a = Value::String(db_string("a").unwrap());
    let b = Value::String(db_string("b").unwrap());
    let rows = vec![
        pair(a.clone(), Value::Int(1)),
        pair(b.clone(), Value::Int(10)),
        pair(a.clone(), Value::Int(2)),
        pair(b.clone(), Value::Int(20)),
        pair(a.clone(), Value::Int(3)),
    ];
    let keys = vec![kernel_key("k", 1)];
    let aggregates = vec![kernel_agg("sum", Some("v"), false, false, 2)];
    let expected = vec![
        vec![a, Value::Int(1), Value::Int(6)],
        vec![b, Value::Int(10), Value::Int(30)],
    ];
    for policy in policies() {
        let table = run_kernel(
            rows.clone(),
            &keys,
            &aggregates,
            eval,
            policy,
            MemoryBudget::unlimited(),
        )
        .expect("duplicates group");
        assert_kernel_rows(&table, &expected, "duplicates share state");
    }
    // The independent grouping oracle agrees on the partition: two groups
    // with three and two members in first-emission order.
    let plain = rows
        .iter()
        .map(|row| row.values().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(groups_of(&plain, 1), vec![vec![0, 2, 4], vec![1, 3]]);
}

#[test]
fn integer_sum_promotes_then_overflows_typed() {
    // i64 overflow widens to i128 rather than failing; i128 overflow is a
    // typed numeric data exception, never a wrap.
    let graph = SharedGraph::new(GraphId::new(44_007));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let aggregates = vec![kernel_agg("sum", Some("v"), false, false, 1)];
    let wide = run_kernel(
        vec![
            pair(Value::Int(1), Value::Int(i64::MAX)),
            pair(Value::Int(2), Value::Int(1)),
        ],
        &[],
        &aggregates,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect("i64 overflow widens");
    assert_kernel_rows(
        &wide,
        &[vec![
            Value::Int(1),
            Value::Int(i64::MAX),
            Value::Int128(i128::from(i64::MAX) + 1),
        ]],
        "sum widens to i128",
    );
    let err = run_kernel(
        vec![
            pair(Value::Int(1), Value::Int128(i128::MAX)),
            pair(Value::Int(2), Value::Int(1)),
        ],
        &[],
        &aggregates,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect_err("i128 overflow must fail");
    assert!(
        matches!(err, ExecutorError::DataException { .. }),
        "overflow is a data exception, got {err:?}"
    );
}

#[test]
fn signed_zero_and_nan_keys_group_per_regime() {
    // Signed zeros share one not-distinct group; all NaN payloads share
    // one; cross-type numerics collapse. Documented in the core numeric
    // regime, proven here through the batch kernel.
    let graph = SharedGraph::new(GraphId::new(44_008));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let keys = vec![kernel_key("k", 1)];
    let aggregates = vec![kernel_agg("count", None, true, false, 2)];
    let grouped = run_kernel(
        vec![
            pair(Value::Float(0.0), Value::Int(1)),
            pair(Value::Float(-0.0), Value::Int(2)),
            pair(Value::Int(1), Value::Int(3)),
            pair(Value::Float(1.0), Value::Int(4)),
            pair(Value::Int128(1), Value::Int(5)),
        ],
        &keys,
        &aggregates,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect("numeric keys group");
    assert_kernel_rows(
        &grouped,
        &[
            vec![Value::Float(0.0), Value::Int(1), Value::Int(2)],
            vec![Value::Int(1), Value::Int(3), Value::Int(3)],
        ],
        "signed zeros and cross-type numerics collapse",
    );
    let nan_grouped = run_kernel(
        vec![
            pair(Value::Float(f64::NAN), Value::Int(1)),
            pair(
                Value::Float(f64::from_bits(0x7ff8_0000_0000_0001)),
                Value::Int(2),
            ),
        ],
        &keys,
        &aggregates,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect("NaN keys group");
    assert_eq!(
        nan_grouped.row_count(),
        1,
        "all NaN payloads share one group"
    );
    assert_eq!(
        nan_grouped.rows()[0].values()[2],
        Value::Int(2),
        "NaN group counts both members"
    );
}

#[test]
fn nan_aggregates_follow_selected_float_rules() {
    // SUM over NaN is a typed range error (non-finite intermediate), while
    // MIN/MAX order NaNs high per the deterministic numeric sort: min
    // skips past NaN to 1.0, max keeps NaN.
    let graph = SharedGraph::new(GraphId::new(44_009));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Int(1), Value::Float(f64::NAN)),
        pair(Value::Int(2), Value::Float(1.0)),
    ];
    let err = run_kernel(
        rows.clone(),
        &[],
        &[kernel_agg("sum", Some("v"), false, false, 1)],
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect_err("NaN sum must fail");
    assert!(
        matches!(err, ExecutorError::DataException { .. }),
        "NaN sum is a data exception, got {err:?}"
    );
    let extremes = run_kernel(
        rows,
        &[],
        &[
            kernel_agg("min", Some("v"), false, false, 1),
            kernel_agg("max", Some("v"), false, false, 2),
        ],
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect("NaN min/max order");
    assert_eq!(
        extremes.rows()[0].values()[2],
        Value::Float(1.0),
        "min orders NaN high"
    );
    let max = &extremes.rows()[0].values()[3];
    assert!(
        matches!(max, Value::Float(value) if value.is_nan()),
        "max keeps NaN, got {max:?}"
    );
}

#[test]
fn incompatible_group_values_error_typed() {
    // An integer key and a string key share no distinctness family: the
    // grouping fails with a data exception, never a merged or split group.
    let graph = SharedGraph::new(GraphId::new(44_010));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Int(1), Value::Int(1)),
        pair(Value::String(db_string("a").unwrap()), Value::Int(2)),
    ];
    let err = run_kernel(
        rows,
        &[kernel_key("k", 1)],
        &[kernel_agg("count", None, true, false, 2)],
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect_err("incompatible keys must fail");
    assert!(
        matches!(err, ExecutorError::DataException { .. }),
        "incompatible groups are a data exception, got {err:?}"
    );
}

#[test]
fn group_cap_fails_typed_without_partial_output() {
    // The production group cap bounds hash state with the same 5GQL1
    // diagnostic as the row path; the tracer surfaces the error with no
    // partial table.
    let graph = SharedGraph::new(GraphId::new(44_011));
    let caps =
        ImplDefinedCaps::default().with_group_by_key_cap(std::num::NonZeroUsize::new(1).unwrap());
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Int(1), Value::Int(1)),
        pair(Value::Int(2), Value::Int(2)),
    ];
    let err = run_kernel(
        rows,
        &[kernel_key("k", 1)],
        &[kernel_agg("count", None, true, false, 2)],
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect_err("second group exceeds the cap");
    assert!(
        matches!(err, ExecutorError::ProgramLimitExceeded { .. }),
        "cap breach is a resource error, got {err:?}"
    );
    assert_eq!(err.gqlstatus().as_str(), "5GQL1");
}

#[test]
fn memory_exhaustion_fails_without_truncated_groups() {
    // A near-zero budget fails typed instead of presenting a truncated
    // group set; an empty grouped input still succeeds because it reserves
    // nothing.
    let graph = SharedGraph::new(GraphId::new(44_012));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let keys = vec![kernel_key("k", 1)];
    let aggregates = vec![kernel_agg("count", None, true, false, 2)];
    let err = run_kernel(
        vec![pair(Value::Int(1), Value::Int(1))],
        &keys,
        &aggregates,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::new(1),
    )
    .expect_err("exhausted budget must fail");
    assert!(
        matches!(err, ExecutorError::ProgramLimitExceeded { .. }),
        "exhaustion is a resource error, got {err:?}"
    );
    assert_eq!(err.gqlstatus().as_str(), "5GQL1");
    let empty = run_kernel(
        Vec::new(),
        &keys,
        &aggregates,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::new(0),
    )
    .expect("empty grouped reserves nothing");
    assert_eq!(empty.row_count(), 0);
}

#[test]
fn cancelled_grouping_releases_without_partial_output() {
    // A pre-cancelled token fails at init with Cancelled; cancelling after
    // init fails the first pull. Neither path yields a partial table.
    let graph = SharedGraph::new(GraphId::new(44_013));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Int(1), Value::Int(1)),
        pair(Value::Int(2), Value::Int(2)),
    ];
    let keys = vec![kernel_key("k", 1)];
    let aggregates = vec![kernel_agg("count", None, true, false, 2)];
    let token = CancellationToken::new();
    token.cancel();
    let table = BindingTable::new(pair_schema(), rows.clone());
    let mut op = BatchGroupBy::new(
        Box::new(BatchRowSource::new(table, BatchPolicy::default_policy())),
        &keys,
        &aggregates,
        eval,
        BatchPolicy::default_policy(),
    );
    let mut ctx = BatchExecutionContext::new(
        graph.read(),
        BatchCancel::new(Some(&token), None, None),
        MemoryBudget::unlimited(),
    );
    let err = trace_operator_to_table(&mut op, &mut ctx).unwrap_err();
    assert!(matches!(err, ExecutorError::Cancelled { .. }));
    assert_eq!(err.gqlstatus().as_str(), "5GQL2");
    assert!(ctx.is_closed());
    // Cancel between init and pull: init materializes, the pull fails.
    let live = CancellationToken::new();
    let table = BindingTable::new(pair_schema(), rows);
    let mut op = BatchGroupBy::new(
        Box::new(BatchRowSource::new(table, BatchPolicy::default_policy())),
        &keys,
        &aggregates,
        eval,
        BatchPolicy::default_policy(),
    );
    let mut ctx = BatchExecutionContext::new(
        graph.read(),
        BatchCancel::new(Some(&live), None, None),
        MemoryBudget::unlimited(),
    );
    let mut buffer = super::BatchBuffer::new();
    op.init(&mut ctx).expect("live init succeeds");
    assert_eq!(op.state(), OperatorState::Open);
    live.cancel();
    let err = op.next_batch(&mut ctx, &mut buffer).unwrap_err();
    assert!(matches!(err, ExecutorError::Cancelled { .. }));
    assert_eq!(op.state(), OperatorState::Failed);
    op.close(&mut ctx);
    assert!(ctx.is_closed());
}

#[test]
fn group_operator_lifecycle_schema_and_batching() {
    // Init-once lifecycle, declared output schema before any pull, and
    // schema-preserving output over three groups.
    let graph = SharedGraph::new(GraphId::new(44_014));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Int(1), Value::Int(1)),
        pair(Value::Int(2), Value::Int(2)),
        pair(Value::Int(3), Value::Int(3)),
    ];
    let keys = vec![kernel_key("k", 1)];
    let aggregates = vec![kernel_agg("count", None, true, false, 2)];
    let policy = BatchPolicy::new(2, 1 << 20).unwrap();
    let table = BindingTable::new(pair_schema(), rows);
    let mut op = BatchGroupBy::new(
        Box::new(BatchRowSource::new(table, policy)),
        &keys,
        &aggregates,
        eval,
        policy,
    );
    assert_eq!(op.state(), OperatorState::Created);
    let schema = op.output_schema().clone();
    assert_eq!(schema.columns.len(), 3, "input columns plus one aggregate");
    assert_eq!(
        op.aggregate_names(),
        vec![db_string("count_2").unwrap()],
        "descriptor names match discovery order"
    );
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    op.init(&mut ctx).expect("group inits");
    assert_eq!(op.state(), OperatorState::Open);
    // Manual pulls prove the batch slicer splits three groups across two
    // pulls under the two-row policy.
    let mut buffer = super::BatchBuffer::new();
    let mut pulled = 0usize;
    let mut values = Vec::new();
    while let Some(batch) = op.next_batch(&mut ctx, &mut buffer).expect("pulls succeed") {
        pulled += 1;
        values.extend(batch.logical_rows_vec());
        ctx.budget_mut().release(batch.estimated_bytes());
        batch.recycle(&mut buffer);
    }
    assert_eq!(op.state(), OperatorState::Exhausted);
    assert_eq!(pulled, 2, "three groups split across two pulls");
    assert_eq!(op.batches_produced(), 2);
    assert_eq!(values.len(), 3);
    op.close(&mut ctx);
    assert_eq!(op.state(), OperatorState::Closed);
    assert!(ctx.is_closed());
}

#[test]
fn native_group_kernel_matches_independent_oracle() {
    // Cross-type numerics plus nulls (one family per key position)
    // through the native kernel and the independent grouping oracle:
    // identical partitions with identical per-group counts.
    let graph = SharedGraph::new(GraphId::new(44_015));
    let caps = ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Int(1), Value::Int(10)),
        pair(Value::Null, Value::Int(20)),
        pair(Value::Float(1.0), Value::Int(30)),
        pair(Value::Int(2), Value::Int(40)),
        pair(Value::Null, Value::Int(50)),
        pair(Value::Int128(1), Value::Int(60)),
    ];
    let keys = vec![kernel_key("k", 1)];
    let aggregates = vec![kernel_agg("count", None, true, false, 2)];
    let table = run_kernel(
        rows.clone(),
        &keys,
        &aggregates,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect("oracle fixture groups");
    let plain = rows
        .iter()
        .map(|row| row.values().to_vec())
        .collect::<Vec<_>>();
    let model = groups_of(&plain, 1);
    assert_eq!(model.len(), table.row_count(), "same group count");
    // Multiset comparison against the oracle's expected (key, count) rows.
    let mut expected = Vec::new();
    for members in &model {
        let mut row = plain[members[0]][..1].to_vec();
        row.push(Value::Int(members.len() as i64));
        expected.push(row);
    }
    let actual = table
        .rows()
        .iter()
        .map(|row| vec![row.values()[0].clone(), row.values()[2].clone()])
        .collect::<Vec<_>>();
    assert_same_multiset(&expected, &actual, "kernel vs grouping oracle");
}
