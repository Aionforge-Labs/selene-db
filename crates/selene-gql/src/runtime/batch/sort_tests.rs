//! F04-PR04 sorting kernel acceptance.
//!
//! Each test maps to one slice requirement over hand-built rows, so
//! expectations are derived by hand, not copied from either engine:
//!
//! - null ordering in both directions and all explicit policies, with
//!   hand-derived orders;
//! - binary string collation (`"Zebra"` before `"apple"`) against
//!   hand-derived fixtures;
//! - ties keep input order with no implicit total ordering, across batch
//!   sizes and partitionings;
//! - incompatible key families error typed;
//! - memory exhaustion and cancellation fail typed without presenting a
//!   truncated-complete order;
//! - the native kernel matches the independent ordering model.
//!
//! Top-K windows, deduplication, carrier trimming, and operator lifecycle
//! live in [`super::topk_distinct_tests`]. Facade parity against the row
//! oracle lives in [`super::aggregate_differentials`].

use selene_core::{GraphId, Value, db_string};
use selene_graph::SharedGraph;

use crate::{
    EmptyProcedureRegistry, NullsPolicy, OrderDirection, OrderKey,
    plan::BindingTableSchema,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, TxContext},
};

use super::fixtures::{
    assert_kernel_rows, cell, kernel_ctx, kernel_order_key, pair, pair_schema, single_schema,
};
use super::relation_model::{ModelSortKey, model_sort, values_equal};
use super::sort::{BatchSort, BatchTopK};
use super::tracer::trace_operator_to_table;
use super::unit::BatchRowSource;
use super::{
    BatchCancel, BatchExecutionContext, BatchPolicy, MemoryBudget, OperatorState, PhysicalOperator,
};

/// Batch sizes for the boundary matrix: single-row pulls split every tie
/// run; the default covers steady state.
fn policies() -> Vec<BatchPolicy> {
    [1usize, 2, 3, 1024]
        .into_iter()
        .map(|target| BatchPolicy::new(target, 1 << 20).unwrap())
        .collect()
}

/// Build the detached kernel environment most sort tests share.
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

/// Run one sort kernel over hand-built rows.
fn run_sort(
    schema: BindingTableSchema,
    rows: Vec<Binding>,
    keys: &[OrderKey],
    eval: EvalCtx<'_, '_, '_, '_>,
    policy: BatchPolicy,
    budget: MemoryBudget,
) -> Result<BindingTable, ExecutorError> {
    let table = BindingTable::new(schema, rows);
    let source = BatchRowSource::new(table, policy);
    let mut op = BatchSort::new(Box::new(source), keys, eval, policy);
    let mut ctx = kernel_ctx(budget);
    trace_operator_to_table(&mut op, &mut ctx)
}

/// Run one top-K kernel over hand-built rows with a resolved window.
fn run_top_k(
    schema: BindingTableSchema,
    rows: Vec<Binding>,
    keys: &[OrderKey],
    window: (u64, u64),
    eval: EvalCtx<'_, '_, '_, '_>,
    policy: BatchPolicy,
    budget: MemoryBudget,
) -> Result<BindingTable, ExecutorError> {
    let table = BindingTable::new(schema, rows);
    let source = BatchRowSource::new(table, policy);
    let mut op = BatchTopK::new(Box::new(source), keys, window.0, window.1, eval, policy);
    let mut ctx = kernel_ctx(budget);
    trace_operator_to_table(&mut op, &mut ctx)
}

