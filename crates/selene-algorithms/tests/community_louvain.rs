//! Integration tests for `louvain` per spec 16 §E25–§E30.

use selene_algorithms::{GraphProjection, ProjectionConfig, louvain};
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
fn louvain_empty_projection_returns_empty() {
    let shared = SharedGraph::new(GraphId::new(1));
    let proj = build_proj(&shared);
    assert!(louvain(&proj, 50).is_empty());
}

#[test]
fn louvain_single_node_own_community_level_zero() {
    let (shared, nodes) = build_graph(1, &[]);
    let proj = build_proj(&shared);
    let result = louvain(&proj, 50);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, nodes[0]);
    assert_eq!(result[0].2, 0, "single-pass Louvain emits level=0 (§E27)");
}

#[test]
fn louvain_no_edges_each_node_own_community() {
    let (shared, _) = build_graph(5, &[]);
    let proj = build_proj(&shared);
    let result = louvain(&proj, 50);
    let communities: std::collections::HashSet<u64> = result.iter().map(|&(_, c, _)| c).collect();
    assert_eq!(communities.len(), 5, "no edges → 5 distinct communities");
    for &(_, _, level) in &result {
        assert_eq!(level, 0);
    }
}

#[test]
fn louvain_two_disjoint_cliques_partition_correctly() {
    // Two K3 triangles, disconnected (bidirectional within each clique).
    let edges = vec![
        (0, 1),
        (1, 0),
        (1, 2),
        (2, 1),
        (0, 2),
        (2, 0),
        (3, 4),
        (4, 3),
        (4, 5),
        (5, 4),
        (3, 5),
        (5, 3),
    ];
    let (shared, nodes) = build_graph(6, &edges);
    let proj = build_proj(&shared);
    let result = louvain(&proj, 50);
    let comm_n0 = result.iter().find(|&&(n, _, _)| n == nodes[0]).unwrap().1;
    let comm_n1 = result.iter().find(|&&(n, _, _)| n == nodes[1]).unwrap().1;
    let comm_n2 = result.iter().find(|&&(n, _, _)| n == nodes[2]).unwrap().1;
    let comm_n3 = result.iter().find(|&&(n, _, _)| n == nodes[3]).unwrap().1;
    let comm_n4 = result.iter().find(|&&(n, _, _)| n == nodes[4]).unwrap().1;
    let comm_n5 = result.iter().find(|&&(n, _, _)| n == nodes[5]).unwrap().1;
    // Each clique consolidates into one community.
    assert_eq!(comm_n0, comm_n1, "clique 1 unified");
    assert_eq!(comm_n1, comm_n2, "clique 1 unified");
    assert_eq!(comm_n3, comm_n4, "clique 2 unified");
    assert_eq!(comm_n4, comm_n5, "clique 2 unified");
    // The two cliques are disjoint → distinct communities.
    assert_ne!(comm_n0, comm_n3, "disjoint cliques live in distinct comms");
}

#[test]
fn louvain_max_iter_zero_returns_initial_singletons() {
    let (shared, nodes) = build_graph(3, &[(0, 1), (1, 2), (2, 0)]);
    let proj = build_proj(&shared);
    let result = louvain(&proj, 0);
    let communities: std::collections::HashSet<u64> = result.iter().map(|&(_, c, _)| c).collect();
    assert_eq!(
        communities.len(),
        3,
        "max_iter=0 means no movement; 3 singleton communities"
    );
    // Per §E27 ASC by NodeId.
    let nids: Vec<NodeId> = result.iter().map(|&(n, _, _)| n).collect();
    assert_eq!(nids, nodes);
}

#[test]
fn louvain_result_sorted_asc_by_node_id() {
    let (shared, _) = build_graph(4, &[(0, 1), (1, 2), (2, 3)]);
    let proj = build_proj(&shared);
    let result = louvain(&proj, 50);
    let nids: Vec<u64> = result.iter().map(|&(n, _, _)| n.get()).collect();
    let mut sorted = nids.clone();
    sorted.sort();
    assert_eq!(nids, sorted, "§E27: result sorted ASC by NodeId");
}

#[test]
fn louvain_level_field_always_zero_for_current_single_pass() {
    // §E27: current single-pass Louvain reserves `level=0`; future
    // hierarchical extensions will populate higher levels.
    let (shared, _) = build_graph(4, &[(0, 1), (1, 2), (2, 3), (3, 0)]);
    let proj = build_proj(&shared);
    let result = louvain(&proj, 50);
    for &(_, _, level) in &result {
        assert_eq!(level, 0);
    }
}

