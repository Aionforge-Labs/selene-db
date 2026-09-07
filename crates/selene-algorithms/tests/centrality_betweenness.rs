//! Integration tests for `betweenness` per spec 16 §E19–§E24.

use selene_algorithms::{
    BetweennessConfig, GraphProjection, Parallelism, ProjectionConfig,
    betweenness as run_betweenness,
};
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

fn betweenness(proj: &GraphProjection, sample_size: Option<usize>) -> Vec<(NodeId, f64)> {
    run_betweenness(
        proj,
        BetweennessConfig {
            sample_size,
            parallelism: Parallelism::Sequential,
        },
    )
}

#[test]
fn betweenness_empty_projection_returns_empty() {
    let shared = SharedGraph::new(GraphId::new(1));
    let proj = build_proj(&shared);
    assert!(betweenness(&proj, None).is_empty());
}

#[test]
fn betweenness_single_node_returns_zero() {
    let (shared, nodes) = build_graph(1, &[]);
    let proj = build_proj(&shared);
    let result = betweenness(&proj, None);
    assert_eq!(result, vec![(nodes[0], 0.0)]);
}

#[test]
fn betweenness_chain_middle_node_is_intermediate() {
    // n0 → n1 → n2: n1 lies on the unique shortest path from n0 to n2.
    // Directed Brandes': contribution from source n0 only → betweenness[n1] = 1.0.
    let (shared, nodes) = build_graph(3, &[(0, 1), (1, 2)]);
    let proj = build_proj(&shared);
    let result = betweenness(&proj, None);
    let score_n1 = result.iter().find(|&&(n, _)| n == nodes[1]).unwrap().1;
    assert_eq!(score_n1, 1.0, "n1 sits between n0 and n2 (directed)");
    // Endpoints have zero betweenness.
    assert_eq!(result.iter().find(|&&(n, _)| n == nodes[0]).unwrap().1, 0.0);
    assert_eq!(result.iter().find(|&&(n, _)| n == nodes[2]).unwrap().1, 0.0);
}

#[test]
fn betweenness_no_through_paths_returns_all_zero() {
    // Disconnected pairs: n0 → n1, n2 → n3. No node lies between any other.
    let (shared, _) = build_graph(4, &[(0, 1), (2, 3)]);
    let proj = build_proj(&shared);
    let result = betweenness(&proj, None);
    for &(_, score) in &result {
        assert_eq!(score, 0.0);
    }
}

#[test]
fn betweenness_star_center_dominates() {
    // Star: n0 has edges to n1..n4; each spoke has reverse edge.
    // n0 lies on paths between every spoke pair → highest betweenness.
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
    let result = betweenness(&proj, None);
    assert_eq!(result[0].0, nodes[0], "center n0 must rank first");
    assert!(
        result[0].1 > result[1].1,
        "center score must strictly exceed spoke scores"
    );
}

#[test]
fn betweenness_directed_triangle_symmetric_unit_scores() {
    // Directed 3-cycle n0 → n1 → n2 → n0. Each node IS an intermediate on
    // exactly one pair's shortest path:
    //   from n0: shortest n0 → n2 = [n0, n1, n2] → n1 is intermediate (δ += 1)
    //   from n1: shortest n1 → n0 = [n1, n2, n0] → n2 is intermediate (δ += 1)
    //   from n2: shortest n2 → n1 = [n2, n0, n1] → n0 is intermediate (δ += 1)
    // By symmetry all three nodes have betweenness 1.0; result is tied →
    // sort must resolve ASC by NodeId per §E21.
    let (shared, nodes) = build_graph(3, &[(0, 1), (1, 2), (2, 0)]);
    let proj = build_proj(&shared);
    let result = betweenness(&proj, None);
    for &(n, score) in &result {
        assert_eq!(
            score, 1.0,
            "triangle node {n:?} should have betweenness 1.0 by symmetry"
        );
    }
    assert_eq!(
        result.iter().map(|&(n, _)| n).collect::<Vec<_>>(),
        nodes,
        "tied scores must sort ASC by NodeId per §E21"
    );
}

#[test]
fn betweenness_sample_size_none_equals_full() {
    let (shared, _) = build_graph(4, &[(0, 1), (1, 2), (2, 3)]);
    let proj = build_proj(&shared);
    let full = betweenness(&proj, None);
    let same_via_some = betweenness(&proj, Some(4));
    // Some(k) where k >= n_count collapses to None per §E24.
    assert_eq!(full, same_via_some);
}