#[test]
fn null_ordering_follows_policy_in_both_directions() {
    // Hand-derived null placement: Asc defaults last, Desc defaults
    // first, and the explicit policies win in both directions. A host
    // language default (Option ordering, float total order) would fail
    // at least one of these four.
    let graph = SharedGraph::new(GraphId::new(45_001));
    let caps = crate::plan::ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        cell(Value::Int(3)),
        cell(Value::Null),
        cell(Value::Int(1)),
        cell(Value::Null),
        cell(Value::Int(2)),
    ];
    let cases = [
        (
            OrderDirection::Asc,
            None,
            vec![1, 2, 3]
                .into_iter()
                .map(Value::Int)
                .chain([Value::Null, Value::Null])
                .map(|value| vec![value])
                .collect::<Vec<_>>(),
            "asc defaults nulls last",
        ),
        (
            OrderDirection::Asc,
            Some(NullsPolicy::NullsFirst),
            [
                Value::Null,
                Value::Null,
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
            ]
            .into_iter()
            .map(|value| vec![value])
            .collect::<Vec<_>>(),
            "asc nulls first",
        ),
        (
            OrderDirection::Desc,
            None,
            [
                Value::Null,
                Value::Null,
                Value::Int(3),
                Value::Int(2),
                Value::Int(1),
            ]
            .into_iter()
            .map(|value| vec![value])
            .collect::<Vec<_>>(),
            "desc defaults nulls first",
        ),
        (
            OrderDirection::Desc,
            Some(NullsPolicy::NullsLast),
            [
                Value::Int(3),
                Value::Int(2),
                Value::Int(1),
                Value::Null,
                Value::Null,
            ]
            .into_iter()
            .map(|value| vec![value])
            .collect::<Vec<_>>(),
            "desc nulls last",
        ),
    ];
    for (direction, nulls, expected, what) in &cases {
        let keys = vec![kernel_order_key("k", 1, *direction, *nulls)];
        for policy in policies() {
            let table = run_sort(
                single_schema(),
                rows.clone(),
                &keys,
                eval,
                policy,
                MemoryBudget::unlimited(),
            )
            .expect("null ordering sorts");
            assert_kernel_rows(&table, expected, what);
        }
    }
}

#[test]
fn string_keys_follow_binary_collation() {
    // The selected collation is binary codepoint order: uppercase sorts
    // before lowercase regardless of dictionary order. Hand-derived.
    let graph = SharedGraph::new(GraphId::new(45_002));
    let caps = crate::plan::ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let text = ["apple", "Zebra", "banana", "Apple"];
    let rows = text
        .iter()
        .map(|value| cell(Value::String(db_string(value).unwrap())))
        .chain([cell(Value::Null)])
        .collect::<Vec<_>>();
    let ordered = ["Apple", "Zebra", "apple", "banana"];
    let expected_asc = ordered
        .iter()
        .map(|value| vec![Value::String(db_string(value).unwrap())])
        .chain([vec![Value::Null]])
        .collect::<Vec<_>>();
    let keys = vec![kernel_order_key("k", 1, OrderDirection::Asc, None)];
    for policy in policies() {
        let table = run_sort(
            single_schema(),
            rows.clone(),
            &keys,
            eval,
            policy,
            MemoryBudget::unlimited(),
        )
        .expect("strings sort");
        assert_kernel_rows(&table, &expected_asc, "binary collation asc");
    }
    let expected_desc = [vec![Value::Null]]
        .into_iter()
        .chain(
            ordered
                .iter()
                .rev()
                .map(|value| vec![Value::String(db_string(value).unwrap())]),
        )
        .collect::<Vec<_>>();
    let keys = vec![kernel_order_key("k", 1, OrderDirection::Desc, None)];
    let table = run_sort(
        single_schema(),
        rows,
        &keys,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect("strings sort desc");
    assert_kernel_rows(&table, &expected_desc, "binary collation desc");
}

#[test]
fn ties_keep_input_order_across_batch_sizes() {
    // Equal keys keep pull order under every batch shape: no implicit
    // total ordering from ties, even when ties straddle batch windows.
    let graph = SharedGraph::new(GraphId::new(45_003));
    let caps = crate::plan::ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Int(2), Value::Int(30)),
        pair(Value::Int(1), Value::Int(10)),
        pair(Value::Int(2), Value::Int(20)),
        pair(Value::Int(1), Value::Int(40)),
        pair(Value::Int(2), Value::Int(50)),
    ];
    let keys = vec![kernel_order_key("k", 1, OrderDirection::Asc, None)];
    let expected = vec![
        vec![Value::Int(1), Value::Int(10)],
        vec![Value::Int(1), Value::Int(40)],
        vec![Value::Int(2), Value::Int(30)],
        vec![Value::Int(2), Value::Int(20)],
        vec![Value::Int(2), Value::Int(50)],
    ];
    for policy in policies() {
        let table = run_sort(
            pair_schema(),
            rows.clone(),
            &keys,
            eval,
            policy,
            MemoryBudget::unlimited(),
        )
        .expect("ties sort stably");
        assert_kernel_rows(&table, &expected, "ties keep input order");
    }
}

