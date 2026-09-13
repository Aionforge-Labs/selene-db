//! F04-PR03 correlated-execution acceptance: match and chain operators.
//!
//! Each test maps to one slice requirement:
//!
//! - non-leading `MATCH` / `OPTIONAL MATCH` evaluate once per input row
//!   with that row as the seed, through logical planning and the stable
//!   result boundary;
//! - correlated bindings never leak across input rows (matched rows never
//!   contaminate later unmatched rows, at every batch size);
//! - `NEXT` blocks discard (`Chain`) or seed (`CorrelatedChain`) per row
//!   with the row path's error order;
//! - unbatchable inner shapes decline to the row suffix with identical
//!   outcomes (negative scope cases), and computed failures keep their
//!   status on both paths.

use selene_core::Value;
use selene_graph::SharedGraph;

use crate::{
    ExecutionPlan, JoinTree,
    runtime::{Binding, BindingTable, ExecutorError, TxContext},
};

use super::chain::{BatchChain, BatchCorrelatedChain, BatchMatch};
use super::fixtures::{
    batch_prefix_with_policy, oddball_graph, person_graph, plan_source, production_table, row_table,
};
use super::outer::BatchOuterJoin;
use super::tree::build_join_tree;
use super::unit::BatchRowSource;
use super::{
    BatchBuffer, BatchExecutionContext, BatchPolicy, MemoryBudget, OperatorState, PhysicalOperator,
    assert_same_rows, assert_tables_equivalent,
};

/// Batch sizes for the boundary matrix plus deterministic pseudo-random
/// windows (seeded LCG, no new dependencies).
fn policies() -> Vec<BatchPolicy> {
    let mut fixed = [1usize, 2, 3, 7, 1024]
        .into_iter()
        .map(|target| BatchPolicy::new(target, 1 << 20).unwrap())
        .collect::<Vec<_>>();
    let mut state = 0x4528_21E6_38D0_1377u64;
    for _ in 0..10 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let target = 1 + (state >> 33) as usize % 24;
        fixed.push(BatchPolicy::new(target, 1 << 20).unwrap());
    }
    fixed
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
fn check_correlated_matrix(graph: &SharedGraph, source: &str) {
    let plan = plan_source(source);
    for policy in policies() {
        check_full_agree(graph, &plan, policy, source);
    }
}

/// Assert production routing agrees with the row oracle, allowing a row
/// suffix or a full decline.
///
/// For shapes whose leading operators stay row-covered in this slice
/// (`FOR`/`UNWIND` openers), the batch driver declines or stops early and
/// the row path completes them; the observable outcome must still agree.
fn check_production_agree(graph: &SharedGraph, source: &str) {
    assert_tables_equivalent(
        &row_table(graph, source),
        &production_table(graph, source),
        source,
    );
}

/// Pull one operator to owned rows, releasing budget and recycling.
fn pull_all(
    root: &mut dyn PhysicalOperator,
    exec: &mut BatchExecutionContext<'_>,
) -> Vec<Vec<Value>> {
    let mut buffer = BatchBuffer::new();
    let mut rows = Vec::new();
    while let Some(batch) = root.next_batch(exec, &mut buffer).expect("pulls succeed") {
        rows.extend(batch.logical_rows_vec());
        exec.budget_mut().release(batch.estimated_bytes());
        batch.recycle(&mut buffer);
    }
    rows
}

#[test]
fn non_leading_match_evaluates_per_input_row() {
    let graph = person_graph();
    // Hand-derived expectation (independent of both engines): Alice->Bob
    // and Bob->Cara are the only KNOWS edges, so the seeded inner pattern
    // keeps exactly two bindings.
    let table = production_table(
        &graph,
        "MATCH (a:Person) FILTER a.age > 20 MATCH (a)-[e:KNOWS]->(b) RETURN a.name AS aname, b",
    );
    assert_eq!(table.row_count(), 2, "seeded match keeps two bindings");
    check_correlated_matrix(
        &graph,
        "MATCH (a:Person) FILTER a.age > 20 MATCH (a)-[e:KNOWS]->(b) RETURN a.name AS aname, b",
    );
    // A seeded inner comma join works too: Alice->Bob->Cara is the only
    // length-two KNOWS chain from a person.
    check_correlated_matrix(
        &graph,
        "MATCH (a:Person) FILTER a.age > 20 MATCH (a)-[:KNOWS]->(b), (b)-[:KNOWS]->(c) RETURN a.name AS aname, c",
    );
    let chained = production_table(
        &graph,
        "MATCH (a:Person) FILTER a.age > 20 MATCH (a)-[:KNOWS]->(b), (b)-[:KNOWS]->(c) RETURN a.name AS aname, c",
    );
    assert_eq!(chained.row_count(), 1, "one length-two chain survives");
}

