//! Join policy differentials, hand-derived results and independent relation models.
//!
//! Each facade query runs through logical planning and the stable result
//! boundary on both paths; agreement must hold across the fixed and
//! randomized batch-size matrices. Hand-derived counts and node identities
//! on key shapes serve as the independent check alongside the row oracle,
//! while the relation-model multiset proofs live in [`super::join_tests`].

use selene_core::{GraphId, LabelSet, PropertyMap, Value, db_string};
use selene_graph::SharedGraph;

use crate::{
    BuildSide, ExecutionPlan, JoinTree,
    runtime::{BindingTable, ExecutorError, TxContext},
};

use super::fixtures::{
    batch_prefix_with_policy, hub_graph, kernel_ctx, pair, pair_schema, person_graph, plan_source,
    production_table, row_table, seed_nodes,
};
use super::join::{hash_join_rows, nested_loop_join_rows};
use super::{BatchPolicy, MemoryBudget, assert_tables_equivalent};

/// Batch sizes for the boundary matrix: single-row pulls, awkward windows,
/// and the production default.
fn policies() -> Vec<BatchPolicy> {
    [1usize, 2, 3, 7, 1024]
        .into_iter()
        .map(|target| BatchPolicy::new(target, 1 << 20).unwrap())
        .collect()
}

/// Deterministic pseudo-random batch sizes (seeded LCG, no new
/// dependencies): awkward windows the fixed matrix misses.
fn randomized_policies() -> Vec<BatchPolicy> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    (0..10)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let target = 1 + (state >> 33) as usize % 24;
            BatchPolicy::new(target, 1 << 20).unwrap()
        })
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
    super::fixtures::execute_single_row_batches(plan, &mut ctx)
}

/// Assert a fully-batch plan agrees with the row oracle under `policy`.
///
/// Both success (identical schema and rows, in order) and failure
/// (identical GQLSTATUS) must agree; a decline is a test bug for shapes
/// this helper covers.
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
fn check_join_matrix(graph: &SharedGraph, source: &str) {
    let plan = plan_source(source);
    for policy in policies().into_iter().chain(randomized_policies()) {
        check_full_agree(graph, &plan, policy, source);
    }
}

fn set_build_side(plan: &mut ExecutionPlan, side: BuildSide) {
    let pattern = plan.pattern_plan.as_mut().expect("test plan has a pattern");
    let JoinTree::HashJoin { build_side, .. } = &mut pattern.join_tree else {
        panic!("expected a top-level hash join");
    };
    *build_side = side;
}

#[test]
fn comma_join_matches_row_oracle_across_sizes() {
    let graph = person_graph();
    // Hand-derived expectation (independent of both engines): Alice(1)->Bob
    // and Bob(2)->Cara are the only KNOWS edges, so the shared-`a` join
    // keeps exactly two bindings in probe-major order.
    let expected = row_table(
        &graph,
        "MATCH (a:Person) MATCH (a)-[:KNOWS]->(b) RETURN a, b",
    );
    assert_eq!(expected.row_count(), 2);
    for source in [
        "MATCH (a:Person) MATCH (a)-[:KNOWS]->(b) RETURN a, b",
        "MATCH (a:Person), (b:Robot) RETURN a, b",
        "MATCH (a:Sensor) MATCH (a)-[:KNOWS]->(b) RETURN a, b",
    ] {
        check_join_matrix(&graph, source);
    }
    // Cross product keeps every pair: eight persons times two robots.
    let cross = row_table(&graph, "MATCH (a:Person), (b:Robot) RETURN a, b");
    assert_eq!(cross.row_count(), 16, "cross product keeps every pair");
    let production = production_table(&graph, "MATCH (a:Person), (b:Robot) RETURN a, b");
    assert_tables_equivalent(&cross, &production, "cross product production");
}

#[test]
fn build_side_changes_order_identically_on_both_paths() {
    let graph = person_graph();
    for side in [BuildSide::Left, BuildSide::Right] {
        let mut plan = plan_source("MATCH (a:Person) MATCH (a)-[:KNOWS]->(b) RETURN a, b");
        set_build_side(&mut plan, side);
        for policy in policies().into_iter().chain(randomized_policies()) {
            check_full_agree(&graph, &plan, policy, "build-side order");
        }
    }
}

