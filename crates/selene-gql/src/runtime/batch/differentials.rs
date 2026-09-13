//! Primitive batch-policy and indexed-versus-scan differentials.
//!
//! Each test maps to one acceptance case from the slice brief:
//!
//! - indexed versus scan execution agrees on values, duplicates, errors, and
//!   schema for the same query;
//! - null filters, missing properties, and wrong-type operands keep profile
//!   behavior;
//! - OFFSET/LIMIT combinations span empty and intermediate batches (including
//!   zero and exhaustion) across multiple batch sizes;
//! - a small LIMIT after a multiplicity-producing expansion returns the
//!   correct rows (no premature seed limiting, no missing pushdown);
//! - mixed-edge loops and parallel edges keep their bindings;
//! - stale candidates fail predictably instead of rebinding silently.
//!
//! F04-PR09 ran the old row comparisons before deleting that implementation.
//! The single-row-policy comparison now tests partition invariance, not an
//! independent oracle. Literal expectations and the separate relation model
//! supply independent semantic assertions.

use std::time::Instant;

use selene_core::GraphId;
use selene_graph::SharedGraph;

use crate::{
    ExecutionPlan, JoinTree, ScanAccess,
    runtime::{Binding, BindingTable, ExecutorError, TxContext},
};
use selene_testing::mixed_orientation::MixedOrientationFixture;

use super::candidates::ResolvedCandidates;
use super::expand::BatchExpand;
use super::filter::BatchFilter;
use super::fixtures::{
    batch_prefix_with_policy, eval_for, execute_single_row_batches, hub_graph,
    indexed_person_graph, oddball_graph, optimized_plan, person_graph, plan_source,
    production_table, row_table, seed_nodes,
};
use super::page::BatchPage;
use super::project::BatchProject;
use super::scan::BatchScan;
use super::{
    BatchBuffer, BatchCancel, BatchExecutionContext, BatchPolicy, MemoryBudget, OperatorState,
    PhysicalOperator, assert_tables_equivalent,
};

/// Batch sizes for the boundary matrix: single-row pulls, small windows that
/// split fixtures awkwardly, and the production default.
fn policies() -> Vec<BatchPolicy> {
    [1usize, 2, 3, 7, 1024]
        .into_iter()
        .map(|target| BatchPolicy::new(target, 1 << 20).unwrap())
        .collect()
}

/// Execute an already-planned query with the single-row batch policy.
fn row_execute(graph: &SharedGraph, plan: &ExecutionPlan) -> Result<BindingTable, ExecutorError> {
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    execute_single_row_batches(plan, &mut ctx)
}

/// Assert a fully-batch plan agrees with the row oracle under `policy`.
///
/// Fully-batch means the driver covers the pattern and the whole pipeline
/// (`suffix_from` reaches the pipeline end). Both success (identical schema
/// and rows, in order) and failure (identical GQLSTATUS) must agree: a batch
/// error where the row succeeds (or vice versa) is a divergence, and a
/// decline is a test bug for shapes this helper covers.
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

/// Assert agreement for `source` (linear plan) across every matrix policy.
fn check_query_matrix(graph: &SharedGraph, source: &str) {
    let plan = plan_source(source);
    for policy in policies() {
        check_full_agree(graph, &plan, policy, source);
    }
}

#[test]
fn primitive_and_path_shapes_execute_completely() {
    let graph = person_graph();
    // Primitive shapes run fully in batches (no row suffix remains).
    for source in [
        "MATCH (n) RETURN n",
        "MATCH (n:Person) WHERE n.age > 22 RETURN n.name AS name LIMIT 3 OFFSET 1",
        "MATCH (a)-[e:KNOWS]->(b) RETURN a, e, b",
        "RETURN 1 AS one",
    ] {
        let plan = plan_source(source);
        batch_prefix_with_policy(&graph, &plan, BatchPolicy::default_policy())
            .expect("complete primitive execution");
    }
    // F05-PR04 includes variable-length paths in the physical prefix.
    let plan = plan_source("MATCH (a)-[:KNOWS*1..2]->(b) RETURN a, b");
    batch_prefix_with_policy(&graph, &plan, BatchPolicy::default_policy())
        .expect("complete path execution");
}

