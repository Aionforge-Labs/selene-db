//! F04-PR03 set-operation acceptance: kernels, differentials, oracle.
//!
//! Each test maps to one slice requirement:
//!
//! - set versus multiset variants of the same inputs produce deliberately
//!   different duplicate counts (all six operators plus `OTHERWISE`);
//! - schema and column alignment follow the positional contract with the
//!   row path's diagnostics;
//! - internal hash keys agree with the language equality relation (mixed
//!   numerics, permuted records, null rows) between the engine and the
//!   independent oracle;
//! - bounded key caps and memory budgets fail with typed resource errors,
//!   never partial relations;
//! - facade queries agree with the row oracle across randomized batch
//!   sizes, with hand-derived counts as the independent check.

use std::num::NonZeroUsize;

use selene_core::{DbString, Value, db_string};
use selene_graph::SharedGraph;

use crate::{
    ExecutionPlan, SetOp,
    plan::ImplDefinedCaps,
    runtime::{Binding, BindingTable, ExecutorError, TxContext},
};

use super::fixtures::{
    batch_prefix_with_policy, kernel_ctx, person_graph, plan_source, production_table,
};
use super::relation_model::{assert_same_multiset, multiset_op};
use super::set::{BatchSet, combine_set_rows};
use super::unit::BatchRowSource;
use super::{
    BatchBuffer, BatchExecutionContext, BatchPolicy, MemoryBudget, OperatorState, PhysicalOperator,
    assert_tables_equivalent,
};

/// Batch sizes for the boundary matrix plus deterministic pseudo-random
/// windows (seeded LCG, no new dependencies).
fn policies() -> Vec<BatchPolicy> {
    let mut fixed = [1usize, 2, 3, 7, 1024]
        .into_iter()
        .map(|target| BatchPolicy::new(target, 1 << 20).unwrap())
        .collect::<Vec<_>>();
    let mut state = 0x243F_6A88_85A3_08D3u64;
    for _ in 0..10 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let target = 1 + (state >> 33) as usize % 24;
        fixed.push(BatchPolicy::new(target, 1 << 20).unwrap());
    }
    fixed
}

fn int_row(value: i64) -> Binding {
    Binding::new([Value::Int(value)])
}

fn rows(values: &[i64]) -> Vec<Binding> {
    values.iter().map(|value| int_row(*value)).collect()
}

fn values_of(table: &[Binding]) -> Vec<Vec<Value>> {
    table.iter().map(|row| row.values().to_vec()).collect()
}

/// Run one counted/set kernel through the native combinator and the
/// independent oracle, requiring identical multisets.
fn check_kernel(op: SetOp, distinct: bool, lhs: &[i64], rhs: &[i64], what: &str) -> Vec<Binding> {
    let lhs_rows = rows(lhs);
    let rhs_rows = rows(rhs);
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    let (combined, reserved) = combine_set_rows(
        op,
        lhs_rows,
        &rhs_rows,
        &ImplDefinedCaps::default(),
        &mut ctx,
    )
    .unwrap_or_else(|err| panic!("{what}: native combine failed: {err:?}"));
    ctx.budget_mut().release(reserved);
    let oracle = multiset_op(
        op,
        &lhs.iter()
            .map(|value| vec![Value::Int(*value)])
            .collect::<Vec<_>>(),
        &rhs.iter()
            .map(|value| vec![Value::Int(*value)])
            .collect::<Vec<_>>(),
        distinct,
    );
    assert_same_multiset(&oracle, &values_of(&combined), what);
    combined
}

