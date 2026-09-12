//! Executor tests for unbounded and questioned quantifiers.

mod exec_common;

use exec_common::{
    ExecFixture, db_string, edge_ids_for, execute_plan, node_ids_for, planned, props,
};
use selene_core::{DbString, GraphId, LabelSet, Value};
use selene_gql::{
    Binding, BindingTable, BindingTableSchema, EmptyProcedureRegistry, ExecutorError, TxContext,
    execute_pattern, execute_pipeline,
};
use selene_graph::SharedGraph;

fn edge_lists_for(table: &BindingTable, name: &str) -> Vec<Option<Vec<u64>>> {
    exec_common::column_values(table, name)
        .into_iter()
        .map(|value| match value {
            Value::List(items) => Some(
                items
                    .into_iter()
                    .map(|item| match item {
                        Value::EdgeRef(id) => id.get(),
                        other => panic!("expected edge ref in group list, got {other:?}"),
                    })
                    .collect(),
            ),
            Value::Null => None,
            other => panic!("expected edge list or null, got {other:?}"),
        })
        .collect()
}

fn execute_on_graph(
    graph: &SharedGraph,
    plan: &selene_gql::ExecutionPlan,
) -> Result<BindingTable, ExecutorError> {
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    )
    .with_plan_metadata(&plan.expr_ids, &plan.subqueries);
    let input = if let Some(pattern) = &plan.pattern_plan {
        execute_pattern(pattern, &ctx)?
    } else {
        BindingTable::new(
            BindingTableSchema {
                columns: Vec::new(),
            },
            vec![Binding::empty()],
        )
    };
    execute_pipeline(&plan.pipeline, input, &mut ctx)
}

fn cycle_graph() -> SharedGraph {
    let node = db_string("N");
    let edge = db_string("K");
    let name = db_string("name");
    let graph = SharedGraph::new(GraphId::new(6401));
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let a = mutator
            .create_node(
                LabelSet::single(node.clone()),
                props([(name.clone(), Value::String(db_string("A")))]),
            )
            .expect("A inserts");
        let b = mutator
            .create_node(
                LabelSet::single(node),
                props([(name, Value::String(db_string("B")))]),
            )
            .expect("B inserts");
        mutator
            .create_edge(edge.clone(), a, b, props([]))
            .expect("edge 1");
        mutator.create_edge(edge, b, a, props([])).expect("edge 2");
        txn.commit().expect("fixture commits");
    }
    graph
}

fn chain_graph() -> SharedGraph {
    let node = db_string("N");
    let edge = db_string("K");
    let name = db_string("name");
    let graph = SharedGraph::new(GraphId::new(6402));
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let a = named_node(&mut mutator, node.clone(), name.clone(), "A");
        let b = named_node(&mut mutator, node.clone(), name.clone(), "B");
        let c = named_node(&mut mutator, node, name, "C");
        mutator
            .create_edge(edge.clone(), a, b, props([]))
            .expect("edge 1");
        mutator.create_edge(edge, b, c, props([])).expect("edge 2");
        txn.commit().expect("fixture commits");
    }
    graph
}

fn named_node(
    mutator: &mut selene_graph::Mutator<'_, '_>,
    label: DbString,
    name_key: DbString,
    name: &str,
) -> selene_core::NodeId {
    mutator
        .create_node(
            LabelSet::single(label),
            props([(name_key, Value::String(db_string(name)))]),
        )
        .expect("node inserts")
}

#[test]
fn questioned_edge_emits_skipped_and_taken_rows() {
    let fixture = ExecFixture::build();
    let plan = planned("MATCH (a:Person {name: 'Alice'})-[r:KNOWS?]->(b) RETURN r, b");

    let table = execute_plan(&fixture, &plan).expect("questioned edge executes");

    assert_eq!(edge_ids_for(&table, "r"), vec![None, Some(1)]);
    assert_eq!(node_ids_for(&table, "b"), vec![Some(1), Some(2)]);
}

