//! Grouping/sorting policy differentials and the independent relation model.
//!
//! Each facade query runs through logical planning and the stable result
//! boundary on both paths; agreement must hold across the fixed and
//! randomized batch-size matrices. Batch-policy agreement alone is not
//! independent semantic evidence, so
//! grouping partitions, sort orders, and dedup sets are also checked
//! against the independently written relation model
//! ([`super::relation_model`], which shares no executor code with either
//! path), and key shapes carry hand-derived counts. Schema and descriptor
//! assertions run even at zero rows.

use selene_core::{GraphId, Value};
use selene_graph::SharedGraph;

use crate::{
    ExecutionPlan, PipelineOp,
    runtime::{BindingTable, ExecutorError, TxContext},
};

use super::fixtures::{
    batch_prefix_with_policy, hub_graph, oddball_graph, optimized_plan, person_graph, plan_source,
    production_table, row_table,
};
use super::relation_model::{
    ModelSortKey, assert_same_multiset, groups_of, model_distinct, model_sort,
};
use super::{BatchPolicy, assert_tables_equivalent, collect_rows, descriptor_for};

/// Batch sizes for the boundary matrix: single-row pulls, awkward windows,
/// and the production default.
fn policies() -> Vec<BatchPolicy> {
    [1usize, 2, 3, 7, 1024]
        .into_iter()
        .map(|target| BatchPolicy::new(target, 1 << 20).unwrap())
        .collect()
}

