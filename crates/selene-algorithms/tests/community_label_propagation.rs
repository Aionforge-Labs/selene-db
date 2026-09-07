//! Integration tests for `label_propagation` per spec 16 §E25–§E30.

use selene_algorithms::{GraphProjection, ProjectionConfig, label_propagation};
use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap};
use selene_graph::SharedGraph;

fn db_string(name: &str) -> DbString {
    selene_core::db_string(name).unwrap()
}

fn build_proj(shared: &SharedGraph) -> GraphProjection {
    let snapshot = shared.read();
    GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "test".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        None,
    )
    .unwrap()
}

fn build_graph(count: usize, edges: &[(usize, usize)]) -> (SharedGraph, Vec<NodeId>) {
    let shared = SharedGraph::new(GraphId::new(1));
    let label = db_string("N");
    let rel = db_string("R");
    let mut txn = shared.begin_write();
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        let id = txn
            .mutator()
            .create_node(LabelSet::single(label.clone()), PropertyMap::new())
            .unwrap();
        nodes.push(id);
    }
    for &(s, t) in edges {
        txn.mutator()
            .create_edge(rel.clone(), nodes[s], nodes[t], PropertyMap::new())
            .unwrap();
    }
    txn.commit().unwrap();
    (shared, nodes)
}

#[test]
fn label_propagation_empty_projection_returns_empty() {
    let shared = SharedGraph::new(GraphId::new(1));
    let proj = build_proj(&shared);
    assert!(label_propagation(&proj, 100).is_empty());
}

#[test]
fn label_propagation_single_node_keeps_initial_label() {
    let (shared, nodes) = build_graph(1, &[]);
    let proj = build_proj(&shared);
    let result = label_propagation(&proj, 100);
    assert_eq!(result, vec![(nodes[0], nodes[0].get())]);
}

#[test]
fn label_propagation_two_node_both_converge_to_same_label() {
    // n0 ↔ n1 (bidirectional). The donor / our implementation is
    // **asynchronous-deterministic**: nodes update in ASC NodeId order and
    // each update is visible to subsequent updates in the same iteration.
    //
    // Iteration 1:
    //   - n0 visits neighbors (out:n1, in:n1) → both labels = n1.get() = 2 →
    //     n0 adopts 2.
    //   - n1 visits neighbors (out:n0, in:n0) → both labels = n0's NEW label
    //     = 2 → n1 already at 2, no change.
    // Result: both nodes carry label 2 (the larger NodeId's initial label,
    // because update order matters under async LP).
    let (shared, nodes) = build_graph(2, &[(0, 1), (1, 0)]);
    let proj = build_proj(&shared);
    let result = label_propagation(&proj, 50);
    let label_n0 = result.iter().find(|&&(n, _)| n == nodes[0]).unwrap().1;
    let label_n1 = result.iter().find(|&&(n, _)| n == nodes[1]).unwrap().1;
    assert_eq!(label_n0, label_n1, "two-node graph must agree on label");
    assert_eq!(
        label_n0,
        nodes[1].get(),
        "async update order: n0 adopts n1's label first, then n1 stays"
    );
}

#[test]
fn label_propagation_star_center_adopts_smallest_spoke_label() {
    // Center n0 connected (bidirectionally) to spokes n1..n4. After one
    // iteration: n0's neighbor labels are {n1, n2, n3, n4}, each with count
    // 2 (each bidir edge contributes once via out and once via in). All tied
    // → smallest label wins → n0 adopts n1.get(). Spokes have only one
    // neighbor (n0), so each adopts n0.get(); next iteration spokes see
    // labels from n0 + each other (= n1.get()) and adopt.
    let (shared, nodes) = build_graph(
        5,
        &[
            (0, 1),
            (0, 2),
            (0, 3),
            (0, 4),
            (1, 0),
            (2, 0),
            (3, 0),
            (4, 0),
        ],
    );
    let proj = build_proj(&shared);
    let result = label_propagation(&proj, 100);
    // The community ID should be the smallest NodeId in the star (n1, since
    // n0's neighbors are n1..n4 with bidir edges, n0 sees tied label counts
    // and picks min = n1.get()).
    let labels: Vec<u64> = result.iter().map(|&(_, l)| l).collect();
    let unique: std::collections::HashSet<u64> = labels.iter().copied().collect();
    assert_eq!(
        unique.len(),
        1,
        "star graph should converge to a single community, got {labels:?}"
    );
    assert_eq!(
        unique.into_iter().next().unwrap(),
        nodes[1].get(),
        "smallest-label tie-break picks n1.get()"
    );
}