#[test]
fn leading_optional_match_preserves_unmatched_rows() {
    let graph = person_graph();
    // Robots carry no KNOWS edges: the outer join preserves both robots
    // with null `n` rather than dropping them or leaking bindings.
    let table = production_table(
        &graph,
        "MATCH (m:Robot) OPTIONAL MATCH (m)-[e:KNOWS]->(n) RETURN m, e, n",
    );
    assert_eq!(table.row_count(), 2, "both unmatched robots survive");
    for row in table.rows() {
        assert!(
            matches!(row.values(), [_, Value::Null, Value::Null]),
            "unmatched outer rows stay null, got {:?}",
            row.values()
        );
    }
    check_join_matrix(
        &graph,
        "MATCH (m:Robot) OPTIONAL MATCH (m)-[e:KNOWS]->(n) RETURN m, e, n",
    );
    // Persons mix matched and preserved rows through the same operator.
    check_join_matrix(
        &graph,
        "MATCH (a:Person) OPTIONAL MATCH (a)-[e:KNOWS]->(b) RETURN a, e, b",
    );
    let mixed = production_table(
        &graph,
        "MATCH (a:Person) OPTIONAL MATCH (a)-[e:KNOWS]->(b) RETURN a, e, b",
    );
    assert_eq!(mixed.row_count(), 8, "two matched plus six preserved");
    // Leading OPTIONAL MATCH over an empty key space null-extends.
    check_join_matrix(&graph, "OPTIONAL MATCH (a:Nope) RETURN a");
    let empty = production_table(&graph, "OPTIONAL MATCH (a:Nope) RETURN a");
    assert_eq!(empty.row_count(), 1, "one null-extended row");
    assert!(
        matches!(empty.rows()[0].values(), [Value::Null]),
        "no-match row carries nulls, got {:?}",
        empty.rows()[0].values()
    );
}

#[test]
fn non_leading_optional_match_never_leaks_across_rows() {
    let graph = person_graph();
    // Alice matches (one edge), Cara never does: rows after a matched row
    // must still bind nulls, across every batch shape including single-row
    // pulls where each input row evaluates alone.
    let source = "MATCH (a:Person) FILTER a.age > 20 OPTIONAL MATCH (a)-[e:KNOWS]->(b) RETURN a.name AS name, b";
    check_join_matrix(&graph, source);
    let table = production_table(&graph, source);
    let names = table
        .rows()
        .iter()
        .map(|row| format!("{:?}", row.values()))
        .collect::<Vec<_>>();
    assert_eq!(table.row_count(), 8, "matched plus preserved rows");
    assert!(
        names.iter().any(|row| row.contains("Null")),
        "unmatched rows bind null, got {names:?}"
    );
}

#[test]
fn many_to_many_fanout_matches_row_counts_at_facade() {
    // One shared node with two incoming R edges and three incoming S edges:
    // the comma join over the shared binding yields six rows end to end.
    let graph = SharedGraph::new(GraphId::new(43_100));
    let hub = db_string("Hub").unwrap();
    let spoke = db_string("Spoke").unwrap();
    let left_edge = db_string("R").unwrap();
    let right_edge = db_string("S").unwrap();
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let center = mutator
            .create_node(LabelSet::single(hub), PropertyMap::default())
            .expect("hub inserts");
        for _ in 0..2 {
            let source = mutator
                .create_node(LabelSet::single(spoke.clone()), PropertyMap::default())
                .expect("left spoke inserts");
            mutator
                .create_edge(left_edge.clone(), source, center, PropertyMap::default())
                .expect("left edge inserts");
        }
        for _ in 0..3 {
            let source = mutator
                .create_node(LabelSet::single(spoke.clone()), PropertyMap::default())
                .expect("right spoke inserts");
            mutator
                .create_edge(right_edge.clone(), source, center, PropertyMap::default())
                .expect("right edge inserts");
        }
        txn.commit().expect("fixture commits");
    }
    let source = "MATCH (x)-[:R]->(n), (y)-[:S]->(n) RETURN x, y, n";
    check_join_matrix(&graph, source);
    let table = production_table(&graph, source);
    assert_eq!(table.row_count(), 6, "two-by-three facade fanout keeps six");
}