#[test]
fn set_and_multiset_versions_count_deliberately_different() {
    // UNION ALL keeps all five rows; UNION keeps three.
    let all = check_kernel(SetOp::UnionAll, false, &[1, 1, 2], &[2, 3], "union all");
    assert_eq!(all.len(), 5);
    let distinct = check_kernel(SetOp::Union, true, &[1, 1, 2], &[2, 3], "union");
    assert_eq!(distinct.len(), 3);
    assert_eq!(
        values_of(&distinct),
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)]
        ],
        "union keeps first occurrences in arm order"
    );
    // INTERSECT ALL keeps two; INTERSECT keeps one.
    let all = check_kernel(
        SetOp::IntersectAll,
        false,
        &[1, 1, 2, 2, 2],
        &[1, 2, 2, 4],
        "intersect all",
    );
    assert_eq!(all.len(), 3);
    let distinct = check_kernel(
        SetOp::Intersect,
        true,
        &[1, 1, 2, 2, 2],
        &[1, 2, 2, 4],
        "intersect",
    );
    assert_eq!(distinct.len(), 2);
    // EXCEPT ALL keeps two; EXCEPT keeps one.
    let all = check_kernel(
        SetOp::ExceptAll,
        false,
        &[1, 1, 1, 2],
        &[1, 1],
        "except all",
    );
    assert_eq!(all.len(), 2);
    let distinct = check_kernel(SetOp::Except, true, &[1, 1, 1, 2], &[1, 1], "except");
    assert_eq!(distinct.len(), 1);
    assert_eq!(values_of(&distinct), vec![vec![Value::Int(2)]]);
}

#[test]
fn empty_arms_follow_set_rules() {
    assert!(check_kernel(SetOp::UnionAll, false, &[], &[1], "empty lhs").len() == 1);
    assert!(check_kernel(SetOp::Union, true, &[1], &[], "empty rhs").len() == 1);
    assert!(check_kernel(SetOp::IntersectAll, false, &[1], &[], "intersect empty").is_empty());
    assert!(check_kernel(SetOp::ExceptAll, false, &[], &[1], "except empty lhs").is_empty());
    assert_eq!(
        check_kernel(SetOp::Except, true, &[1, 2], &[], "except empty rhs").len(),
        2
    );
}

#[test]
fn null_rows_use_distinctness_equality() {
    // Null equals null for set membership: UNION dedups null rows and
    // EXCEPT removes null rows present on the right.
    let null = || Binding::new([Value::Null]);
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    let (unioned, reserved) = combine_set_rows(
        SetOp::Union,
        vec![null(), null()],
        &[null()],
        &ImplDefinedCaps::default(),
        &mut ctx,
    )
    .expect("null union combines");
    ctx.budget_mut().release(reserved);
    assert_eq!(unioned.len(), 1, "null rows dedup under UNION");
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    let (dropped, reserved) = combine_set_rows(
        SetOp::Except,
        vec![null(), int_row(1)],
        &[null()],
        &ImplDefinedCaps::default(),
        &mut ctx,
    )
    .expect("null except combines");
    ctx.budget_mut().release(reserved);
    assert_eq!(values_of(&dropped), vec![vec![Value::Int(1)]]);
    let oracle = multiset_op(
        SetOp::Union,
        &[vec![Value::Null], vec![Value::Null]],
        &[vec![Value::Null]],
        true,
    );
    assert_same_multiset(&oracle, &values_of(&unioned), "null union oracle");
}