#[test]
fn numeric_keys_order_across_types_with_nan_high() {
    // Cross-type numerics order by exact value; NaN sorts high per the
    // deterministic numeric sort (not by float total order accident: the
    // engine never calls total_cmp on the sort path).
    let graph = SharedGraph::new(GraphId::new(45_004));
    let caps = crate::plan::ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        cell(Value::Float(f64::NAN)),
        cell(Value::Int(2)),
        cell(Value::Float(1.5)),
        cell(Value::Int(1)),
        cell(Value::Float(f64::NEG_INFINITY)),
    ];
    let keys = vec![kernel_order_key("k", 1, OrderDirection::Asc, None)];
    let table = run_sort(
        single_schema(),
        rows,
        &keys,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect("numerics sort");
    let ordered = table
        .rows()
        .iter()
        .map(|row| row.values()[0].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        ordered[..4],
        vec![
            Value::Float(f64::NEG_INFINITY),
            Value::Int(1),
            Value::Float(1.5),
            Value::Int(2),
        ],
        "exact numeric order, got {ordered:?}"
    );
    assert!(
        matches!(ordered[4], Value::Float(value) if value.is_nan()),
        "NaN sorts high, got {ordered:?}"
    );
}

#[test]
fn incompatible_sort_keys_error_typed() {
    // An integer and a string share no ordering family: the sort fails
    // with a data exception, never an invented cross-family order.
    let graph = SharedGraph::new(GraphId::new(45_005));
    let caps = crate::plan::ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        cell(Value::Int(1)),
        cell(Value::String(db_string("a").unwrap())),
    ];
    let keys = vec![kernel_order_key("k", 1, OrderDirection::Asc, None)];
    let err = run_sort(
        single_schema(),
        rows,
        &keys,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect_err("incompatible keys must fail");
    assert!(
        matches!(err, ExecutorError::DataException { .. }),
        "incompatible sort is a data exception, got {err:?}"
    );
}