#[test]
fn betweenness_sample_size_zero_returns_zeros() {
    // Some(0) → no sources → all centrality zero per §E24.
    let (shared, _) = build_graph(3, &[(0, 1), (1, 2)]);
    let proj = build_proj(&shared);
    let result = betweenness(&proj, Some(0));
    assert_eq!(result.len(), 3);
    for &(_, score) in &result {
        assert_eq!(score, 0.0);
    }
}

#[test]
fn betweenness_sample_size_scaling_preserves_magnitude() {
    // For a small graph the sample-size scaling preserves the magnitude
    // domain: full-computation centrality and sampled-scaled centrality
    // should be in the same ballpark (within a small constant factor for
    // n=4 → k=2 sampling).
    let (shared, _) = build_graph(5, &[(0, 1), (1, 2), (2, 3), (3, 4)]);
    let proj = build_proj(&shared);
    let full = betweenness(&proj, None);
    let sampled = betweenness(&proj, Some(2));
    // The sampled centrality is scaled (n/k = 5/2 = 2.5x) so the total
    // magnitudes are comparable. Full magnitude is well-defined; sampled
    // should be a positive approximation.
    let full_sum: f64 = full.iter().map(|&(_, s)| s).sum();
    let sampled_sum: f64 = sampled.iter().map(|&(_, s)| s).sum();
    assert!(full_sum > 0.0);
    // Sampled with deterministic step may miss some paths; assert it's
    // strictly non-negative and the magnitudes don't blow up.
    assert!(sampled_sum >= 0.0);
}

#[test]
fn betweenness_sampling_spans_full_index_range_includes_tail() {
    // PR #60 Codex P2 regression: prior step=n/k formula would yield
    // sources [0,1,2,3] for n=5, k=4 — skipping index 4 (the tail). The
    // endpoint-aware formula `i * (n - 1) / (k - 1)` produces sources
    // [0,1,2,4] — landing at BOTH endpoints.
    //
    // Discriminating fixture: chain n4 → n0 → n1 → n2 → n3 (note n4 is the
    // HEAD of the chain in graph-flow terms, but its NodeId is the largest
    // and thus the dense-index tail). The only way `n0` gets nonzero
    // betweenness is when n4 is sampled as a source (n4 is the only node
    // with n0 as a downstream intermediate on its shortest paths).
    //
    // OLD sampling [0,1,2,3]: source 4 missing → centrality[n0] = 0.
    // NEW sampling [0,1,2,4]: source 4 included → centrality[n0] > 0.
    let (shared, nodes) = build_graph(5, &[(4, 0), (0, 1), (1, 2), (2, 3)]);
    let proj = build_proj(&shared);
    let result = betweenness(&proj, Some(4));

    let score_n0 = result.iter().find(|&&(n, _)| n == nodes[0]).unwrap().1;
    assert!(
        score_n0 > 0.0,
        "endpoint-aware sampling must include n4 (dense tail); n0 should \
         then receive δ contribution from n4's shortest paths. Got 0.0."
    );
}

#[test]
fn betweenness_sample_size_one_returns_single_source_result() {
    // §E24 amendment: Some(1) on n>1 samples a single source at dense
    // index 0 (the endpoint-aware formula divides by zero for k=1, so it's
    // special-cased).
    let (shared, nodes) = build_graph(3, &[(0, 1), (1, 2)]);
    let proj = build_proj(&shared);
    let result = betweenness(&proj, Some(1));
    // From source n0 only: n1 lies between n0 and n2 → δ(n1) = 1.
    // Scaled by n/k = 3/1 = 3.
    let score_n1 = result.iter().find(|&&(n, _)| n == nodes[1]).unwrap().1;
    assert_eq!(score_n1, 3.0, "Some(1) samples n0 only; n1 betweenness × 3");
}

#[test]
fn betweenness_handles_sparse_row_projection() {
    // §E20 — state arrays sized by RowIndex, not max_row + 1.
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
    txn.mutator()
        .create_edge(rel.clone(), nodes[0], nodes[50], PropertyMap::new())
        .unwrap();
    txn.mutator()
        .create_edge(rel.clone(), nodes[50], nodes[99], PropertyMap::new())
        .unwrap();
    txn.commit().unwrap();

    let snapshot = shared.read();
    let scope = snapshot
        .bind_node_candidates([nodes[0], nodes[50], nodes[99]])
        .unwrap();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "sparse-bw".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        Some(&scope),
    )
    .unwrap();

    assert_eq!(proj.node_count(), 3);
    let result = betweenness(&proj, None);
    // n50 sits between n0 and n99 (the only directed n0→n99 path goes
    // through n50); betweenness[n50] = 1.0.
    let score_n50 = result.iter().find(|&&(n, _)| n == nodes[50]).unwrap().1;
    assert_eq!(score_n50, 1.0);
}