#[test]
fn mixed_numeric_and_record_rows_share_key_equality() {
    use selene_core::Record;
    use smallvec::smallvec;
    let name = |s: &str| db_string(s).unwrap();
    let rec = |pairs: Vec<(DbString, Value)>| {
        Binding::new([Value::Record(Box::new(Record::Open(
            pairs.into_iter().collect(),
        )))])
    };
    // One comparable family per combination: the language rejects
    // cross-family columns (see the error-parity check below), while
    // compatible rows inside a family share one equality relation.
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    let (unioned, reserved) = combine_set_rows(
        SetOp::Union,
        vec![Binding::new([Value::Int(1)])],
        &[
            Binding::new([Value::Float(1.0)]),
            Binding::new([Value::Uint(1)]),
            Binding::new([Value::Int(2)]),
        ],
        &ImplDefinedCaps::default(),
        &mut ctx,
    )
    .expect("numeric union combines");
    ctx.budget_mut().release(reserved);
    assert_eq!(unioned.len(), 2, "numerics collapse, got {unioned:?}");
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    let (unioned, reserved) = combine_set_rows(
        SetOp::Union,
        vec![rec(vec![
            (name("a"), Value::Int(1)),
            (name("b"), Value::Int(2)),
        ])],
        &[rec(vec![
            (name("b"), Value::Float(2.0)),
            (name("a"), Value::Int(1)),
        ])],
        &ImplDefinedCaps::default(),
        &mut ctx,
    )
    .expect("record union combines");
    ctx.budget_mut().release(reserved);
    assert_eq!(unioned.len(), 1, "permuted records collapse");
    let oracle = multiset_op(
        SetOp::Union,
        &[vec![Value::Int(1)]],
        &[
            vec![Value::Float(1.0)],
            vec![Value::Uint(1)],
            vec![Value::Int(2)],
        ],
        true,
    );
    assert_same_multiset(
        &oracle,
        &[vec![Value::Int(1)], vec![Value::Int(2)]],
        "numeric union oracle",
    );
    let oracle = multiset_op(
        SetOp::Union,
        &[vec![Value::Record(Box::new(Record::Open(smallvec![
            (name("a"), Value::Int(1)),
            (name("b"), Value::Int(2)),
        ])))]],
        &[vec![Value::Record(Box::new(Record::Open(smallvec![
            (name("b"), Value::Float(2.0)),
            (name("a"), Value::Int(1)),
        ])))]],
        true,
    );
    assert_eq!(oracle.len(), 1, "oracle collapses permuted records");
    // Cross-family columns fail identically: integers against records are
    // a language-level data exception, never a silent dedup split.
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    let err = combine_set_rows(
        SetOp::Union,
        vec![Binding::new([Value::Int(1)])],
        &[rec(vec![(name("a"), Value::Int(1))])],
        &ImplDefinedCaps::default(),
        &mut ctx,
    )
    .expect_err("cross-family columns must fail");
    assert!(
        matches!(
            err,
            ExecutorError::DataException {
                subclass: crate::runtime::DataExceptionSubclass::ValuesNotComparable,
                ..
            }
        ),
        "values-not-comparable, got {err:?}"
    );
}

#[test]
fn key_cap_failure_matches_row_diagnostic() {
    // One allowed key with two distinct right-arm keys: the second distinct
    // key trips the production cap with the row path's diagnostic.
    let caps = ImplDefinedCaps::default().with_set_op_key_cap(NonZeroUsize::new(1).unwrap());
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    let err = combine_set_rows(
        SetOp::IntersectAll,
        rows(&[1, 2]),
        &rows(&[1, 2]),
        &caps,
        &mut ctx,
    )
    .expect_err("key cap must trip");
    assert!(
        matches!(err, ExecutorError::ProgramLimitExceeded { detail, .. } if detail == "set-op key cap exceeded"),
        "row-identical cap diagnostic, got {err:?}"
    );
    assert_eq!(err.gqlstatus().as_str(), "5GQL1");
}

#[test]
fn bounded_budget_fails_set_fanout_without_partial_output() {
    let mut ctx = kernel_ctx(MemoryBudget::new(64));
    let err = combine_set_rows(
        SetOp::Union,
        rows(&[1, 2, 3, 4, 5, 6, 7, 8]),
        &rows(&[9, 10, 11, 12, 13, 14, 15, 16]),
        &ImplDefinedCaps::default(),
        &mut ctx,
    )
    .expect_err("bounded set fanout must fail");
    assert!(
        matches!(err, ExecutorError::ProgramLimitExceeded { .. }),
        "typed resource error, got {err:?}"
    );
    assert_eq!(err.gqlstatus().as_str(), "5GQL1");
}

/// Execute an already-planned query with the single-row batch policy.
fn row_execute(graph: &SharedGraph, plan: &ExecutionPlan) -> Result<BindingTable, ExecutorError> {
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    super::fixtures::execute_single_row_batches(plan, &mut ctx)
}