#[test]
fn joins_paths_and_disjunction_execute_completely() {
    let graph = person_graph();
    for source in [
        "MATCH (a:Person) MATCH (a)-[:KNOWS]->(b) RETURN a, b",
        "MATCH (a:Person), (b:Robot) RETURN a, b",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a, b",
        "MATCH (a:Person) FILTER a.age > 20 MATCH (a)-[:KNOWS]->(b) RETURN a, b",
    ] {
        let plan = plan_source(source);
        batch_prefix_with_policy(&graph, &plan, BatchPolicy::default_policy())
            .expect("complete physical join");
    }
    // F05-PR04 routes variable-length paths through the same batch tree.
    let plan = plan_source("MATCH (a)-[:KNOWS*1..2]->(b) RETURN a, b");
    batch_prefix_with_policy(&graph, &plan, BatchPolicy::default_policy()).expect("physical path");
    // Repeated disjunctive branches must deduplicate their common anchor,
    // rather than falling back or multiplying downstream aggregates.
    let mut disjunctive = plan_source("MATCH (n) RETURN n");
    let pattern = disjunctive.pattern_plan.as_mut().expect("pattern");
    let crate::JoinTree::Scan(scan) = pattern.join_tree.clone() else {
        panic!("expected a single scan to rewrite");
    };
    pattern.join_tree = crate::JoinTree::DisjunctiveScan {
        branches: vec![scan.clone(), scan.clone()],
        scan_anchor: scan,
    };
    let disjunctive = batch_prefix_with_policy(&graph, &disjunctive, BatchPolicy::default_policy())
        .expect("physical disjunction");
    assert_eq!(disjunctive.row_count(), 10);
    for source in [
        "MATCH (n) RETURN n",
        "MATCH (a:Person) MATCH (a)-[:KNOWS]->(b) RETURN a, b",
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a, b",
    ] {
        let plan = plan_source(source);
        batch_prefix_with_policy(&graph, &plan, BatchPolicy::default_policy())
            .expect("physical pattern");
    }
}

#[allow(clippy::print_stdout)]
#[test]
fn join_perf_probe_reports_observed_numbers() {
    // Observed numbers only: no timing assertions. Shapes cover selective
    // joins, skewed keys, many-to-many fanout, and sparse outer matches,
    // with peak build/probe memory from bounded budget counters and
    // tiny-input latency for the nested-loop path choice.
    selective_probe();
    skewed_probe();
    fanout_probe();
    sparse_outer_probe();
    tiny_latency_probe();
}

#[allow(clippy::print_stdout)]
fn selective_probe() {
    let graph = person_graph();
    let source = "MATCH (a:Person) MATCH (a)-[:KNOWS]->(b) RETURN a, b";
    let _ = production_table(&graph, source);
    let _ = row_table(&graph, source);
    let started = std::time::Instant::now();
    let batch = production_table(&graph, source);
    let batch_us = started.elapsed().as_micros();
    let started = std::time::Instant::now();
    let rowed = row_table(&graph, source);
    let row_us = started.elapsed().as_micros();
    assert_eq!(batch.row_count(), rowed.row_count());
    println!(
        "batch-join probe: shape=selective rows={} batch_us={batch_us} row_us={row_us}",
        batch.row_count()
    );
}

#[allow(clippy::print_stdout)]
fn skewed_probe() {
    let graph = hub_graph();
    // Fourteen edges share one hub key: extreme skew through the hash path.
    let source = "MATCH (h:Hub) MATCH (h)-[e:KNOWS]->(s) RETURN h, e, s";
    let _ = production_table(&graph, source);
    let _ = row_table(&graph, source);
    let started = std::time::Instant::now();
    let batch = production_table(&graph, source);
    let batch_us = started.elapsed().as_micros();
    let started = std::time::Instant::now();
    let rowed = row_table(&graph, source);
    let row_us = started.elapsed().as_micros();
    assert_eq!(batch.row_count(), rowed.row_count());
    println!(
        "batch-join probe: shape=skewed rows={} batch_us={batch_us} row_us={row_us}",
        batch.row_count()
    );
}