#[test]
fn optional_match_never_leaks_across_input_rows() {
    let graph = person_graph();
    let source = "MATCH (a:Person) FILTER a.age > 20 OPTIONAL MATCH (a)-[e:KNOWS]->(b) RETURN a.name AS name, b";
    check_correlated_matrix(&graph, source);
    let table = production_table(&graph, source);
    assert_eq!(table.row_count(), 8, "two matched plus six preserved");
    // Only Alice and Bob have outgoing KNOWS edges; every other row must
    // bind null `b` rather than a leaked neighbor. Single-row batches
    // evaluate each input row alone, large batches share them: both must
    // isolate.
    for row in table.rows() {
        let [name, other] = row.values() else {
            panic!("expected two columns, got {:?}", row.values());
        };
        let Value::String(name) = name else {
            panic!("expected a name, got {name:?}");
        };
        let matched = ["Alice", "Bob"].iter().any(|who| name.as_str() == *who);
        assert_eq!(
            !matches!(other, Value::Null),
            matched,
            "row for {name:?} leaked or lost its match: {other:?}"
        );
    }
}

#[test]
fn uncorrelated_chain_discards_input_and_keeps_error_order() {
    let graph = person_graph();
    let table = production_table(&graph, "RETURN 1 AS a NEXT RETURN 2 AS b");
    assert_eq!(table.row_count(), 1);
    assert_eq!(table.rows()[0].values(), &[Value::Int(2)]);
    check_correlated_matrix(&graph, "RETURN 1 AS a NEXT RETURN 2 AS b");
    check_correlated_matrix(&graph, "MATCH (n:Person) RETURN n AS m NEXT RETURN 9 AS b");
    check_correlated_matrix(
        &graph,
        "RETURN 1 AS a NEXT RETURN 2 AS b NEXT RETURN 3 AS c",
    );
    // `FOR` openers stay row-covered in this slice (unwind is not a
    // join/set family): production still agrees through the row path.
    check_production_agree(&graph, "FOR a IN [1, 2, 3] RETURN a NEXT RETURN 9 AS b");
    // Left-side failures surface before the right block runs, and
    // right-side failures surface identically: error order is preserved.
    // The projection failure shape is the proven oddball case (a string age
    // meeting integer arithmetic), which errors at execution time on both
    // paths; the literal-mismatch arms would fail earlier at analysis.
    let odd = oddball_graph();
    for source in [
        "MATCH (n:Person) RETURN n.age + 1 AS x NEXT RETURN 2 AS b",
        "MATCH (n:Person) RETURN n AS m NEXT MATCH (p:Person) RETURN p.age + 1 AS y",
    ] {
        let plan = plan_source(source);
        for policy in policies() {
            check_full_agree(&odd, &plan, policy, source);
        }
        assert!(
            row_execute(&odd, &plan).is_err(),
            "error case must fail: {source}"
        );
    }
}

#[test]
fn correlated_chain_runs_its_block_per_input_row() {
    let graph = person_graph();
    // Hand-derived expectation: integer seeds (Alice 21, Bob 22) each seed
    // one block evaluation in scan order.
    let source =
        "MATCH (a:Person) FILTER a.age < 23 RETURN a.age AS seed NEXT RETURN seed + 10 AS b";
    let table = production_table(&graph, source);
    let values = table
        .rows()
        .iter()
        .map(|row| row.values().to_vec())
        .collect::<Vec<_>>();
    assert_same_rows(
        &values,
        &[vec![Value::Int(31)], vec![Value::Int(32)]],
        "per-row block evaluation",
    );
    check_correlated_matrix(&graph, source);
    // The canonical FOR opener keeps its row routing with identical
    // outcomes (unwind coverage belongs to a later slice).
    check_production_agree(&graph, "FOR a IN [1, 2] RETURN a NEXT RETURN a + 10 AS b");
    // A correlated pattern block observes prior bindings per row.
    check_correlated_matrix(
        &graph,
        "MATCH (a:Person) RETURN a NEXT MATCH (b:Person) FILTER b = a RETURN b",
    );
    // Limits inside and after blocks compose with batch routing.
    check_correlated_matrix(
        &graph,
        "MATCH (a:Person) FILTER a.age < 23 RETURN a.age AS seed NEXT RETURN seed LIMIT 1",
    );
    check_correlated_matrix(
        &graph,
        "RETURN 1 AS a NEXT MATCH (n:Person) RETURN n LIMIT 2",
    );
}