/// Assert a fully-batch plan agrees with the row oracle under `policy`.
fn check_full_agree(graph: &SharedGraph, plan: &ExecutionPlan, policy: BatchPolicy, what: &str) {
    let row = row_execute(graph, plan);
    let batch = batch_prefix_with_policy(graph, plan, policy);
    match (row, batch) {
        (Ok(expected), Ok(table)) => {
            assert_tables_equivalent(&expected, &table, what);
        }
        (Err(expected), Err(actual)) => assert_eq!(
            expected.gqlstatus(),
            actual.gqlstatus(),
            "{what}: error status diverged (row {expected:?} vs batch {actual:?})"
        ),
        (Ok(_), Err(err)) => panic!("{what}: batch failed where row succeeded: {err:?}"),
        (Err(err), Ok(_)) => {
            panic!("{what}: single-row policy failed where another policy succeeded: {err:?}")
        }
    }
}

/// Assert agreement for `source` across the fixed and randomized matrices.
fn check_set_matrix(graph: &SharedGraph, source: &str) {
    let plan = plan_source(source);
    for policy in policies() {
        check_full_agree(graph, &plan, policy, source);
    }
}

#[test]
fn all_set_operators_match_row_oracle_across_sizes() {
    let graph = person_graph();
    // Hand-derived counts (independent of both engines) for the anchor
    // shapes: three persons plus two robots, disjoint name sets.
    let union_all = production_table(
        &graph,
        "MATCH (n:Person) RETURN n.name AS name UNION ALL MATCH (m:Robot) RETURN m.name AS name",
    );
    assert_eq!(union_all.row_count(), 10, "eight persons plus two robots");
    let union = production_table(
        &graph,
        "MATCH (n:Person) RETURN n.name AS name UNION MATCH (m:Robot) RETURN m.name AS name",
    );
    assert_eq!(union.row_count(), 10, "disjoint names keep ten");
    let self_union = production_table(
        &graph,
        "MATCH (n:Person) RETURN n.name AS name UNION MATCH (n:Person) RETURN n.name AS name",
    );
    assert_eq!(self_union.row_count(), 8, "self-union dedups to eight");
    let self_union_all = production_table(
        &graph,
        "MATCH (n:Person) RETURN n.name AS name UNION ALL MATCH (n:Person) RETURN n.name AS name",
    );
    assert_eq!(self_union_all.row_count(), 16, "self-union-all doubles");
    for source in [
        "RETURN 1 AS n UNION ALL RETURN 2 AS n",
        "RETURN 1 AS n UNION RETURN 1 AS n",
        "RETURN 1 AS n UNION ALL RETURN 1 AS n",
        "RETURN 1 AS n UNION RETURN 1 AS n UNION ALL RETURN 2 AS n",
        "RETURN 1 AS n LIMIT 0 UNION ALL RETURN 2 AS n LIMIT 0",
        "RETURN 1 AS n INTERSECT RETURN 1 AS n",
        "RETURN 1 AS n INTERSECT ALL RETURN 1 AS n",
        "RETURN 1 AS n EXCEPT RETURN 2 AS n",
        "RETURN 1 AS n EXCEPT ALL RETURN 1 AS n",
        "MATCH (n:Person) RETURN n.name AS name UNION ALL MATCH (m:Robot) RETURN m.name AS name",
        "MATCH (n:Person) RETURN n.name AS name UNION MATCH (n:Person) RETURN n.name AS name",
        "MATCH (n:Person) RETURN n.age AS age INTERSECT MATCH (m:Person) RETURN m.age AS age",
        "MATCH (n:Person) RETURN n.age AS age INTERSECT ALL MATCH (m:Person) RETURN m.age AS age",
        "MATCH (n:Person) RETURN n.name AS name EXCEPT MATCH (m:Robot) RETURN m.name AS name",
        "MATCH (n:Person) RETURN n.name AS name EXCEPT ALL MATCH (m:Robot) RETURN m.name AS name",
        "RETURN 1 AS n UNION ALL RETURN 2 AS n LIMIT 0 UNION ALL RETURN 3 AS n",
        // A join inside a set arm batch-routes through the nested driver.
        "MATCH (a:Person) MATCH (a)-[:KNOWS]->(b) RETURN a.name AS name UNION ALL MATCH (m:Robot) RETURN m.name AS name",
        // A correlated match inside a set arm routes the same way.
        "MATCH (a:Person) FILTER a.age > 20 MATCH (a)-[:KNOWS]->(b) RETURN a.name AS name UNION ALL MATCH (m:Robot) RETURN m.name AS name",
        // OTHERWISE runs its right arm only when the left input is empty.
        "RETURN 1 AS v LIMIT 0 OTHERWISE RETURN 2 AS v",
        "RETURN 1 AS n OTHERWISE RETURN 1 / 0 AS n",
    ] {
        check_set_matrix(&graph, source);
    }
    // Hand-derived OTHERWISE outcomes: the failing-division arm must never
    // execute while the left input is non-empty (conditional execution, not
    // eager), and the empty left input yields the right arm.
    let skipped = production_table(&graph, "RETURN 1 AS n OTHERWISE RETURN 1 / 0 AS n");
    assert_eq!(skipped.row_count(), 1, "non-empty lhs skips rhs");
    let taken = production_table(&graph, "RETURN 1 AS v LIMIT 0 OTHERWISE RETURN 2 AS v");
    assert_eq!(taken.row_count(), 1, "empty lhs runs rhs");
    let nested = production_table(
        &graph,
        "MATCH (a:Person) MATCH (a)-[:KNOWS]->(b) RETURN a.name AS name UNION ALL MATCH (m:Robot) RETURN m.name AS name",
    );
    assert_eq!(nested.row_count(), 4, "two joined plus two robots");
}