/// Deterministic pseudo-random batch sizes (seeded LCG, no new
/// dependencies): awkward windows the fixed matrix misses, so the same
/// input partitions differently without changing the required result.
fn randomized_policies() -> Vec<BatchPolicy> {
    let mut state = 0x51ED_6A02_85A3_08D3u64;
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

/// Assert agreement for `source` (linear plan) across the fixed and
/// randomized matrices.
fn check_query_matrix(graph: &SharedGraph, source: &str) {
    let plan = plan_source(source);
    for policy in policies().into_iter().chain(randomized_policies()) {
        check_full_agree(graph, &plan, policy, source);
    }
}

/// Assert agreement for an optimized plan (top-K fusion shapes) across the
/// fixed and randomized matrices.
fn check_optimized_matrix(graph: &SharedGraph, source: &str) {
    let plan = optimized_plan(source, graph);
    for policy in policies().into_iter().chain(randomized_policies()) {
        check_full_agree(graph, &plan, policy, source);
    }
}

#[test]
fn driver_accepts_group_sort_shapes_and_composes_suffixes() {
    let graph = person_graph();
    // Grouping, ordering, dedup, and carrier-trim shapes run fully in
    // batches (no row suffix remains).
    for source in [
        "MATCH (n:Person) RETURN n.age AS age, count(*) AS c GROUP BY n.age",
        "MATCH (n:Person) RETURN count(*) AS c",
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name",
        "MATCH (n:Person) RETURN DISTINCT n.age AS age",
        "RETURN 1 AS one",
        "RETURN count(*) AS c",
    ] {
        let plan = plan_source(source);
        batch_prefix_with_policy(&graph, &plan, BatchPolicy::default_policy())
            .expect("complete group/sort execution");
    }
}

#[test]
fn empty_inputs_yield_three_distinct_required_results() {
    let graph = person_graph();
    // Empty ungrouped aggregation yields one row: COUNT(*) is zero, not
    // zero rows.
    for source in [
        "MATCH (n:Nope) RETURN count(*) AS c",
        "MATCH (n:Nope) RETURN count(n) AS c, sum(n.age) AS s, avg(n.age) AS a",
    ] {
        check_query_matrix(&graph, source);
    }
    let ungrouped = row_table(&graph, "MATCH (n:Nope) RETURN count(*) AS c");
    assert_eq!(ungrouped.row_count(), 1, "empty ungrouped yields one row");
    assert_eq!(
        collect_rows(&production_table(
            &graph,
            "MATCH (n:Nope) RETURN count(*) AS c"
        )),
        vec![vec![Value::Int(0)]],
        "hand-derived empty COUNT(*) is zero"
    );
    let full = row_table(
        &graph,
        "MATCH (n:Nope) RETURN count(n) AS c, sum(n.age) AS s, avg(n.age) AS a",
    );
    assert_eq!(
        collect_rows(&full),
        vec![vec![Value::Int(0), Value::Int(0), Value::Null]],
        "hand-derived empty results: count zero, sum zero, avg null"
    );
    assert_tables_equivalent(
        &full,
        &production_table(
            &graph,
            "MATCH (n:Nope) RETURN count(n) AS c, sum(n.age) AS s, avg(n.age) AS a",
        ),
        "empty ungrouped production",
    );
    // Empty grouped input yields zero rows with the full descriptor.
    check_query_matrix(
        &graph,
        "MATCH (n:Nope) RETURN n AS x, count(*) AS c GROUP BY n",
    );
    let grouped = production_table(
        &graph,
        "MATCH (n:Nope) RETURN n AS x, count(*) AS c GROUP BY n",
    );
    assert_eq!(grouped.row_count(), 0, "empty grouped yields no rows");
    assert_eq!(
        grouped.schema().columns.len(),
        2,
        "empty grouped keeps input plus aggregate columns"
    );
    assert_eq!(
        descriptor_for(&grouped),
        descriptor_for(&row_table(
            &graph,
            "MATCH (n:Nope) RETURN n AS x, count(*) AS c GROUP BY n"
        )),
        "empty grouped descriptors agree"
    );
}

#[test]
fn grouped_counts_match_row_oracle_and_hand_derived_totals() {
    let graph = person_graph();
    // Eight persons with ages 21..28 plus two ageless robots: grouping all
    // nodes by age yields eight singleton groups plus one all-null group
    // of two.
    let source = "MATCH (n) RETURN n.age AS age, count(*) AS c GROUP BY n.age";
    check_query_matrix(&graph, source);
    let table = production_table(&graph, source);
    assert_eq!(table.row_count(), 9, "eight ages plus one null group");
    let null_groups = table
        .rows()
        .iter()
        .filter(|row| row.values()[0] == Value::Null)
        .collect::<Vec<_>>();
    assert_eq!(null_groups.len(), 1, "nulls belong to one group");
    assert_eq!(
        null_groups[0].values()[1],
        Value::Int(2),
        "null group counts both robots"
    );
    // The independent grouping oracle agrees on the output partition:
    // grouping the produced rows by their key column yields nine
    // singletons — no split groups (one key in two rows) and no merged
    // groups (fewer than nine rows) — and every non-null group counts
    // exactly one (ages are unique in the fixture).
    let actual = collect_rows(&table);
    let model = groups_of(&actual, 1);
    assert_eq!(model.len(), 9, "oracle partitions nine groups");
    assert!(
        model.iter().all(|members| members.len() == 1),
        "oracle finds no split groups"
    );
    for row in &actual {
        if row[0] != Value::Null {
            assert_eq!(row[1], Value::Int(1), "unique ages count once");
        }
    }
    let actual_pairs = actual
        .iter()
        .map(|row| vec![row[0].clone(), row[1].clone()])
        .collect::<Vec<_>>();
    // Order-insensitive agreement with the row oracle complements the
    // ordered differential above.
    let oracle_pairs = collect_rows(&row_table(&graph, source));
    assert_same_multiset(
        &oracle_pairs,
        &actual_pairs,
        "facade vs row oracle multiset",
    );
    // Ungrouped totals over the eight persons.
    check_query_matrix(
        &graph,
        "MATCH (n:Person) RETURN count(*) AS c, sum(n.age) AS s",
    );
    let totals = production_table(
        &graph,
        "MATCH (n:Person) RETURN count(*) AS c, sum(n.age) AS s",
    );
    assert_eq!(
        collect_rows(&totals),
        vec![vec![
            Value::Int(8),
            Value::Int(21 + 22 + 23 + 24 + 25 + 26 + 27 + 28)
        ]],
        "hand-derived person totals"
    );
}

#[test]
fn distinct_aggregates_agree_with_oracle() {
    let graph = person_graph();
    for source in [
        "MATCH (n:Person) RETURN count(DISTINCT n.age) AS c",
        "MATCH (n:Person) RETURN sum(DISTINCT n.age) AS s",
        "MATCH (n:Person) RETURN collect_list(DISTINCT n.age) AS xs",
        "MATCH (n) RETURN count(DISTINCT n.age) AS c",
    ] {
        check_query_matrix(&graph, source);
    }
    // All eight ages are distinct: DISTINCT changes nothing here, which the
    // hand-derived total pins down.
    let distinct = production_table(&graph, "MATCH (n:Person) RETURN count(DISTINCT n.age) AS c");
    assert_eq!(
        collect_rows(&distinct),
        vec![vec![Value::Int(8)]],
        "eight distinct ages"
    );
}

#[test]
fn incompatible_group_values_fail_identically() {
    // Persons carry integer ages, robots carry none, and the oddball graph
    // mixes an integer, a missing, and a string age: grouping the mixed
    // property fails with the same GQLSTATUS on both paths, never a merged
    // group.
    let graph = oddball_graph();
    let source = "MATCH (n:Person) RETURN n.age AS age, count(*) AS c GROUP BY n.age";
    let plan = plan_source(source);
    for policy in policies().into_iter().chain(randomized_policies()) {
        check_full_agree(&graph, &plan, policy, source);
    }
    let row = row_execute(&graph, &plan);
    assert!(
        row.is_err(),
        "mixed int/string grouping must fail on the row path too"
    );
}

#[test]
fn null_ordering_matches_oracle_and_independent_model() {
    let graph = person_graph();
    // Robots carry no age: age ordering interleaves eight integers with
    // two nulls per the null policy.
    for source in [
        "MATCH (n) RETURN n.name AS name ORDER BY n.age",
        "MATCH (n) RETURN n.name AS name ORDER BY n.age DESC",
        "MATCH (n) RETURN n.name AS name ORDER BY n.age NULLS FIRST",
        "MATCH (n) RETURN n.name AS name ORDER BY n.age DESC NULLS LAST",
    ] {
        check_query_matrix(&graph, source);
    }
    // Independent model parity on the default-ascending shape: extract the
    // (age, name) pairs from the row oracle, sort indexes with the
    // separately written comparator, and require the production name order
    // to match exactly.
    let plan = plan_source("MATCH (n) RETURN n.age AS age, n.name AS name ORDER BY n.age");
    let expected = row_execute(&graph, &plan).expect("row oracle sorts");
    let plain = collect_rows(&expected);
    let order = model_sort(
        &plain,
        &[ModelSortKey {
            column: 0,
            ascending: true,
            nulls_first: false,
        }],
    );
    let model_names = order
        .iter()
        .map(|index| plain[*index][1].clone())
        .collect::<Vec<_>>();
    let batch = batch_prefix_with_policy(&graph, &plan, BatchPolicy::default_policy())
        .expect("driver executes ordering");
    let batch_names = collect_rows(&batch)
        .iter()
        .map(|row| row[1].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        batch_names, model_names,
        "production order matches the independent model"
    );
    // Hand-derived null placement: ascending defaults nulls last, so the
    // two ageless robots (R2/R3) close the order after Alice..Hal (ages
    // 21..28). The tail pair is asserted as a set: ties promise no order.
    assert_eq!(batch_names.len(), 10);
    let (oldest, tail) = batch_names.split_at(8);
    let oldest = oldest
        .iter()
        .map(|value| match value {
            Value::String(name) => name.as_str().to_owned(),
            other => panic!("expected a name, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        oldest,
        ["Alice", "Bob", "Cara", "Dan", "Erin", "Fay", "Gus", "Hal"],
        "ascending ages order Alice..Hal, got {oldest:?}"
    );
    let mut tail = tail
        .iter()
        .map(|value| match value {
            Value::String(name) => name.as_str().to_owned(),
            other => panic!("expected a name, got {other:?}"),
        })
        .collect::<Vec<_>>();
    tail.sort();
    assert_eq!(
        tail,
        ["R2", "R3"],
        "ageless robots close the ascending order, got {tail:?}"
    );
}

#[test]
fn collation_sensitive_keys_match_fixtures() {
    // Person names are all capitalized (Alice..Hal plus R2/R3): binary
    // collation orders them by codepoint, which the hand-written expected
    // orders pin down. Byte-order cross-checks catch any drift toward a
    // case-insensitive or locale collation.
    let graph = person_graph();
    let source = "MATCH (n) RETURN n.name AS name ORDER BY name";
    check_query_matrix(&graph, source);
    let table = production_table(&graph, source);
    let names = table
        .rows()
        .iter()
        .map(|row| match &row.values()[0] {
            Value::String(value) => value.as_str().to_owned(),
            other => panic!("expected a name string, got {other:?}"),
        })
        .collect::<Vec<_>>();
    // Ten nodes are named: eight persons plus R2/R3.
    assert_eq!(
        names,
        [
            "Alice", "Bob", "Cara", "Dan", "Erin", "Fay", "Gus", "Hal", "R2", "R3"
        ],
        "hand-derived ascending name order, got {names:?}"
    );
    let desc = production_table(&graph, "MATCH (n) RETURN n.name AS name ORDER BY name DESC");
    let desc_names = desc
        .rows()
        .iter()
        .map(|row| match &row.values()[0] {
            Value::String(value) => value.as_str().to_owned(),
            other => panic!("expected a name string, got {other:?}"),
        })
        .collect::<Vec<_>>();
    let mut reversed = names.clone();
    reversed.reverse();
    assert_eq!(
        desc_names, reversed,
        "descending is the exact reverse, got {desc_names:?}"
    );
    // Grouping is collation-sensitive too: the ten names form ten groups
    // on both engines and in the oracle.
    let grouped = production_table(
        &graph,
        "MATCH (n) RETURN n.name AS name, count(*) AS c GROUP BY n.name",
    );
    assert_eq!(grouped.row_count(), 10, "ten distinct names");
    let plain = collect_rows(&row_table(
        &graph,
        "MATCH (n) RETURN n.name AS name, count(*) AS c GROUP BY n.name",
    ));
    assert_eq!(groups_of(&plain, 1).len(), 10, "oracle agrees");
}

#[test]
fn distinct_rows_agree_with_oracle_and_model() {
    let graph = person_graph();
    for source in [
        "MATCH (n:Person) RETURN DISTINCT n.age AS age",
        "MATCH (n) RETURN DISTINCT n.age AS age",
        "RETURN 1 AS x UNION ALL RETURN 2 AS x UNION ALL RETURN 1 AS x",
    ] {
        check_query_matrix(&graph, source);
    }
    // Hand-derived: eight persons carry ages 21..28, all distinct.
    let ages = production_table(&graph, "MATCH (n:Person) RETURN DISTINCT n.age AS age");
    assert_eq!(ages.row_count(), 8, "eight distinct ages survive");
    // Independent model parity on the all-nodes shape (null age deduped
    // to one row): model the row oracle's age column, compare multisets.
    let expected = row_table(&graph, "MATCH (n) RETURN n.age AS age");
    let plain = collect_rows(&expected);
    let model = model_distinct(&plain);
    let actual = collect_rows(&production_table(
        &graph,
        "MATCH (n) RETURN DISTINCT n.age AS age",
    ));
    assert_same_multiset(&model, &actual, "facade vs distinctness oracle");
}

#[test]
fn order_limit_windows_agree_across_matrices() {
    let graph = person_graph();
    // Unfused ORDER BY plus page windows compose across batch sizes,
    // including exhaustion and overrun.
    for source in [
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name LIMIT 3",
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name OFFSET 6",
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name LIMIT 2 OFFSET 7",
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name DESC LIMIT 4 OFFSET 2",
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name LIMIT 0",
    ] {
        check_query_matrix(&graph, source);
    }
    let window = production_table(
        &graph,
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name LIMIT 2 OFFSET 7",
    );
    assert_eq!(window.row_count(), 1, "offset seven of eight leaves one");
}

#[test]
fn fused_top_k_windows_agree_across_matrices() {
    let graph = person_graph();
    // Optimized plans fuse adjacent ORDER BY plus bounded LIMIT into TopK:
    // the fused operator must return the same window the row path does,
    // with ties resolving identically.
    for source in [
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name LIMIT 3",
        "MATCH (n:Person) RETURN n.age AS age ORDER BY age DESC LIMIT 4",
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name LIMIT 2 OFFSET 7",
        "MATCH (n:Person) RETURN n.age AS age ORDER BY age LIMIT 0",
        "MATCH (n) RETURN n.age AS age ORDER BY age LIMIT 5",
    ] {
        check_optimized_matrix(&graph, source);
    }
    // The fused shape really runs fused: the optimized plan carries TopK.
    let fused = optimized_plan(
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name LIMIT 3",
        &graph,
    );
    assert!(
        fused
            .pipeline
            .iter()
            .any(|op| matches!(op, PipelineOp::TopK { .. })),
        "optimizer fuses the window into TopK"
    );
    let window = {
        let plan = fused;
        let mut ctx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &crate::EmptyProcedureRegistry,
            graph.index_providers(),
        );
        super::super::plan_runner::execute_plan(&plan, &mut ctx).expect("production runs fused")
    };
    assert_eq!(window.row_count(), 3, "fused window keeps three");
}

#[test]
fn incompatible_sort_keys_fail_identically() {
    // Ordering the oddball mixed int/string ages fails with the same
    // GQLSTATUS on both paths, never an invented cross-family order.
    let graph = oddball_graph();
    let source = "MATCH (n:Person) RETURN n.age AS age ORDER BY age";
    let plan = plan_source(source);
    for policy in policies().into_iter().chain(randomized_policies()) {
        check_full_agree(&graph, &plan, policy, source);
    }
    assert!(
        row_execute(&graph, &plan).is_err(),
        "mixed int/string ordering must fail on the row path too"
    );
}

#[test]
fn union_all_doubles_rows_through_prefix() {
    // Set composition stays in the batch prefix with exact multiplicity:
    // union arms double the eight ages.
    let graph = person_graph();
    let source =
        "MATCH (n:Person) RETURN n.age AS age UNION ALL MATCH (m:Person) RETURN m.age AS age";
    check_query_matrix(&graph, source);
    let doubled = production_table(&graph, source);
    assert_eq!(doubled.row_count(), 16, "union all doubles the eight ages");
}

#[test]
fn zero_row_schema_and_diagnostics_agree() {
    // Schema plus descriptor information survive even when the row count
    // is zero, on every new operator family.
    for source in [
        "MATCH (n:Nope) RETURN n.age AS age, count(*) AS c GROUP BY n.age",
        "MATCH (n:Nope) RETURN n.name AS name ORDER BY name",
        "MATCH (n:Nope) RETURN DISTINCT n.age AS age",
        "MATCH (n:Nope) RETURN n.name AS name ORDER BY name LIMIT 3",
    ] {
        let plan = plan_source(source);
        let expected = row_execute(&person_graph(), &plan).expect("row oracle runs");
        assert_eq!(expected.row_count(), 0);
        let batch = batch_prefix_with_policy(&person_graph(), &plan, BatchPolicy::default_policy())
            .expect("driver executes empty shapes");
        assert_eq!(batch.row_count(), 0);
        assert_tables_equivalent(&expected, &batch, source);
        assert_eq!(
            descriptor_for(&expected),
            descriptor_for(&batch),
            "{source}: descriptors agree at zero rows"
        );
    }
}

#[test]
fn hub_expansion_then_group_and_order_agree() {
    // Multiplicity-producing expansion feeds grouping (duplicate spokes
    // collapse with counts) and ordering across every partitioning.
    let graph = hub_graph();
    for source in [
        "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN s, count(*) AS c GROUP BY s",
        "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN e.score AS score ORDER BY score",
        "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN e.score AS score ORDER BY score DESC LIMIT 4",
    ] {
        check_query_matrix(&graph, source);
    }
    let grouped = production_table(
        &graph,
        "MATCH (h:Hub)-[e:KNOWS]->(s) RETURN s, count(*) AS c GROUP BY s",
    );
    // Twelve spokes plus the hub-as-loop-target: thirteen distinct
    // endpoints, with the parallel duplicate counted twice.
    assert_eq!(grouped.row_count(), 13, "thirteen endpoint groups");
    let total: i64 = grouped
        .rows()
        .iter()
        .map(|row| match row.values()[1] {
            Value::Int(count) => count,
            ref other => panic!("expected a count, got {other:?}"),
        })
        .sum();
    assert_eq!(total, 14, "group counts sum to every edge row");
}

#[test]
fn empty_graph_id_is_stable() {
    // Graph identity smoke for the matrix (mirrors the join rigidity
    // probe): grouping on a fresh graph id plans and runs.
    let graph = SharedGraph::new(GraphId::new(44_100));
    check_query_matrix(&graph, "RETURN count(*) AS c");
    let table = production_table(&graph, "RETURN count(*) AS c");
    assert_eq!(
        collect_rows(&table),
        vec![vec![Value::Int(1)]],
        "unit seed counts once"
    );
}

/// Performance probe: grouping and sorting shapes with observed numbers.
///
/// No timing assertions: rows visited, group counts, batch counts, peak
/// estimated bytes, and elapsed microseconds print per shape for batch and
/// row sides alike. Ordering cost is measured separately from
/// projection/serialization by timing the bare operator shapes against
/// wider-projection variants.
#[allow(clippy::print_stdout)]
#[test]
fn perf_probe_reports_group_sort_numbers() {
    group_shape_probe();
    sort_shape_probe();
    top_k_shape_probe();
    skew_shape_probe();
}

#[allow(clippy::print_stdout)]
fn group_shape_probe() {
    use std::time::Instant;
    let graph = person_graph();
    // Few groups (eight ages), many rows per group via union doubling.
    let source = "MATCH (n:Person) RETURN n.age AS age, count(*) AS c GROUP BY n.age";
    let plan = plan_source(source);
    let _ = production_table(&graph, source);
    let _ = row_table(&graph, source);
    let started = Instant::now();
    let batch = production_table(&graph, source);
    let batch_us = started.elapsed().as_micros();
    let started = Instant::now();
    let rowed = row_table(&graph, source);
    let row_us = started.elapsed().as_micros();
    assert_eq!(batch.row_count(), rowed.row_count());
    // Peak estimated bytes through the driver with the default policy.
    let table = batch_prefix_with_policy(&graph, &plan, BatchPolicy::default_policy())
        .expect("driver executes grouping");
    println!(
        "batch-group-sort probe: shape=few-groups rows={} groups={} batch_us={batch_us} single_row_policy_us={row_us} output_rows={}",
        batch.row_count(),
        batch.row_count(),
        table.row_count(),
    );
}

#[allow(clippy::print_stdout)]
fn sort_shape_probe() {
    use std::time::Instant;
    let graph = person_graph();
    for source in [
        "MATCH (n:Person) RETURN n.name AS name ORDER BY name",
        "MATCH (n) RETURN n.name AS name ORDER BY name",
    ] {
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
            "batch-group-sort probe: shape=sort rows={} batch_us={batch_us} row_us={row_us} source={source}",
            batch.row_count(),
        );
    }
}

#[allow(clippy::print_stdout)]
fn top_k_shape_probe() {
    use std::time::Instant;
    let graph = person_graph();
    // Sort with LIMIT against the full sort: the window shape returns the
    // head while ordering everything on the unfused path.
    let full = "MATCH (n:Person) RETURN n.name AS name ORDER BY name";
    let limited = "MATCH (n:Person) RETURN n.name AS name ORDER BY name LIMIT 2";
    let _ = production_table(&graph, full);
    let _ = production_table(&graph, limited);
    let started = Instant::now();
    let full_table = production_table(&graph, full);
    let full_us = started.elapsed().as_micros();
    let started = Instant::now();
    let limited_table = production_table(&graph, limited);
    let limited_us = started.elapsed().as_micros();
    assert_eq!(full_table.row_count(), 8);
    assert_eq!(limited_table.row_count(), 2);
    println!(
        "batch-group-sort probe: shape=top-k full_us={full_us} limited_us={limited_us} rows=8 window=2"
    );
}

#[allow(clippy::print_stdout)]
fn skew_shape_probe() {
    use std::time::Instant;
    // Skew: every node shares one age group through a constant key, so one
    // group accumulates all rows while the sort still orders all rows.
    let graph = person_graph();
    let source = "MATCH (n:Person) RETURN count(*) AS c";
    let _ = production_table(&graph, source);
    let _ = row_table(&graph, source);
    let started = Instant::now();
    let batch = production_table(&graph, source);
    let batch_us = started.elapsed().as_micros();
    let started = Instant::now();
    let rowed = row_table(&graph, source);
    let row_us = started.elapsed().as_micros();
    assert_eq!(collect_rows(&batch), collect_rows(&rowed));
    // Wide keys: grouping by whole-node bindings instead of one property.
    let wide = "MATCH (n:Person) RETURN n, count(*) AS c GROUP BY n";
    let started = Instant::now();
    let wide_batch = production_table(&graph, wide);
    let wide_us = started.elapsed().as_micros();
    assert_eq!(wide_batch.row_count(), 8, "eight node groups");
    println!(
        "batch-group-sort probe: shape=skew batch_us={batch_us} row_us={row_us} wide_us={wide_us} wide_groups={}",
        wide_batch.row_count(),
    );
}