#[test]
fn indexed_and_scan_execution_agree() {
    let graph = indexed_person_graph();
    // Label-index access: the intrinsic label bitmap is always available to
    // the live catalog, so this optimized plan must leave Linear behind.
    let plan = optimized_plan("MATCH (n:Person) RETURN n", &graph);
    assert_needs_index(&plan, "label scan");
    // Typed-index equality and range access over the real age index.
    let plan = optimized_plan("MATCH (n:Person) WHERE n.age = 24 RETURN n", &graph);
    assert_needs_index(&plan, "equality lookup");
    let plan = optimized_plan(
        "MATCH (n:Person) WHERE n.age > 25 RETURN n.name AS name",
        &graph,
    );
    assert_needs_index(&plan, "range lookup");
    // Agreement across policies for each optimized shape, plus agreement
    // between the optimized (indexed) batch run and the linear row run: the
    // index must not change values, duplicates, errors, or schema.
    for source in [
        "MATCH (n:Person) RETURN n",
        "MATCH (n:Person) WHERE n.age = 24 RETURN n",
        "MATCH (n:Person) WHERE n.age > 25 RETURN n.name AS name",
    ] {
        let linear = plan_source(source);
        let expected = row_execute(&graph, &linear).expect("linear row executes");
        let optimized = optimized_plan(source, &graph);
        for policy in policies() {
            check_full_agree(&graph, &optimized, policy, source);
            let batch = batch_prefix_with_policy(&graph, &optimized, policy)
                .expect("driver executes indexed shapes");
            assert_tables_equivalent(&expected, &batch, source);
        }
    }
}

/// Assert an optimized plan actually exercises an index access path.
///
/// A vacuous all-Linear "indexed" test would prove nothing; fail loudly so
/// the fixture grows until the optimizer bites.
fn assert_needs_index(plan: &ExecutionPlan, what: &str) {
    let pattern = plan.pattern_plan.as_ref().expect("test plan has a pattern");
    let JoinTree::Scan(scan) = &pattern.join_tree else {
        panic!("{what}: expected a single scan");
    };
    assert!(
        !matches!(scan.access, ScanAccess::Linear),
        "{what}: optimizer left the scan Linear"
    );
}

#[test]
fn null_missing_and_wrong_type_filters_agree() {
    let graph = oddball_graph();
    // Missing properties and wrong-type operands filter to empty or false on
    // both paths; profile behavior is whatever the shared helpers do, as
    // long as both paths do it identically.
    for source in [
        "MATCH (n:Person) WHERE n.age = 40 RETURN n",
        "MATCH (n:Person) WHERE n.missing = 1 RETURN n",
        "MATCH (n:Person) WHERE n.age = 'old' RETURN n",
        "MATCH (n:Person) WHERE n.age > 30 RETURN n",
        "MATCH (n:Person) WHERE n.age <> 40 RETURN n",
    ] {
        check_query_matrix(&graph, source);
    }
}

#[test]
fn computed_projection_errors_are_preserved() {
    // A wrong-type operand inside a projection errors on both paths with the
    // same GQLSTATUS: filters never suppress errors into drops, and the
    // batch projector aborts exactly like the row projector.
    let graph = oddball_graph();
    check_query_matrix(&graph, "MATCH (n:Person) RETURN n.age + 1 AS next_age");
    // Same guarantee through a filter position.
    check_query_matrix(&graph, "MATCH (n:Person) WHERE n.age + 1 > 40 RETURN n");
    // Proven-safe leading pushdown: the first row carries the good integer
    // age, so both paths truncate before the bad rows and succeed with one
    // identical row instead of erroring.
    let plan = plan_source("MATCH (n:Person) LIMIT 1 RETURN n.age + 1 AS next_age");
    for policy in policies() {
        check_full_agree(&graph, &plan, policy, "leading-limit pushdown");
    }
    let expected = row_table(
        &graph,
        "MATCH (n:Person) LIMIT 1 RETURN n.age + 1 AS next_age",
    );
    assert_eq!(
        expected.row_count(),
        1,
        "pushdown truncates before bad rows"
    );
}