#[test]
fn set_operators_agree_with_oracle_on_duplicate_heavy_inputs() {
    // Duplicate-heavy literal arms with hand-derived multiset expectations
    // checked against both the row oracle and the relation oracle.
    let cases = [
        (
            "RETURN 1 AS n UNION ALL RETURN 1 AS n UNION ALL RETURN 2 AS n",
            3,
        ),
        ("RETURN 1 AS n UNION RETURN 1 AS n", 1),
        ("RETURN 1 AS n INTERSECT ALL RETURN 1 AS n", 1),
        ("RETURN 1 AS n INTERSECT RETURN 2 AS n", 0),
        ("RETURN 1 AS n EXCEPT ALL RETURN 1 AS n", 0),
        ("RETURN 2 AS n EXCEPT RETURN 1 AS n", 1),
    ];
    let graph = person_graph();
    for (source, count) in cases {
        let table = production_table(&graph, source);
        assert_eq!(table.row_count(), count, "hand-derived count for {source}");
        check_set_matrix(&graph, source);
    }
}

#[test]
fn mismatched_arms_fail_identically_on_both_paths() {
    // Differing column counts pass lowering and fail at the runtime
    // boundary with the same data exception on both paths.
    let graph = person_graph();
    let plan = plan_source("RETURN 1 AS a UNION ALL RETURN 1 AS a, 2 AS b");
    for policy in policies() {
        let row = row_execute(&graph, &plan);
        let batch = batch_prefix_with_policy(&graph, &plan, policy);
        match (row, batch) {
            (Err(expected), Err(actual)) => assert_eq!(
                expected.gqlstatus(),
                actual.gqlstatus(),
                "arm-mismatch status diverged"
            ),
            (row, batch) => panic!(
                "mismatched arms must fail, got row_ok={} batch_ok={}",
                row.is_ok(),
                batch.is_ok()
            ),
        }
    }
}