#[test]
fn label_propagation_two_disjoint_cliques_self_label() {
    // Two K3 triangles, disconnected: {n0,n1,n2} and {n3,n4,n5}.
    let edges = vec![
        // Triangle 1: bidirectional
        (0, 1),
        (1, 0),
        (1, 2),
        (2, 1),
        (0, 2),
        (2, 0),
        // Triangle 2: bidirectional
        (3, 4),
        (4, 3),
        (4, 5),
        (5, 4),
        (3, 5),
        (5, 3),
    ];
    let (shared, nodes) = build_graph(6, &edges);
    let proj = build_proj(&shared);
    let result = label_propagation(&proj, 100);
    let label_n0 = result.iter().find(|&&(n, _)| n == nodes[0]).unwrap().1;
    let label_n1 = result.iter().find(|&&(n, _)| n == nodes[1]).unwrap().1;
    let label_n2 = result.iter().find(|&&(n, _)| n == nodes[2]).unwrap().1;
    let label_n3 = result.iter().find(|&&(n, _)| n == nodes[3]).unwrap().1;
    let label_n4 = result.iter().find(|&&(n, _)| n == nodes[4]).unwrap().1;
    let label_n5 = result.iter().find(|&&(n, _)| n == nodes[5]).unwrap().1;
    // Within each clique, all three nodes must converge to the same label.
    assert_eq!(label_n0, label_n1, "triangle 1 unified");
    assert_eq!(label_n1, label_n2, "triangle 1 unified");
    assert_eq!(label_n3, label_n4, "triangle 2 unified");
    assert_eq!(label_n4, label_n5, "triangle 2 unified");
    // The two cliques have disjoint NodeId ranges → labels MUST differ.
    assert_ne!(label_n0, label_n3, "disjoint cliques have distinct labels");
}

#[test]
fn label_propagation_max_iter_zero_returns_initial_state() {
    let (shared, nodes) = build_graph(3, &[(0, 1), (1, 2), (2, 0)]);
    let proj = build_proj(&shared);
    let result = label_propagation(&proj, 0);
    // Each node retains its initial label = own NodeId.
    for &(nid, label) in &result {
        assert_eq!(label, nid.get(), "max_iter=0 → initial labels survive");
    }
    let labels: std::collections::HashSet<u64> = result.iter().map(|&(_, l)| l).collect();
    assert_eq!(
        labels.len(),
        3,
        "3 distinct labels (one per node) when no iteration runs"
    );
    // Sanity: result includes every node.
    assert_eq!(result.len(), 3);
    let nids: Vec<NodeId> = result.iter().map(|&(n, _)| n).collect();
    assert_eq!(nids, nodes);
}

#[test]
fn label_propagation_disconnected_nodes_each_own_community() {
    let (shared, nodes) = build_graph(4, &[]);
    let proj = build_proj(&shared);
    let result = label_propagation(&proj, 100);
    let labels: std::collections::HashSet<u64> = result.iter().map(|&(_, l)| l).collect();
    assert_eq!(labels.len(), 4, "4 disconnected nodes = 4 communities");
    // Each node's label is its own NodeId (Raghavan: empty neighbors → skip).
    for (i, &(nid, label)) in result.iter().enumerate() {
        assert_eq!(nid, nodes[i]);
        assert_eq!(label, nid.get(), "isolated node retains own label");
    }
}

#[test]
fn label_propagation_result_sorted_asc_by_node_id() {
    let (shared, _nodes) = build_graph(5, &[(0, 1), (1, 2), (2, 3), (3, 4)]);
    let proj = build_proj(&shared);
    let result = label_propagation(&proj, 100);
    let nids: Vec<u64> = result.iter().map(|&(n, _)| n.get()).collect();
    let mut sorted = nids.clone();
    sorted.sort();
    assert_eq!(nids, sorted, "§E27: result sorted ASC by NodeId");
}

#[test]
fn label_propagation_handles_sparse_row_projection() {
    // §E26: state arrays sized by RowIndex, not max_row + 1.
    let shared = SharedGraph::new(GraphId::new(1));
    let label = db_string("N");
    let rel = db_string("R");
    let mut txn = shared.begin_write();
    let mut nodes = Vec::with_capacity(100);
    for _ in 0..100 {
        nodes.push(
            txn.mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .unwrap(),
        );
    }
    // Bidirectional edge between n[10] and n[90] — only these two will be in
    // the scope. Without §E26 RowIndex sizing, state arrays would be 90+
    // wide.
    txn.mutator()
        .create_edge(rel.clone(), nodes[10], nodes[90], PropertyMap::new())
        .unwrap();
    txn.mutator()
        .create_edge(rel, nodes[90], nodes[10], PropertyMap::new())
        .unwrap();
    txn.commit().unwrap();

    let snapshot = shared.read();
    let scope = snapshot
        .bind_node_candidates([nodes[10], nodes[90]])
        .unwrap();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "sparse-lp".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        Some(&scope),
    )
    .unwrap();
    assert_eq!(proj.node_count(), 2);
    let result = label_propagation(&proj, 50);
    assert_eq!(result.len(), 2);
    // After enough iterations on a bidirectional 2-node graph, both end with
    // the SAME label (oscillation possible but eventually they agree at some
    // iteration). Strictly: after even iterations they swap to original; we
    // assert containment in the initial-label set, NOT equality.
    let labels: Vec<u64> = result.iter().map(|&(_, l)| l).collect();
    let initial = [nodes[10].get(), nodes[90].get()];
    for &l in &labels {
        assert!(
            initial.contains(&l),
            "label {l} should be one of {initial:?}"
        );
    }
}