#[test]
fn variable_length_inner_patterns_run_without_a_row_suffix() {
    let graph = person_graph();
    let source = "MATCH (a:Person) FILTER a.age > 20 MATCH (a)-[:KNOWS*1..2]->(b) RETURN a, b";
    let plan = plan_source(source);
    batch_prefix_with_policy(&graph, &plan, BatchPolicy::default_policy())
        .expect("complete physical path match");
    let expected = row_table(&graph, source);
    let actual = production_table(&graph, source);
    assert_tables_equivalent(&expected, &actual, "batch path match composition");
}

#[test]
fn outer_operator_matches_row_tree_directly() {
    // Direct BatchOuterJoin over a hand-split pattern: the left scan runs
    // batched while each right evaluation seeds from its own left row.
    let graph = person_graph();
    let plan = plan_source("MATCH (m:Robot) OPTIONAL MATCH (m)-[e:KNOWS]->(n) RETURN m, e, n");
    let pattern = plan.pattern_plan.as_ref().expect("pattern");
    let JoinTree::Outer {
        left,
        right,
        key,
        right_filters,
    } = &pattern.join_tree
    else {
        panic!("expected a top-level outer join");
    };
    let schema = crate::runtime::pattern::schema_for_pattern(pattern);
    let tx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let eval = crate::runtime::EvalCtx {
        tx: &tx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let policy = BatchPolicy::new(1, 1 << 20).unwrap();
    let left_op =
        build_join_tree(left, pattern, schema.clone(), eval, policy, None).expect("left builds");
    let mut outer = BatchOuterJoin::new(
        left_op,
        right,
        pattern,
        key,
        right_filters,
        schema.clone(),
        eval,
        policy,
    );
    assert_eq!(outer.state(), OperatorState::Created);
    let mut ctx = BatchExecutionContext::borrowed(
        tx.snapshot(),
        tx.batch_cancel(),
        MemoryBudget::unlimited(),
    );
    outer.init(&mut ctx).expect("outer inits");
    assert_eq!(outer.state(), OperatorState::Open);
    let rows = pull_all(&mut outer, &mut ctx);
    assert_eq!(outer.state(), OperatorState::Exhausted);
    assert_eq!(
        outer.batches_produced(),
        2,
        "two preserved rows pull singly"
    );
    outer.close(&mut ctx);
    assert_eq!(outer.state(), OperatorState::Closed);
    let table = BindingTable::new(schema, rows.into_iter().map(Binding::new).collect());
    assert_tables_equivalent(
        &row_table(
            &graph,
            "MATCH (m:Robot) OPTIONAL MATCH (m)-[e:KNOWS]->(n) RETURN m, e, n",
        ),
        &table,
        "direct outer",
    );
}

#[test]
fn match_operator_matches_row_pipeline_directly() {
    // Direct BatchMatch over row-oracle input rows: per-row seeded
    // evaluation with the row path's target schema, isolated across rows.
    // Alice matches (one edge), the remaining seven persons do not: any
    // cross-row leak would surface as a non-null `b` on an unmatched row.
    let graph = person_graph();
    let plan = plan_source(
        "MATCH (a:Person) FILTER a.age > 20 OPTIONAL MATCH (a)-[e:KNOWS]->(b) RETURN a, e, b",
    );
    let (match_pattern, optional) = match plan.pipeline.as_slice() {
        [_, crate::PipelineOp::OptionalMatch(pattern), ..] => (pattern, true),
        _ => panic!("expected a project plus optional match prefix"),
    };
    let input = row_table(&graph, "MATCH (a:Person) FILTER a.age > 20 RETURN a");
    assert_eq!(input.row_count(), 8);
    let input_schema = input.schema().clone();
    let target = crate::runtime::pipeline::target_schema(&input_schema, match_pattern);
    let tx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let eval = crate::runtime::EvalCtx {
        tx: &tx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let policy = BatchPolicy::new(2, 1 << 20).unwrap();
    let mut matcher = BatchMatch::new(
        Box::new(BatchRowSource::new(input, policy)),
        match_pattern,
        target.clone(),
        input_schema,
        optional,
        eval,
        policy,
    );
    assert_eq!(matcher.state(), OperatorState::Created);
    let mut ctx = BatchExecutionContext::borrowed(
        tx.snapshot(),
        tx.batch_cancel(),
        MemoryBudget::unlimited(),
    );
    matcher.init(&mut ctx).expect("match inits");
    assert_eq!(matcher.state(), OperatorState::Open);
    let rows = pull_all(&mut matcher, &mut ctx);
    matcher.close(&mut ctx);
    assert_eq!(matcher.state(), OperatorState::Closed);
    let table = BindingTable::new(target, rows.into_iter().map(Binding::new).collect());
    assert_tables_equivalent(
        &row_table(
            &graph,
            "MATCH (a:Person) FILTER a.age > 20 OPTIONAL MATCH (a)-[e:KNOWS]->(b) RETURN a, e, b",
        ),
        &table,
        "direct match",
    );
    // Six of eight rows preserve nulls: no matched binding leaked.
    assert_eq!(
        table
            .rows()
            .iter()
            .filter(|row| matches!(row.values(), [_, _, Value::Null]))
            .count(),
        6,
        "unmatched rows keep null bindings"
    );
}

#[test]
fn chain_operators_match_row_blocks_directly() {
    // Direct BatchChain / BatchCorrelatedChain over row-source inputs with
    // blocks extracted from NEXT plans.
    let graph = person_graph();
    let plan = plan_source("RETURN 1 AS a NEXT RETURN 2 AS b");
    let rhs = match plan.pipeline.as_slice() {
        [_, crate::PipelineOp::Chain(rhs)] => rhs.as_ref(),
        _ => panic!("expected uncorrelated chain"),
    };
    let tx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let eval = crate::runtime::EvalCtx {
        tx: &tx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let policy = BatchPolicy::default_policy();
    let input = BindingTable::new(
        plan_source("RETURN 1 AS a").output_schema.clone(),
        vec![Binding::new([Value::Int(1)])],
    );
    let mut chain = BatchChain::new(
        Box::new(BatchRowSource::new(input, policy)),
        rhs,
        eval,
        policy,
    );
    assert_eq!(chain.state(), OperatorState::Created);
    let mut ctx = BatchExecutionContext::borrowed(
        tx.snapshot(),
        tx.batch_cancel(),
        MemoryBudget::unlimited(),
    );
    chain.init(&mut ctx).expect("chain inits");
    let rows = pull_all(&mut chain, &mut ctx);
    chain.close(&mut ctx);
    assert_same_rows(&rows, &[vec![Value::Int(2)]], "chain runs its block");

    let plan = plan_source("FOR a IN [1, 2] RETURN a NEXT RETURN a + 10 AS b");
    let rhs = match plan.pipeline.as_slice() {
        [_, _, crate::PipelineOp::CorrelatedChain(rhs)] => rhs.as_ref(),
        _ => panic!("expected correlated chain, got {:?}", plan.pipeline.len()),
    };
    let input = BindingTable::new(
        plan_source("FOR a IN [1, 2] RETURN a")
            .output_schema
            .clone(),
        vec![Binding::new([Value::Int(1)]), Binding::new([Value::Int(2)])],
    );
    let mut correlated = BatchCorrelatedChain::new(
        Box::new(BatchRowSource::new(input, policy)),
        rhs,
        eval,
        policy,
    );
    let mut ctx = BatchExecutionContext::borrowed(
        tx.snapshot(),
        tx.batch_cancel(),
        MemoryBudget::unlimited(),
    );
    correlated.init(&mut ctx).expect("correlated chain inits");
    assert_eq!(correlated.state(), OperatorState::Open);
    let rows = pull_all(&mut correlated, &mut ctx);
    correlated.close(&mut ctx);
    assert_same_rows(
        &rows,
        &[vec![Value::Int(11)], vec![Value::Int(12)]],
        "correlated chain seeds per row",
    );
}

#[test]
fn bounded_budget_fails_correlated_fanout_without_partial_output() {
    // A byte-sized budget fails the match output reservation before any
    // row materializes: the failure is typed with no partial table.
    let graph = person_graph();
    let plan = plan_source(
        "MATCH (a:Person) FILTER a.age > 20 OPTIONAL MATCH (a)-[e:KNOWS]->(b) RETURN a, e, b",
    );
    let (match_pattern, optional) = match plan.pipeline.as_slice() {
        [_, crate::PipelineOp::OptionalMatch(pattern), ..] => (pattern, true),
        _ => panic!("expected a project plus optional match prefix"),
    };
    let input = row_table(&graph, "MATCH (a:Person) FILTER a.age > 20 RETURN a");
    let input_schema = input.schema().clone();
    let target = crate::runtime::pipeline::target_schema(&input_schema, match_pattern);
    let tx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let eval = crate::runtime::EvalCtx {
        tx: &tx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let policy = BatchPolicy::default_policy();
    let mut matcher = BatchMatch::new(
        Box::new(BatchRowSource::new(input, policy)),
        match_pattern,
        target,
        input_schema,
        optional,
        eval,
        policy,
    );
    let mut ctx =
        BatchExecutionContext::borrowed(tx.snapshot(), tx.batch_cancel(), MemoryBudget::new(1));
    let err = matcher.init(&mut ctx).expect_err("bounded match must fail");
    assert!(
        matches!(err, ExecutorError::ProgramLimitExceeded { .. }),
        "typed resource error, got {err:?}"
    );
    assert_eq!(err.gqlstatus().as_str(), "5GQL1");
    assert_eq!(matcher.state(), OperatorState::Failed);
    matcher.close(&mut ctx);
}