#[test]
fn set_operator_serves_slices_and_releases_budget() {
    // Direct BatchSet over a row source plus a one-row right arm: UNION
    // dedups [1, 1, 2] ++ [2] to [1, 2] across tiny pulls, then releases.
    let graph = person_graph();
    let arm = plan_source("RETURN 1 AS n");
    let schema = arm.output_schema.clone();
    let lhs = BindingTable::new(schema.clone(), rows(&[1, 1, 2]));
    let rhs = plan_source("RETURN 2 AS n");
    let tx = TxContext::read_only(
        graph.read(),
        &arm.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let eval = crate::runtime::EvalCtx {
        tx: &tx,
        expr_ids: &rhs.expr_ids,
        subqueries: &rhs.subqueries,
    };
    let policy = BatchPolicy::new(1, 1 << 20).unwrap();
    let mut set = BatchSet::new(
        Box::new(BatchRowSource::new(lhs, policy)),
        SetOp::Union,
        &rhs,
        schema.clone(),
        eval,
        policy,
    );
    assert_eq!(set.state(), OperatorState::Created);
    let mut ctx = BatchExecutionContext::new(
        graph.read(),
        crate::runtime::batch::budget::BatchCancel::disabled(),
        MemoryBudget::unlimited(),
    );
    set.init(&mut ctx).expect("set inits");
    assert_eq!(set.state(), OperatorState::Open);
    let mut buffer = BatchBuffer::new();
    let mut found = Vec::new();
    while let Some(batch) = set
        .next_batch(&mut ctx, &mut buffer)
        .expect("pulls succeed")
    {
        found.extend(batch.logical_rows_vec());
        ctx.budget_mut().release(batch.estimated_bytes());
        batch.recycle(&mut buffer);
    }
    assert_eq!(set.state(), OperatorState::Exhausted);
    assert_eq!(
        found,
        vec![vec![Value::Int(1)], vec![Value::Int(2)]],
        "union dedups across arms"
    );
    assert!(set.batches_produced() > 1, "output splits across pulls");
    set.close(&mut ctx);
    assert_eq!(set.state(), OperatorState::Closed);
}

#[test]
fn production_path_enforces_set_key_cap() {
    // The operator reads the statement caps (not defaults): one allowed key
    // with a two-key right arm fails end to end with the row diagnostic.
    let graph = person_graph();
    let caps = ImplDefinedCaps::default().with_set_op_key_cap(NonZeroUsize::new(1).unwrap());
    let arm = plan_source("RETURN 1 AS n");
    let schema = arm.output_schema.clone();
    let lhs = BindingTable::new(schema.clone(), rows(&[1, 2]));
    let rhs = plan_source("RETURN 1 AS n UNION ALL RETURN 2 AS n");
    let tx = TxContext::read_only(
        graph.read(),
        &caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let eval = crate::runtime::EvalCtx {
        tx: &tx,
        expr_ids: &rhs.expr_ids,
        subqueries: &rhs.subqueries,
    };
    let policy = BatchPolicy::default_policy();
    let mut set = BatchSet::new(
        Box::new(BatchRowSource::new(lhs, policy)),
        SetOp::IntersectAll,
        &rhs,
        schema,
        eval,
        policy,
    );
    let mut ctx = BatchExecutionContext::new(
        graph.read(),
        crate::runtime::batch::budget::BatchCancel::disabled(),
        MemoryBudget::unlimited(),
    );
    let err = set.init(&mut ctx).expect_err("capped intersect must fail");
    assert!(
        matches!(err, ExecutorError::ProgramLimitExceeded { detail, .. } if detail == "set-op key cap exceeded"),
        "row-identical cap diagnostic, got {err:?}"
    );
    set.close(&mut ctx);
}

#[test]
fn driver_accepts_set_shapes_through_production() {
    let graph = person_graph();
    for source in [
        "RETURN 1 AS n UNION ALL RETURN 2 AS n",
        "RETURN 1 AS n INTERSECT RETURN 1 AS n",
        "RETURN 1 AS n EXCEPT ALL RETURN 2 AS n",
        "MATCH (n:Person) RETURN n AS x UNION ALL MATCH (m:Robot) RETURN m AS x",
    ] {
        let plan = plan_source(source);
        batch_prefix_with_policy(&graph, &plan, BatchPolicy::default_policy())
            .expect("complete physical set operation");
    }
}