#[test]
fn questioned_edge_null_propagates_properties() {
    let fixture = ExecFixture::build();
    let plan = planned("MATCH (a:Person {name: 'Alice'})-[r:KNOWS?]->(b) RETURN r.score AS score");

    let table = execute_plan(&fixture, &plan).expect("questioned edge executes");

    assert_eq!(
        exec_common::column_values(&table, "score"),
        vec![Value::Null, Value::Int(1)]
    );
}

#[test]
fn questioned_edge_zero_hop_composes_with_selectors_and_path_modes() {
    let fixture = ExecFixture::build();
    let shortest = planned(
        "MATCH ANY SHORTEST (a:Person {name: 'Alice'})-[r:KNOWS?]->(b:Person {name: 'Alice'}) RETURN r, b",
    );
    let acyclic = planned(
        "MATCH ACYCLIC (a:Person {name: 'Alice'})-[r:KNOWS?]->(b:Person {name: 'Alice'}) RETURN r, b",
    );

    let shortest_rows = execute_plan(&fixture, &shortest).expect("shortest executes");
    let acyclic_rows = execute_plan(&fixture, &acyclic).expect("acyclic executes");

    assert_eq!(edge_ids_for(&shortest_rows, "r"), vec![None]);
    assert_eq!(node_ids_for(&shortest_rows, "b"), vec![Some(1)]);
    assert_eq!(edge_ids_for(&acyclic_rows, "r"), vec![None]);
    assert_eq!(node_ids_for(&acyclic_rows, "b"), vec![Some(1)]);
}

#[test]
fn unbounded_trail_prunes_repeated_edges_in_loop() {
    let graph = cycle_graph();
    let plan = planned("MATCH TRAIL (a:N {name: 'A'})-[r:K+]->(b:N) RETURN r, b");

    let table = execute_on_graph(&graph, &plan).expect("unbounded trail executes");

    assert_eq!(
        edge_lists_for(&table, "r"),
        vec![Some(vec![1]), Some(vec![1, 2])]
    );
    assert_eq!(node_ids_for(&table, "b"), vec![Some(2), Some(1)]);
}

#[test]
fn unbounded_simple_allows_terminal_return_to_source() {
    let graph = cycle_graph();
    let plan = planned("MATCH SIMPLE (a:N {name: 'A'})-[r:K+]->(b:N {name: 'A'}) RETURN r, b");

    let table = execute_on_graph(&graph, &plan).expect("unbounded simple executes");

    assert_eq!(edge_lists_for(&table, "r"), vec![Some(vec![1, 2])]);
    assert_eq!(node_ids_for(&table, "b"), vec![Some(1)]);
}

#[test]
fn unbounded_cap_exceed_returns_program_limit() {
    let graph = chain_graph();
    let mut plan = planned("MATCH ANY (a:N {name: 'A'})-[:K+]->(b:N) RETURN b");
    plan.impl_defined_caps.max_quantifier = 1;

    let err = execute_on_graph(&graph, &plan).expect_err("cap exceeds");

    assert!(matches!(
        err,
        ExecutorError::ProgramLimitExceeded {
            detail: "max_path_hops",
            ..
        }
    ));
    assert_eq!(err.gqlstatus().as_str(), "5GQL1");
}

// Open shortest selectors now enumerate complete hop layers with a conservative
// completion certificate, without changing WALK into TRAIL. On this simple cycle
// the minimum-length results coincide, but lower bounds/counts may reuse edges.

/// `(b, edge-id-list)` rows, sorted, for order-independent comparison.
fn shortest_rows(table: &BindingTable) -> Vec<(Option<u64>, Option<Vec<u64>>)> {
    let bs = node_ids_for(table, "b");
    let rs = edge_lists_for(table, "r");
    let mut rows: Vec<_> = bs.into_iter().zip(rs).collect();
    rows.sort();
    rows
}