#[test]
fn top_k_matches_full_sort_plus_page_including_ties() {
    // Every window of a tied fixture matches slicing the fully sorted
    // output; ties at the window edge resolve to the earliest input rows
    // deterministically.
    let graph = SharedGraph::new(GraphId::new(45_006));
    let caps = crate::plan::ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Int(2), Value::Int(0)),
        pair(Value::Int(1), Value::Int(1)),
        pair(Value::Int(2), Value::Int(2)),
        pair(Value::Int(3), Value::Int(3)),
        pair(Value::Int(1), Value::Int(4)),
        pair(Value::Int(2), Value::Int(5)),
    ];
    let keys = vec![kernel_order_key("k", 1, OrderDirection::Asc, None)];
    let full = run_sort(
        pair_schema(),
        rows.clone(),
        &keys,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect("full sort runs");
    let full_rows = full
        .rows()
        .iter()
        .map(|row| row.values().to_vec())
        .collect::<Vec<_>>();
    for (offset, count) in [
        (0, 0),
        (0, 1),
        (0, 3),
        (1, 2),
        (2, 10),
        (4, 2),
        (6, 1),
        (10, 5),
    ] {
        for policy in policies() {
            let window = run_top_k(
                pair_schema(),
                rows.clone(),
                &keys,
                (offset, count),
                eval,
                policy,
                MemoryBudget::unlimited(),
            )
            .expect("top-k runs");
            let start = (offset as usize).min(full_rows.len());
            let end = start.saturating_add(count as usize).min(full_rows.len());
            assert_kernel_rows(&window, &full_rows[start..end], "top-k window");
        }
    }
    // All-equal keys: the earliest input rows win the window.
    let tied = vec![
        pair(Value::Int(7), Value::Int(0)),
        pair(Value::Int(7), Value::Int(1)),
        pair(Value::Int(7), Value::Int(2)),
        pair(Value::Int(7), Value::Int(3)),
    ];
    let window = run_top_k(
        pair_schema(),
        tied,
        &keys,
        (1, 2),
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect("tied top-k runs");
    assert_kernel_rows(
        &window,
        &[
            vec![Value::Int(7), Value::Int(1)],
            vec![Value::Int(7), Value::Int(2)],
        ],
        "tied top-k keeps earliest inputs",
    );
}

#[test]
fn top_k_heap_bounds_memory_below_full_sort() {
    // A narrow window over many rows peaks far below the full sort
    // buffer: the heap holds the retained window, not the input.
    let graph = SharedGraph::new(GraphId::new(45_007));
    let caps = crate::plan::ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = (0..200)
        .map(|value| cell(Value::Int(199 - value)))
        .collect::<Vec<_>>();
    let keys = vec![kernel_order_key("k", 1, OrderDirection::Asc, None)];
    let policy = BatchPolicy::default_policy();
    let source = BatchRowSource::new(BindingTable::new(single_schema(), rows.clone()), policy);
    let mut sort = BatchSort::new(Box::new(source), &keys, eval, policy);
    let mut sort_ctx = kernel_ctx(MemoryBudget::unlimited());
    trace_operator_to_table(&mut sort, &mut sort_ctx).expect("sort runs");
    let source = BatchRowSource::new(BindingTable::new(single_schema(), rows), policy);
    let mut top_k = BatchTopK::new(Box::new(source), &keys, 0, 3, eval, policy);
    let mut top_ctx = kernel_ctx(MemoryBudget::unlimited());
    let table = trace_operator_to_table(&mut top_k, &mut top_ctx).expect("top-k runs");
    assert_eq!(table.row_count(), 3);
    assert_kernel_rows(
        &table,
        &[
            vec![Value::Int(0)],
            vec![Value::Int(1)],
            vec![Value::Int(2)],
        ],
        "narrow window keeps the head",
    );
    assert!(
        top_ctx.budget_peak() < sort_ctx.budget_peak(),
        "top-k peak {} is below sort peak {}",
        top_ctx.budget_peak(),
        sort_ctx.budget_peak()
    );
}

#[test]
fn sort_memory_exhaustion_and_cancel_fail_typed() {
    // A near-zero budget fails typed instead of presenting a truncated
    // order; cancellation fails before or during the pull stream with no
    // partial table.
    use selene_core::CancellationToken;
    let graph = SharedGraph::new(GraphId::new(45_010));
    let caps = crate::plan::ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![cell(Value::Int(2)), cell(Value::Int(1))];
    let keys = vec![kernel_order_key("k", 1, OrderDirection::Asc, None)];
    let err = run_sort(
        single_schema(),
        rows.clone(),
        &keys,
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
    // Empty sorts reserve nothing and succeed even at zero budget.
    let empty = run_sort(
        single_schema(),
        Vec::new(),
        &keys,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::new(0),
    )
    .expect("empty sort reserves nothing");
    assert_eq!(empty.row_count(), 0);
    assert_eq!(empty.schema(), &single_schema());
    // Pre-cancelled token fails the trace.
    let token = CancellationToken::new();
    token.cancel();
    let table = BindingTable::new(single_schema(), rows.clone());
    let mut op = BatchSort::new(
        Box::new(BatchRowSource::new(table, BatchPolicy::default_policy())),
        &keys,
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
    let table = BindingTable::new(single_schema(), rows);
    let mut op = BatchSort::new(
        Box::new(BatchRowSource::new(table, BatchPolicy::default_policy())),
        &keys,
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
fn sort_matches_independent_model() {
    // The native kernel and the separately written ordering model agree on
    // a mixed two-key fixture (ints, strings, nulls — one family per key
    // — descending second key with nulls first).
    let graph = SharedGraph::new(GraphId::new(45_011));
    let caps = crate::plan::ImplDefinedCaps::default();
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = crate::SubqueryRegistry::default();
    kernel_setup!(graph, caps, expr_ids, subqueries, tx, eval);
    let rows = vec![
        pair(Value::Int(2), Value::String(db_string("b").unwrap())),
        pair(Value::Null, Value::String(db_string("a").unwrap())),
        pair(Value::Int(1), Value::Null),
        pair(Value::Int(2), Value::String(db_string("a").unwrap())),
        pair(Value::Null, Value::Null),
        pair(Value::Int(1), Value::String(db_string("c").unwrap())),
    ];
    let keys = vec![
        kernel_order_key("k", 1, OrderDirection::Asc, None),
        kernel_order_key("v", 2, OrderDirection::Desc, Some(NullsPolicy::NullsFirst)),
    ];
    let table = run_sort(
        pair_schema(),
        rows.clone(),
        &keys,
        eval,
        BatchPolicy::default_policy(),
        MemoryBudget::unlimited(),
    )
    .expect("model fixture sorts");
    let plain = rows
        .iter()
        .map(|row| row.values().to_vec())
        .collect::<Vec<_>>();
    let model = model_sort(
        &plain,
        &[
            ModelSortKey {
                column: 0,
                ascending: true,
                nulls_first: false,
            },
            ModelSortKey {
                column: 1,
                ascending: false,
                nulls_first: true,
            },
        ],
    );
    let expected = model
        .iter()
        .map(|index| plain[*index].clone())
        .collect::<Vec<_>>();
    assert_kernel_rows(
        &table,
        &expected,
        "native sort matches the independent model",
    );
    // The model's own null/value accounting is sane on this fixture: null
    // keys sort last, and within key 1 the null value sorts first (desc,
    // nulls first).
    assert!(values_equal(&expected[0][0], &Value::Int(1)));
    assert!(values_equal(&expected[4][0], &Value::Null));
}