#[test]
fn louvain_weighted_projection_biases_partition() {
    // Three nodes in a path n0 - n1 - n2 with weighted edges; if (n0,n1) has
    // a heavy weight and (n1,n2) has a light weight, Louvain should bias n1
    // toward n0's community. We use a `weight` property and a
    // weight_property projection.
    let shared = SharedGraph::new(GraphId::new(1));
    let nlabel = db_string("N");
    let rel = db_string("R");
    let weight_key = db_string("w");
    let mut txn = shared.begin_write();
    let mut nodes = Vec::with_capacity(3);
    for _ in 0..3 {
        nodes.push(
            txn.mutator()
                .create_node(LabelSet::single(nlabel.clone()), PropertyMap::new())
                .unwrap(),
        );
    }
    // Heavy edge n0 <-> n1
    let mut heavy = PropertyMap::new();
    heavy
        .set(weight_key.clone(), selene_core::Value::Int(100))
        .unwrap();
    txn.mutator()
        .create_edge(rel.clone(), nodes[0], nodes[1], heavy.clone())
        .unwrap();
    txn.mutator()
        .create_edge(rel.clone(), nodes[1], nodes[0], heavy)
        .unwrap();
    // Light edge n1 <-> n2
    let mut light = PropertyMap::new();
    light
        .set(weight_key.clone(), selene_core::Value::Int(1))
        .unwrap();
    txn.mutator()
        .create_edge(rel.clone(), nodes[1], nodes[2], light.clone())
        .unwrap();
    txn.mutator()
        .create_edge(rel, nodes[2], nodes[1], light)
        .unwrap();
    txn.commit().unwrap();
    let snapshot = shared.read();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "weighted".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: Some(weight_key),
        },
        None,
    )
    .unwrap();
    let result = louvain(&proj, 50);
    let comm_n0 = result.iter().find(|&&(n, _, _)| n == nodes[0]).unwrap().1;
    let comm_n1 = result.iter().find(|&&(n, _, _)| n == nodes[1]).unwrap().1;
    // The heavy edge should pull n0 and n1 into the same community.
    assert_eq!(
        comm_n0, comm_n1,
        "heavy-weighted (n0,n1) edge unifies their community"
    );
}

#[test]
fn louvain_weighted_degree_invariant_and_heavy_edge_assignment() {
    let shared = SharedGraph::new(GraphId::new(1));
    let nlabel = db_string("N");
    let rel = db_string("R");
    let weight_key = db_string("w");
    let mut txn = shared.begin_write();
    let mut nodes = Vec::with_capacity(3);
    for _ in 0..3 {
        nodes.push(
            txn.mutator()
                .create_node(LabelSet::single(nlabel.clone()), PropertyMap::new())
                .unwrap(),
        );
    }

    let mut light = PropertyMap::new();
    light
        .set(weight_key.clone(), selene_core::Value::Int(1))
        .unwrap();
    txn.mutator()
        .create_edge(rel.clone(), nodes[0], nodes[1], light.clone())
        .unwrap();
    txn.mutator()
        .create_edge(rel.clone(), nodes[1], nodes[0], light)
        .unwrap();

    let mut heavy = PropertyMap::new();
    heavy
        .set(weight_key.clone(), selene_core::Value::Int(100))
        .unwrap();
    txn.mutator()
        .create_edge(rel.clone(), nodes[1], nodes[2], heavy.clone())
        .unwrap();
    txn.mutator()
        .create_edge(rel, nodes[2], nodes[1], heavy)
        .unwrap();
    txn.commit().unwrap();

    let snapshot = shared.read();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "weighted-tie-break".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: Some(weight_key),
        },
        None,
    )
    .unwrap();
    let total_weight: f64 = proj
        .iter_nodes()
        .flat_map(|node| proj.out_neighbors(node))
        .map(|neighbor| neighbor.weight)
        .sum();
    let weighted_degree_sum: f64 = proj
        .iter_nodes()
        .map(|node| {
            proj.out_neighbors(node)
                .iter()
                .chain(proj.in_neighbors(node))
                .map(|neighbor| neighbor.weight)
                .sum::<f64>()
        })
        .sum();
    assert!(
        (weighted_degree_sum - (2.0 * total_weight)).abs() <= 1.0e-12,
        "weighted degree invariant must be Σd_i = 2m"
    );

    let result = louvain(&proj, 50);
    let comm_n1 = result.iter().find(|&&(n, _, _)| n == nodes[1]).unwrap().1;
    let comm_n2 = result.iter().find(|&&(n, _, _)| n == nodes[2]).unwrap().1;

    assert_eq!(
        comm_n1, comm_n2,
        "heavy high-NodeId edge should pull n1 toward n2"
    );
}

#[test]
fn louvain_handles_sparse_row_projection() {
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
    // Bidirectional edges within scope {n[10], n[50], n[90]} forming a
    // triangle.
    for &(s, t) in &[(10, 50), (50, 10), (50, 90), (90, 50), (10, 90), (90, 10)] {
        txn.mutator()
            .create_edge(rel.clone(), nodes[s], nodes[t], PropertyMap::new())
            .unwrap();
    }
    txn.commit().unwrap();

    let snapshot = shared.read();
    let scope = snapshot
        .bind_node_candidates([nodes[10], nodes[50], nodes[90]])
        .unwrap();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "sparse-louvain".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        Some(&scope),
    )
    .unwrap();
    assert_eq!(proj.node_count(), 3);
    let result = louvain(&proj, 50);
    assert_eq!(result.len(), 3);
    // The K3 in scope should converge to a single community.
    let communities: std::collections::HashSet<u64> = result.iter().map(|&(_, c, _)| c).collect();
    assert_eq!(
        communities.len(),
        1,
        "K3 triangle should unify into one community"
    );
}