#[test]
fn unbounded_all_shortest_terminates_and_equals_trail_on_cycle() {
    let graph = cycle_graph();
    // Pre-fix this raised 5GQL1 (ProgramLimitExceeded "max_quantifier") because the
    // bare ALL SHORTEST selector kept the default WALK and the unbounded WALK
    // expanded to the max_quantifier cap before the selector could run.
    let plan = planned("MATCH ALL SHORTEST (a:N {name: 'A'})-[r:K+]->(b:N) RETURN r, b");

    let table = execute_on_graph(&graph, &plan).expect("unbounded ALL SHORTEST terminates");

    // Identical to the explicit TRAIL spelling: b=B path [1], b=A path [1, 2].
    assert_eq!(
        shortest_rows(&table),
        vec![(Some(1), Some(vec![1, 2])), (Some(2), Some(vec![1]))]
    );

    let trail = planned("MATCH TRAIL (a:N {name: 'A'})-[r:K+]->(b:N) RETURN r, b");
    let trail_table = execute_on_graph(&graph, &trail).expect("trail executes");
    assert_eq!(shortest_rows(&table), shortest_rows(&trail_table));
}

#[test]
fn unbounded_any_shortest_terminates_one_row_per_endpoint_on_cycle() {
    let graph = cycle_graph();
    let plan = planned("MATCH ANY SHORTEST (a:N {name: 'A'})-[r:K+]->(b:N) RETURN b");

    let table = execute_on_graph(&graph, &plan).expect("unbounded ANY SHORTEST terminates");

    // One row per distinct (source, target) endpoint pair: b=B and b=A.
    let mut bs = node_ids_for(&table, "b");
    bs.sort();
    assert_eq!(bs, vec![Some(1), Some(2)]);
}

#[test]
fn unbounded_all_shortest_keeps_equal_length_paths_and_equals_trail() {
    // Diamond with two equal-length shortest paths to D plus a longer one:
    //   A -e1-> B -e3-> D   (len 2, shortest)
    //   A -e2-> C -e4-> D   (len 2, shortest)
    //   A -e1-> B -e5-> E -e6-> D  (len 3, longer — must be pruned by the selector)
    let node = db_string("N");
    let edge = db_string("K");
    let name = db_string("name");
    let graph = SharedGraph::new(GraphId::new(6403));
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let a = named_node(&mut mutator, node.clone(), name.clone(), "A");
        let b = named_node(&mut mutator, node.clone(), name.clone(), "B");
        let c = named_node(&mut mutator, node.clone(), name.clone(), "C");
        let d = named_node(&mut mutator, node.clone(), name.clone(), "D");
        let e = named_node(&mut mutator, node, name, "E");
        mutator
            .create_edge(edge.clone(), a, b, props([]))
            .expect("e1");
        mutator
            .create_edge(edge.clone(), a, c, props([]))
            .expect("e2");
        mutator
            .create_edge(edge.clone(), b, d, props([]))
            .expect("e3");
        mutator
            .create_edge(edge.clone(), c, d, props([]))
            .expect("e4");
        mutator
            .create_edge(edge.clone(), b, e, props([]))
            .expect("e5");
        mutator.create_edge(edge, e, d, props([])).expect("e6");
        txn.commit().expect("fixture commits");
    }

    let plan =
        planned("MATCH ALL SHORTEST (a:N {name: 'A'})-[r:K+]->(d:N {name: 'D'}) RETURN r, d");
    let table = execute_on_graph(&graph, &plan).expect("ALL SHORTEST terminates");

    // BOTH equal-length shortest paths to D are retained (the longer 3-hop one
    // is pruned by the selector): edge-lists [1, 3] and [2, 4].
    let mut edge_lists: Vec<_> = edge_lists_for(&table, "r")
        .into_iter()
        .map(|opt| opt.expect("edge list present"))
        .collect();
    edge_lists.sort();
    assert_eq!(edge_lists, vec![vec![1, 3], vec![2, 4]]);

    // Result-equivalent to the explicit TRAIL-mode spelling of the same query.
    let trail =
        planned("MATCH ALL SHORTEST TRAIL (a:N {name: 'A'})-[r:K+]->(d:N {name: 'D'}) RETURN r, d");
    let trail_table = execute_on_graph(&graph, &trail).expect("trail spelling executes");
    let mut trail_lists: Vec<_> = edge_lists_for(&trail_table, "r")
        .into_iter()
        .map(|opt| opt.expect("edge list present"))
        .collect();
    trail_lists.sort();
    assert_eq!(edge_lists, trail_lists);
}