#[allow(clippy::print_stdout)]
fn fanout_probe() {
    // Synthetic 200x200 fanout at the kernel level with peak memory from a
    // bounded budget context (unlimited production still accounts peaks).
    let schema = pair_schema();
    let build = (0..200)
        .map(|index| pair(Value::Int(9), Value::Int(index)))
        .collect::<Vec<_>>();
    let probe = (0..200)
        .map(|index| pair(Value::Int(9), Value::Int(index)))
        .collect::<Vec<_>>();
    let mut ctx = kernel_ctx(MemoryBudget::unlimited());
    let started = std::time::Instant::now();
    let (rows, reserved) =
        hash_join_rows(&build, &probe, &[0], true, &schema, &mut ctx).expect("fanout joins");
    let hash_us = started.elapsed().as_micros();
    let peak = ctx.budget_peak();
    ctx.budget_mut().release(reserved);
    assert_eq!(rows.len(), 40_000);
    let mut loop_ctx = kernel_ctx(MemoryBudget::unlimited());
    let started = std::time::Instant::now();
    let (loop_rows, loop_reserved) =
        nested_loop_join_rows(&build, &probe, &[0], true, &schema, &mut loop_ctx)
            .expect("fanout joins");
    let loop_us = started.elapsed().as_micros();
    loop_ctx.budget_mut().release(loop_reserved);
    assert_eq!(loop_rows.len(), 40_000);
    println!(
        "batch-join probe: shape=fanout-200x200 rows=40000 hash_us={hash_us} loop_us={loop_us} peak_bytes={peak}"
    );
}

#[allow(clippy::print_stdout)]
fn sparse_outer_probe() {
    // One thousand preserved rows with three matches: the outer path pays
    // per-row right evaluation with almost no output amplification.
    let graph = SharedGraph::new(GraphId::new(43_200));
    seed_nodes(&graph, 1_000, Some("Crowd"));
    let source = "MATCH (c:Crowd) OPTIONAL MATCH (c)-[:KNOWS]->(n) RETURN c, n";
    let _ = production_table(&graph, source);
    let _ = row_table(&graph, source);
    let started = std::time::Instant::now();
    let batch = production_table(&graph, source);
    let batch_us = started.elapsed().as_micros();
    let started = std::time::Instant::now();
    let rowed = row_table(&graph, source);
    let row_us = started.elapsed().as_micros();
    assert_eq!(batch.row_count(), 1_000);
    assert_eq!(rowed.row_count(), 1_000);
    println!("batch-join probe: shape=sparse-outer rows=1000 batch_us={batch_us} row_us={row_us}");
}

#[allow(clippy::print_stdout)]
fn tiny_latency_probe() {
    // Tiny inputs stay on the nested-loop path: report both paths at 2x3 so
    // the low-overhead choice is observed rather than assumed.
    let schema = pair_schema();
    let build = vec![
        pair(Value::Int(1), Value::Int(1)),
        pair(Value::Int(1), Value::Int(2)),
    ];
    let probe = vec![
        pair(Value::Int(1), Value::Int(3)),
        pair(Value::Int(1), Value::Int(4)),
        pair(Value::Int(1), Value::Int(5)),
    ];
    let mut hash_ctx = kernel_ctx(MemoryBudget::unlimited());
    let started = std::time::Instant::now();
    for _ in 0..1_000 {
        let (rows, reserved) =
            hash_join_rows(&build, &probe, &[0], true, &schema, &mut hash_ctx).expect("tiny joins");
        hash_ctx.budget_mut().release(reserved);
        assert_eq!(rows.len(), 6);
    }
    let hash_us = started.elapsed().as_micros();
    let mut loop_ctx = kernel_ctx(MemoryBudget::unlimited());
    let started = std::time::Instant::now();
    for _ in 0..1_000 {
        let (rows, reserved) =
            nested_loop_join_rows(&build, &probe, &[0], true, &schema, &mut loop_ctx)
                .expect("tiny joins");
        loop_ctx.budget_mut().release(reserved);
        assert_eq!(rows.len(), 6);
    }
    let loop_us = started.elapsed().as_micros();
    println!("batch-join probe: shape=tiny-2x3-x1000 hash_us={hash_us} loop_us={loop_us}");
}
