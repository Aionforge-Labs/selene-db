//! End-to-end GQL tests for the `IM_TRUNCATE` vendor extension (BRIEF-150).
//!
//! Drives `TRUNCATE NODE TYPE :L` / `TRUNCATE EDGE TYPE :L` through the full
//! parse -> analyze -> plan -> execute pipeline and proves:
//! - TRUNCATE NODE TYPE :L is observationally identical to
//!   `MATCH (n:L) DETACH DELETE n` (same graph state, no dangling edges).
//! - Exactly ONE declarative change reaches the WAL/changeset (O(1)).
//! - The GQL Flagger stamps `IM_TRUNCATE` on every truncate statement.

mod exec_common;

use selene_core::{Change, GraphId, LabelSet, NodeId, PropertyMap};
use selene_gql::{
    EmptyProcedureRegistry, ExecutionPlan, ExecutorError, TxContext, analyze, execute_pattern,
    execute_pipeline, feature_walk, parse, plan,
};
use selene_graph::{CommitOutcome, SeleneGraph, SharedGraph};
use selene_profile::FeatureId;

use exec_common::db_string;

fn planned(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

fn run_write(
    graph: &SharedGraph,
    plan: &ExecutionPlan,
) -> Result<(selene_gql::BindingTable, CommitOutcome), ExecutorError> {
    let snapshot = graph.read();
    let mut txn = graph.begin_write();
    let result = {
        let mut ctx = TxContext::write(
            snapshot,
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            &mut txn,
            graph.index_providers(),
        );
        let input = if let Some(pattern) = &plan.pattern_plan {
            execute_pattern(pattern, &ctx)?
        } else {
            selene_gql::BindingTable::new(
                selene_gql::BindingTableSchema { columns: vec![] },
                vec![selene_gql::Binding::empty()],
            )
        };
        execute_pipeline(&plan.pipeline, input, &mut ctx)
    };
    match result {
        Ok(table) => {
            let outcome = txn.commit().expect("write commits");
            Ok((table, outcome))
        }
        Err(error) => {
            txn.rollback();
            Err(error)
        }
    }
}

/// Seed: 3 nodes of :L, 1 :Keep node, edges incident to :L plus a survivor.
fn fixture() -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(4242));
    let mut txn = graph.begin_write();
    {
        let l = db_string("L");
        let keep = db_string("Keep");
        let e = db_string("REL");
        let mut m = txn.mutator();
        let n0 = m
            .create_node(LabelSet::single(l.clone()), PropertyMap::new())
            .unwrap();
        let n1 = m
            .create_node(LabelSet::single(l.clone()), PropertyMap::new())
            .unwrap();
        let n2 = m
            .create_node(LabelSet::single(l), PropertyMap::new())
            .unwrap();
        let k = m
            .create_node(LabelSet::single(keep), PropertyMap::new())
            .unwrap();
        m.create_edge(e.clone(), n0, n1, PropertyMap::new())
            .unwrap();
        m.create_edge(e.clone(), n1, n2, PropertyMap::new())
            .unwrap();
        m.create_edge(e.clone(), n2, k, PropertyMap::new()).unwrap();
        m.create_edge(e, k, k, PropertyMap::new()).unwrap(); // survivor
    }
    txn.commit().expect("fixture commits");
    graph
}

fn assert_same_observable_state(a: &SeleneGraph, b: &SeleneGraph) {
    assert_eq!(a.node_store.alive, b.node_store.alive, "alive nodes differ");
    assert_eq!(a.edge_store.alive, b.edge_store.alive, "alive edges differ");
    assert_eq!(a.idx_label, b.idx_label, "node label index differs");
    assert_eq!(
        a.idx_edge_label, b.idx_edge_label,
        "edge label index differs"
    );
    assert_eq!(
        a.adjacency_out, b.adjacency_out,
        "outgoing adjacency differs"
    );
    assert_eq!(a.adjacency_in, b.adjacency_in, "incoming adjacency differs");
}

#[test]
fn truncate_node_type_equals_detach_delete_end_to_end() {
    let truncated = fixture();
    let detached = fixture();

    let (_, outcome) =
        run_write(&truncated, &planned("TRUNCATE NODE TYPE :L")).expect("truncate executes");
    // O(1): exactly one declarative change regardless of 3 nodes + 3 edges.
    assert_eq!(
        outcome.changes.len(),
        1,
        "truncate writes exactly one change"
    );
    assert!(matches!(
        &outcome.changes[0],
        Change::NodesOfTypeTruncated { label } if *label == db_string("L")
    ));

    run_write(&detached, &planned("MATCH (n:L) DETACH DELETE n")).expect("detach delete executes");

    assert_same_observable_state(&truncated.read(), &detached.read());

    // No :L nodes survive and no dangling edges remain.
    let g = truncated.read();
    assert_eq!(g.node_label_cardinality(&db_string("L")), 0);
    for row in g.edge_store.alive.iter() {
        let row = row as usize;
        let source = *g.edge_store.source.get(row).unwrap();
        let target = *g.edge_store.target.get(row).unwrap();
        assert!(
            g.is_node_alive(source) && g.is_node_alive(target),
            "dangling edge"
        );
    }
    // Survivor node + self-edge remain.
    assert!(g.is_node_alive(NodeId::new(4)));
}

#[test]
fn truncate_edge_type_writes_one_change_end_to_end() {
    let graph = fixture();
    let (_, outcome) =
        run_write(&graph, &planned("TRUNCATE EDGE TYPE :REL")).expect("edge truncate executes");
    assert_eq!(outcome.changes.len(), 1);
    assert!(matches!(
        &outcome.changes[0],
        Change::EdgesOfTypeTruncated { label } if *label == db_string("REL")
    ));
    let g = graph.read();
    assert_eq!(g.edge_count(), 0, "all REL edges removed");
    assert_eq!(g.node_count(), 4, "nodes untouched by edge truncate");
}

#[test]
fn truncate_absent_label_is_clean_noop_end_to_end() {
    let graph = fixture();
    let (_, outcome) =
        run_write(&graph, &planned("TRUNCATE NODE TYPE :Missing")).expect("noop executes");
    assert!(outcome.changes.is_empty(), "absent label writes no change");
    assert_eq!(graph.read().node_count(), 4);
}

#[test]
fn flagger_stamps_im_truncate_for_node_and_edge_truncate() {
    let node_features = feature_walk(&parse("TRUNCATE NODE TYPE :Sensor").expect("parses"))
        .into_iter()
        .filter(|feature| feature.feature_id == FeatureId::IM_TRUNCATE)
        .count();
    assert_eq!(node_features, 1, "node truncate stamps IM_TRUNCATE once");

    let edge_features = feature_walk(&parse("TRUNCATE EDGE TYPE :REL").expect("parses"))
        .into_iter()
        .filter(|feature| feature.feature_id == FeatureId::IM_TRUNCATE)
        .count();
    assert_eq!(edge_features, 1, "edge truncate stamps IM_TRUNCATE once");

    // A non-truncate DDL must not stamp IM_TRUNCATE.
    let other = feature_walk(&parse("DROP NODE TYPE :Sensor").expect("parses"))
        .into_iter()
        .any(|feature| feature.feature_id == FeatureId::IM_TRUNCATE);
    assert!(!other, "DROP NODE TYPE must not stamp IM_TRUNCATE");
}