#[test]
fn offset_limit_matrix_spans_batches() {
    let graph = person_graph();
    // Ten nodes: offsets and counts hit empty results, exact exhaustion,
    // overrun, and windows straddling batch boundaries under every policy.
    // `check_query_matrix` fans each query over single-row, awkward-window,
    // and default batch sizes.
    for offset in [0u64, 1, 8, 9, 10, 100] {
        for count in [0u64, 1, 2, 5, 9, 10] {
            check_query_matrix(
                &graph,
                &format!("MATCH (n) RETURN n LIMIT {count} OFFSET {offset}"),
            );
        }
    }
    // Filtered windows leave intermediate batches empty under small policies:
    // skipping must not reset per batch, and the surviving schema must match.
    for offset in [0u64, 1, 4] {
        for count in [0u64, 1, 3] {
            check_query_matrix(
                &graph,
                &format!(
                    "MATCH (n:Person) WHERE n.age <> 24 RETURN n.name AS name LIMIT {count} OFFSET {offset}"
                ),
            );
        }
    }
    // The proven-safe pushdown shapes agree too: leading limits and safe
    // post-return limits truncate the pattern identically on both paths.
    check_query_matrix(&graph, "MATCH (n) LIMIT 3 RETURN n");
    check_query_matrix(&graph, "MATCH (n) RETURN n LIMIT 0");
}

#[test]
fn small_limit_after_expansion_returns_correct_rows() {
    let graph = hub_graph();
    // Fourteen KNOWS edges (twelve spokes plus a parallel duplicate and a
    // loop): a small LIMIT must return the first rows of the full expansion,
    // never a prematurely limited seed, under every batch size.
    for source in [
        "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN s LIMIT 2",
        "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN s LIMIT 2 OFFSET 3",
        "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN s LIMIT 0",
        "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN h, e, s",
    ] {
        check_query_matrix(&graph, source);
    }
    let expected = row_table(&graph, "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN s LIMIT 2");
    assert_eq!(expected.row_count(), 2, "limit returns two rows");
    // Full expansion keeps parallel-edge multiplicity: spoke zero appears
    // twice, and the loop contributes its row.
    let full = row_table(&graph, "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN s");
    assert_eq!(full.row_count(), 14, "expansion keeps every edge row");
    let limited = production_table(&graph, "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN s LIMIT 2");
    assert_eq!(
        &collect_limited_ids(&limited),
        &collect_limited_ids(&expected),
        "production wiring returns the same limited rows"
    );
}

fn collect_limited_ids(table: &BindingTable) -> Vec<String> {
    table
        .rows()
        .iter()
        .map(|row| format!("{:?}", row.values()))
        .collect()
}