#[test]
fn bounded_shortest_unchanged_on_cycle() {
    // Regression: a BOUNDED repeat is finite under WALK already; the downshift
    // is unbounded-only and must not alter bounded behavior.
    let graph = cycle_graph();
    let plan = planned("MATCH ALL SHORTEST (a:N {name: 'A'})-[r:K*1..3]->(b:N) RETURN r, b");

    let table = execute_on_graph(&graph, &plan).expect("bounded shortest executes");

    // Shortest hop-rank to each reachable endpoint: b=B [1], b=A [1, 2].
    assert_eq!(
        shortest_rows(&table),
        vec![(Some(1), Some(vec![1, 2])), (Some(2), Some(vec![1]))]
    );
}

#[test]
fn unbounded_counted_shortest_includes_non_simple_walks_on_cycle() {
    let graph = cycle_graph();
    let plan = planned("MATCH SHORTEST 2 (a:N {name: 'A'})-[r:K+]->(b:N) RETURN r");

    let table = execute_on_graph(&graph, &plan).expect("complete selected hop layers");
    let mut paths = edge_lists_for(&table, "r");
    paths.sort();
    assert_eq!(
        paths,
        vec![
            Some(vec![1]),
            Some(vec![1, 2]),
            Some(vec![1, 2, 1]),
            Some(vec![1, 2, 1, 2])
        ]
    );
}

// FU-2 (Codex PR #245 r2, P2): the count-1 counted spellings are ISO §16.6 SR2c
// EQUIVALENT to the keyword forms — `SHORTEST 1 [PATH]` == `ANY SHORTEST`
// (`CountedShortest { paths: 1 }`) and `SHORTEST [1] GROUP[S]` == `ALL SHORTEST`
// (`CountedShortestGroup { groups: 1 }`). They are min-length shortest selectors,
// so on a cyclic graph they terminate identically to their keyword twins.

#[test]
fn unbounded_counted_shortest_one_terminates_and_equals_any_shortest_on_cycle() {
    let graph = cycle_graph();
    let counted = planned("MATCH SHORTEST 1 (a:N {name: 'A'})-[r:K+]->(b:N) RETURN b");

    let table = execute_on_graph(&graph, &counted)
        .expect("unbounded SHORTEST 1 terminates (== ANY SHORTEST)");

    // One row per distinct (source, target) endpoint pair, exactly like ANY SHORTEST.
    let mut bs = node_ids_for(&table, "b");
    bs.sort();
    assert_eq!(bs, vec![Some(1), Some(2)]);

    let any = planned("MATCH ANY SHORTEST (a:N {name: 'A'})-[r:K+]->(b:N) RETURN b");
    let any_table = execute_on_graph(&graph, &any).expect("ANY SHORTEST executes");
    let mut any_bs = node_ids_for(&any_table, "b");
    any_bs.sort();
    assert_eq!(bs, any_bs);
}

#[test]
fn unbounded_shortest_one_group_terminates_and_equals_all_shortest_on_cycle() {
    let graph = cycle_graph();
    let counted = planned("MATCH SHORTEST 1 GROUP (a:N {name: 'A'})-[r:K+]->(b:N) RETURN r, b");

    let table = execute_on_graph(&graph, &counted)
        .expect("unbounded SHORTEST 1 GROUP terminates (== ALL SHORTEST)");

    // Identical to ALL SHORTEST: b=A path [1, 2], b=B path [1].
    assert_eq!(
        shortest_rows(&table),
        vec![(Some(1), Some(vec![1, 2])), (Some(2), Some(vec![1]))]
    );

    let all = planned("MATCH ALL SHORTEST (a:N {name: 'A'})-[r:K+]->(b:N) RETURN r, b");
    let all_table = execute_on_graph(&graph, &all).expect("ALL SHORTEST executes");
    assert_eq!(shortest_rows(&table), shortest_rows(&all_table));
}