#[test]
fn indexed_edge_expansion_agrees() {
    let graph = hub_graph();
    // Inline edge property maps select the edge typed-index path; with one
    // matching edge against one seed row the indexed-expansion branch
    // (filter no larger than the child input) deterministically runs.
    let plan = optimized_plan(
        "MATCH (h:Hub)-[e:KNOWS {score: 7}]->(s) RETURN h, e, s",
        &graph,
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern");
    let JoinTree::Expand { edge, .. } = &pattern.join_tree else {
        panic!("expected a single expansion");
    };
    assert!(
        !matches!(edge.access, ScanAccess::Linear),
        "optimizer left the edge access Linear"
    );
    for policy in policies() {
        check_full_agree(
            &graph,
            &plan,
            policy,
            "MATCH (h:Hub)-[e:KNOWS {score: 7}]->(s) RETURN h, e, s",
        );
    }
}

#[test]
fn mixed_edge_loops_and_parallel_edges_agree() {
    // Reverse-created undirected edges, parallel identities, and both loop
    // kinds: batch expansion must keep every F01-PR04 binding.
    let fixture = MixedOrientationFixture::build();
    let graph = &fixture.graph;
    for source in [
        "MATCH (a:N)-[e:E]->(b:N) RETURN a, e, b",
        "MATCH (a:N)-[e:E]-(b:N) RETURN a, e, b",
        "MATCH (a:N)~[e:E]~(b:N) RETURN a, e, b",
        "MATCH (a:A)-[e:E]->(b:B) RETURN a, e, b",
    ] {
        check_query_matrix(graph, source);
    }
}

#[test]
fn stale_candidates_fail_predictably() {
    let graph = SharedGraph::new(GraphId::new(42_100));
    seed_nodes(&graph, 2, Some("Thing"));
    let plan = plan_source("MATCH (n:Thing) RETURN n");
    let (scan_ir, pattern) = super::fixtures::scan_parts(&plan);
    let schema = row_table(&graph, "MATCH (n:Thing) RETURN n")
        .schema()
        .clone();

    // Resolve against the first snapshot, then advance the generation.
    let snap_a = graph.read();
    let tx_a = TxContext::read_only(
        snap_a.clone(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let eval_a = crate::runtime::EvalCtx {
        tx: &tx_a,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let resolved = ResolvedCandidates::resolve(scan_ir, &eval_a).expect("candidates resolve");
    seed_nodes(&graph, 1, Some("Thing"));
    let snap_b = graph.read();
    assert_ne!(
        snap_a.meta.generation, snap_b.meta.generation,
        "fixture write advances the generation"
    );

    // Binding-level: validation against the new snapshot fails loudly.
    let err = resolved
        .binding()
        .validate(&snap_b)
        .expect_err("stale binding must fail");
    assert!(
        matches!(err, ExecutorError::ImplementationDefined { .. }),
        "stale candidates fail predictably, got {err:?}"
    );

    // Operator-level: a scan resolving against snapshot B but pinned to
    // snapshot A fails at init instead of rebinding physical rows.
    let tx_b = TxContext::read_only(
        snap_b,
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let eval_b = crate::runtime::EvalCtx {
        tx: &tx_b,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let mut scan = BatchScan::new(
        scan_ir,
        pattern,
        schema,
        eval_b,
        BatchPolicy::default_policy(),
    );
    let mut ctx =
        BatchExecutionContext::new(snap_a, BatchCancel::disabled(), MemoryBudget::unlimited());
    let err = scan
        .init(&mut ctx)
        .expect_err("cross-snapshot init must fail");
    assert!(
        matches!(err, ExecutorError::ImplementationDefined { .. }),
        "cross-snapshot scan init fails predictably, got {err:?}"
    );
    scan.close(&mut ctx);
    drop(scan);
    drop(tx_a);
    drop(tx_b);
}

#[test]
fn cached_plans_re_resolve_per_execution() {
    // Plans cache access paths, never candidates: executing one planned query
    // across a generation bump observes fresh rows on both paths.
    let graph = SharedGraph::new(GraphId::new(42_101));
    seed_nodes(&graph, 2, Some("Thing"));
    let plan = plan_source("MATCH (n:Thing) RETURN n");
    let first = production_table(&graph, "MATCH (n:Thing) RETURN n");
    assert_eq!(first.row_count(), 2);
    seed_nodes(&graph, 1, Some("Thing"));
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let second =
        super::super::plan_runner::execute_plan(&plan, &mut ctx).expect("re-execution works");
    assert_eq!(second.row_count(), 3, "cached plan observes fresh rows");
    assert_tables_equivalent(
        &row_table(&graph, "MATCH (n:Thing) RETURN n"),
        &second,
        "re-executed cached plan",
    );
}

#[test]
fn complete_batch_composition_preserves_outcomes() {
    // Native, ordering, optional, and set operators run through the row
    // dispatcher on batch-materialized prefixes: structured outcomes
    // (schemas, multiplicities, order) survive result conversion.
    let graph = person_graph();
    for source in [
        "MATCH (n:Person) RETURN n AS x UNION ALL MATCH (m:Robot) RETURN m AS x",
        "MATCH (n:Person) RETURN n.age AS age ORDER BY age",
        "MATCH (n:Person) OPTIONAL MATCH (n)-[e:KNOWS]->(m) RETURN n, e, m",
        "RETURN 1 AS one",
        "RETURN 1 + 2 AS three",
    ] {
        let expected = row_table(&graph, source);
        let actual = production_table(&graph, source);
        assert_tables_equivalent(&expected, &actual, source);
    }
}

#[allow(clippy::print_stdout)]
#[test]
fn perf_probe_reports_primitive_numbers() {
    // Observed numbers only: no timing assertions. Shapes cover tiny
    // queries, selective filters, high-degree expansion, and
    // limit-short-circuit work. Rows visited, batch counts, and elapsed
    // microseconds print per shape for batch and row sides alike.
    tiny_probe();
    filter_probe();
    expand_probe();
    limit_probe();
}

#[allow(clippy::print_stdout)]
fn tiny_probe() {
    let graph = SharedGraph::new(GraphId::new(42_200));
    // Warmup so the timed samples exclude cold allocator and code-cache costs.
    let _ = production_table(&graph, "RETURN 1 AS one");
    let _ = row_table(&graph, "RETURN 1 AS one");
    let started = Instant::now();
    let batch = production_table(&graph, "RETURN 1 AS one");
    let batch_us = started.elapsed().as_micros();
    let started = Instant::now();
    let rowed = row_table(&graph, "RETURN 1 AS one");
    let row_us = started.elapsed().as_micros();
    assert_eq!(batch.row_count(), rowed.row_count());
    println!("batch-primitive probe: shape=tiny batch_us={batch_us} row_us={row_us}");
}

#[allow(clippy::print_stdout)]
fn filter_probe() {
    let graph = SharedGraph::new(GraphId::new(42_201));
    seed_nodes(&graph, 10_000, None);
    let source = "MATCH (n) WHERE false RETURN n";
    let _ = production_table(&graph, source);
    let _ = row_table(&graph, source);
    let started = Instant::now();
    let batch = production_table(&graph, source);
    let batch_us = started.elapsed().as_micros();
    let started = Instant::now();
    let rowed = row_table(&graph, source);
    let row_us = started.elapsed().as_micros();
    assert_eq!(batch.row_count(), 0);
    assert_eq!(rowed.row_count(), 0);
    println!(
        "batch-primitive probe: shape=selective-filter rows=10000 batch_us={batch_us} row_us={row_us}"
    );
}

#[allow(clippy::print_stdout)]
fn expand_probe() {
    let graph = hub_graph();
    let source = "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN s";
    let _ = production_table(&graph, source);
    let _ = row_table(&graph, source);
    let started = Instant::now();
    let batch = production_table(&graph, source);
    let batch_us = started.elapsed().as_micros();
    let started = Instant::now();
    let rowed = row_table(&graph, source);
    let row_us = started.elapsed().as_micros();
    assert_eq!(batch.row_count(), rowed.row_count());
    println!(
        "batch-primitive probe: shape=high-degree-expand rows={} batch_us={batch_us} row_us={row_us}",
        batch.row_count()
    );
}

#[allow(clippy::print_stdout)]
fn limit_probe() {
    let graph = SharedGraph::new(GraphId::new(42_202));
    seed_nodes(&graph, 10_000, None);
    let source = "MATCH (n) RETURN n LIMIT 5";
    let _ = production_table(&graph, source);
    let _ = row_table(&graph, source);
    let started = Instant::now();
    let batch = production_table(&graph, source);
    let batch_us = started.elapsed().as_micros();
    let started = Instant::now();
    let rowed = row_table(&graph, source);
    let row_us = started.elapsed().as_micros();
    assert_eq!(batch.row_count(), 5);
    assert_eq!(rowed.row_count(), 5);
    println!(
        "batch-primitive probe: shape=limit-short-circuit rows=10000 limit=5 batch_us={batch_us} row_us={row_us}"
    );
}

/// Pull one operator tree to owned rows, releasing budget and recycling.
fn pull_tree(
    root: &mut dyn PhysicalOperator,
    exec: &mut BatchExecutionContext<'_>,
) -> Vec<Vec<selene_core::Value>> {
    let mut buffer = BatchBuffer::new();
    let mut rows = Vec::new();
    while let Some(batch) = root.next_batch(exec, &mut buffer).expect("pulls succeed") {
        rows.extend(batch.logical_rows_vec());
        exec.budget_mut().release(batch.estimated_bytes());
        batch.recycle(&mut buffer);
    }
    rows
}

/// Borrowed execution context over a test transaction context.
fn borrowed_exec<'a, 't>(tx: &'a TxContext<'t, 't>) -> BatchExecutionContext<'a> {
    BatchExecutionContext::borrowed(tx.snapshot(), tx.batch_cancel(), MemoryBudget::unlimited())
}

#[test]
fn filter_operator_keeps_only_true_and_reports_state() {
    // Direct BatchFilter over single-row-batch scans: the computed pattern
    // predicate keeps True rows and drops the rest, with lifecycle reported.
    let graph = person_graph();
    let source = "MATCH (n:Person) WHERE n.age + 0 > 25 RETURN n";
    let plan = plan_source(source);
    let pattern = plan.pattern_plan.as_ref().expect("pattern");
    assert_eq!(
        pattern.filters.len(),
        1,
        "computed WHERE stays one pattern filter"
    );
    let schema = super::super::pattern::schema_for_pattern(pattern);
    let JoinTree::Scan(scan_ir) = &pattern.join_tree else {
        panic!("expected a single scan");
    };
    let tx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let policy = BatchPolicy::new(1, 1 << 20).unwrap();
    let scan = BatchScan::new(
        scan_ir,
        pattern,
        schema.clone(),
        eval_for(&tx, &plan),
        policy,
    );
    let mut filter = BatchFilter::new(Box::new(scan), &pattern.filters[0], eval_for(&tx, &plan));
    assert_eq!(filter.state(), OperatorState::Created);
    let mut exec = borrowed_exec(&tx);
    filter.init(&mut exec).expect("filter inits");
    assert_eq!(filter.state(), OperatorState::Open);
    let rows = pull_tree(&mut filter, &mut exec);
    assert_eq!(filter.state(), OperatorState::Exhausted);
    filter.close(&mut exec);
    assert_eq!(filter.state(), OperatorState::Closed);
    let table = BindingTable::new(schema, rows.into_iter().map(Binding::new).collect());
    assert_tables_equivalent(&row_table(&graph, source), &table, "direct filter");
}

#[test]
fn project_operator_evaluates_items_and_reports_state() {
    // Direct BatchProject over single-row-batch scans: every projection item
    // evaluates per logical row through the shared evaluator.
    let graph = person_graph();
    let source = "MATCH (n) RETURN n.name AS name";
    let plan = plan_source(source);
    let pattern = plan.pattern_plan.as_ref().expect("pattern");
    let schema = super::super::pattern::schema_for_pattern(pattern);
    let JoinTree::Scan(scan_ir) = &pattern.join_tree else {
        panic!("expected a single scan");
    };
    let items = match plan.pipeline.as_slice() {
        [crate::PipelineOp::Project(items)] => items,
        _ => panic!("expected a lone projection"),
    };
    let output_schema = super::super::pipeline::schema_for_items(items);
    let tx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let policy = BatchPolicy::new(1, 1 << 20).unwrap();
    let scan = BatchScan::new(scan_ir, pattern, schema, eval_for(&tx, &plan), policy);
    let mut project = BatchProject::new(Box::new(scan), items, output_schema, eval_for(&tx, &plan));
    assert_eq!(project.state(), OperatorState::Created);
    let mut exec = borrowed_exec(&tx);
    project.init(&mut exec).expect("project inits");
    let rows = pull_tree(&mut project, &mut exec);
    assert_eq!(project.state(), OperatorState::Exhausted);
    project.close(&mut exec);
    let table = BindingTable::new(
        project.output_schema().clone(),
        rows.into_iter().map(Binding::new).collect(),
    );
    assert_tables_equivalent(&row_table(&graph, source), &table, "direct project");
}

#[test]
fn page_operator_counters_survive_batch_boundaries() {
    // Direct BatchPage over two-row batches: skip/take counters are
    // operator-level, so windows straddling batch boundaries stay exact and
    // the surviving rows match the row oracle slice.
    let graph = person_graph();
    let source = "MATCH (n) RETURN n";
    let plan = plan_source(source);
    let pattern = plan.pattern_plan.as_ref().expect("pattern");
    let schema = super::super::pattern::schema_for_pattern(pattern);
    let JoinTree::Scan(scan_ir) = &pattern.join_tree else {
        panic!("expected a single scan");
    };
    let tx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let policy = BatchPolicy::new(2, 1 << 20).unwrap();
    let scan = BatchScan::new(scan_ir, pattern, schema, eval_for(&tx, &plan), policy);
    let mut page = BatchPage::new(Box::new(scan), 3, 4);
    let mut exec = borrowed_exec(&tx);
    page.init(&mut exec).expect("page inits");
    let rows = pull_tree(&mut page, &mut exec);
    assert_eq!(page.skipped(), 3, "skip counter is operator-level");
    assert_eq!(page.emitted(), 4, "take counter is operator-level");
    assert_eq!(page.state(), OperatorState::Exhausted);
    page.close(&mut exec);
    let expected = row_table(&graph, source);
    let window = BindingTable::new(expected.schema().clone(), expected.rows()[3..7].to_vec());
    let table = BindingTable::new(
        window.schema().clone(),
        rows.into_iter().map(Binding::new).collect(),
    );
    assert_tables_equivalent(&window, &table, "direct page window");
}

#[test]
fn expand_operator_splits_output_and_reports_counts() {
    // Direct BatchExpand over two-row batches: fourteen edge rows (twelve
    // spokes plus a parallel duplicate and a loop) stream out in multiple
    // pulls with exact row content and reported counts.
    let graph = hub_graph();
    let source = "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN h, e, s";
    let plan = plan_source(source);
    let pattern = plan.pattern_plan.as_ref().expect("pattern");
    let schema = super::super::pattern::schema_for_pattern(pattern);
    let JoinTree::Expand {
        child,
        edge,
        direction,
    } = &pattern.join_tree
    else {
        panic!("expected one expansion");
    };
    let JoinTree::Scan(scan_ir) = child.as_ref() else {
        panic!("expected a scan child");
    };
    let tx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &crate::EmptyProcedureRegistry,
        graph.index_providers(),
    );
    let policy = BatchPolicy::new(2, 1 << 20).unwrap();
    let scan = BatchScan::new(
        scan_ir,
        pattern,
        schema.clone(),
        eval_for(&tx, &plan),
        policy,
    );
    let mut expand = BatchExpand::new(
        Box::new(scan),
        edge,
        *direction,
        pattern,
        schema,
        eval_for(&tx, &plan),
        policy,
    );
    let mut exec = borrowed_exec(&tx);
    expand.init(&mut exec).expect("expand inits");
    let rows = pull_tree(&mut expand, &mut exec);
    assert_eq!(rows.len(), 14, "expansion keeps every edge row");
    assert_eq!(expand.output_count(), 14, "reported count matches");
    assert!(
        expand.batches_produced() > 1,
        "output splits across pulls at size two"
    );
    assert_eq!(expand.state(), OperatorState::Exhausted);
    expand.close(&mut exec);
    let table = BindingTable::new(
        expand.output_schema().clone(),
        rows.into_iter().map(Binding::new).collect(),
    );
    assert_tables_equivalent(&row_table(&graph, source), &table, "direct expand");
}