#[test]
fn unbounded_shortest_bare_group_defaults_to_one_and_terminates_on_cycle() {
    // `SHORTEST GROUP` (no count) defaults groups -> 1 per ISO §16.6 SR2b, so it is
    // also the ALL SHORTEST min-length selector and must terminate on a cycle.
    let graph = cycle_graph();
    let plan = planned("MATCH SHORTEST GROUP (a:N {name: 'A'})-[r:K+]->(b:N) RETURN r, b");

    let table =
        execute_on_graph(&graph, &plan).expect("unbounded SHORTEST GROUP (groups=1) terminates");

    assert_eq!(
        shortest_rows(&table),
        vec![(Some(1), Some(vec![1, 2])), (Some(2), Some(vec![1]))]
    );
}

#[test]
fn unbounded_counted_shortest_group_two_keeps_non_simple_second_groups() {
    let graph = cycle_graph();
    let plan = planned("MATCH SHORTEST 2 GROUPS (a:N {name: 'A'})-[r:K+]->(b:N) RETURN r");

    let table = execute_on_graph(&graph, &plan).expect("complete selected length groups");
    let mut paths = edge_lists_for(&table, "r");
    paths.sort();
    assert_eq!(
        paths,
        vec![
            Some(vec![1]),
            Some(vec![1, 2]),
            Some(vec![1, 2, 1]),
            Some(vec![1, 2, 1, 2])
        ]
    );
}

#[test]
fn different_edges_makes_counted_shortest_finite_on_cycle() {
    // With DIFFERENT EDGES (G002, ISO
    // §16.4 NOTE 222) the candidate set is constrained to edge-distinct paths (TRAIL),
    // which is finite — so `SHORTEST N DIFFERENT EDGES` over a cycle TERMINATES and
    // counts the N shortest edge-distinct paths. Correct AND complete: node-repeating-
    // but-edge-distinct trails are still counted (DIFFERENT EDGES forbids only edge
    // reuse, not node reuse). The mode is enforced during product traversal.
    let graph = cycle_graph();
    let plan = planned("MATCH SHORTEST 2 DIFFERENT EDGES (a:N {name: 'A'})-[r:K+]->(b:N) RETURN r");

    let table = execute_on_graph(&graph, &plan)
        .expect("DIFFERENT EDGES bounds the candidate set to trails, so it terminates");

    // The 2 shortest edge-distinct paths from A: A->B [1] and A->B->A [1, 2].
    let mut paths = edge_lists_for(&table, "r");
    paths.sort();
    assert_eq!(paths, vec![Some(vec![1]), Some(vec![1, 2])]);
}

#[test]
fn lower_bounded_shortest_over_cycle_preserves_required_edge_reuse() {
    // Codex (PR #245, P2): the WALK->TRAIL downshift is only result-equivalent when
    // the quantifier lower bound is <= 1. With `min >= 2`, removing a cycle would
    // drop below the bound, so the shortest WALK satisfying the bound can legitimately
    // REUSE an edge (e.g. on the A<->B cycle the shortest >=2-hop walk to B is the
    // edge-reusing A->B->A->B [1,2,1]). A TRAIL downshift would drop those rows and
    // return a WRONG (under-)result. Complete hop layers now retain both endpoints.
    let graph = cycle_graph();
    let plan = planned("MATCH ALL SHORTEST (a:N {name: 'A'})-[r:K*2..]->(b:N) RETURN r");

    let table = execute_on_graph(&graph, &plan).expect("lower-bound-aware shortest");
    let mut paths = edge_lists_for(&table, "r");
    paths.sort();
    assert_eq!(paths, vec![Some(vec![1, 2]), Some(vec![1, 2, 1])]);
}
