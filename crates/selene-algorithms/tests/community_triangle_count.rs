//! Integration tests for `triangle_count` per spec 16 §E25, §E27, §E29.

use std::num::NonZeroUsize;

use selene_algorithms::{
    GraphProjection, Parallelism, ProjectionConfig, TriangleCountConfig, triangle_count,
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

fn triangle_config(parallelism: Parallelism) -> TriangleCountConfig {
    TriangleCountConfig { parallelism }
}

fn sequential_triangle_config() -> TriangleCountConfig {
    triangle_config(Parallelism::Sequential)
}

fn threads4() -> Parallelism {
    Parallelism::Threads(NonZeroUsize::new(4).expect("non-zero thread count"))
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
fn triangle_count_empty_projection_returns_empty() {
    let shared = SharedGraph::new(GraphId::new(1));
    let proj = build_proj(&shared);
    assert!(triangle_count(&proj, sequential_triangle_config()).is_empty());
}

#[test]
fn triangle_count_single_node_returns_zero() {
    let (shared, nodes) = build_graph(1, &[]);
    let proj = build_proj(&shared);
    let result = triangle_count(&proj, sequential_triangle_config());
    assert_eq!(result, vec![(nodes[0], 0)]);
}

#[test]
fn triangle_count_directed_triangle_each_vertex_gets_one() {
    // n0 → n1 → n2 → n0. Under the undirected view (out ∪ in), each pair is
    // connected → forms a triangle. Each vertex participates in 1 triangle.
    let (shared, _) = build_graph(3, &[(0, 1), (1, 2), (2, 0)]);
    let proj = build_proj(&shared);
    let result = triangle_count(&proj, sequential_triangle_config());
    assert_eq!(result.len(), 3);
    for &(_, count) in &result {
        assert_eq!(count, 1, "K3 vertex participates in exactly 1 triangle");
    }
    // Σ counts == 3 × num_triangles ⇒ num_triangles = 1.
    let total: usize = result.iter().map(|&(_, c)| c).sum();
    assert_eq!(total / 3, 1);
}

#[test]
fn triangle_count_k4_each_vertex_in_three_triangles() {
    // K4: every pair connected. C(4,3) = 4 triangles, each vertex
    // participates in 3 (one for each other-three-subset including it).
    let edges = vec![(0, 1), (1, 2), (2, 0), (0, 3), (1, 3), (2, 3)];
    let (shared, _) = build_graph(4, &edges);
    let proj = build_proj(&shared);
    let result = triangle_count(&proj, sequential_triangle_config());
    for &(_, count) in &result {
        assert_eq!(count, 3, "K4 vertex participates in 3 triangles");
    }
    let total: usize = result.iter().map(|&(_, c)| c).sum();
    assert_eq!(total, 12, "Σ = 3 · num_triangles = 3·4 = 12");
}

#[test]
fn triangle_count_chain_returns_zero() {
    // Chain: n0 → n1 → n2 (no triangle).
    let (shared, _) = build_graph(3, &[(0, 1), (1, 2)]);
    let proj = build_proj(&shared);
    let result = triangle_count(&proj, sequential_triangle_config());
    for &(_, count) in &result {
        assert_eq!(count, 0, "chain has no triangles");
    }
}

#[test]
fn triangle_count_star_no_triangles() {
    // Star: n0 connected (bidirectionally) to n1..n4; no triangles since
    // spokes are not connected to each other.
    let (shared, _) = build_graph(
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
    let result = triangle_count(&proj, sequential_triangle_config());
    for &(_, count) in &result {
        assert_eq!(count, 0, "star has no triangles");
    }
}

#[test]
fn triangle_count_self_loop_not_counted() {
    // Self-loop n0 → n0; plus a triangle n0 → n1 → n2 → n0. The self-loop
    // must NOT contribute (per §E29).
    let (shared, nodes) = build_graph(3, &[(0, 0), (0, 1), (1, 2), (2, 0)]);
    let proj = build_proj(&shared);
    let result = triangle_count(&proj, sequential_triangle_config());
    let count_n0 = result.iter().find(|&&(n, _)| n == nodes[0]).unwrap().1;
    let count_n1 = result.iter().find(|&&(n, _)| n == nodes[1]).unwrap().1;
    let count_n2 = result.iter().find(|&&(n, _)| n == nodes[2]).unwrap().1;
    assert_eq!(count_n0, 1, "n0 in exactly 1 triangle (self-loop ignored)");
    assert_eq!(count_n1, 1);
    assert_eq!(count_n2, 1);
}

#[test]
fn triangle_count_parallel_edges_collapse() {
    // §E25: triangle_count's binary-search adjacency dedupes parallels.
    // n0 → n1 with multiplicity 3, plus n1 → n2 and n2 → n0. Should be one
    // triangle; multiplicity must NOT inflate.
    let edges = vec![(0, 1), (0, 1), (0, 1), (1, 2), (2, 0)];
    let (shared, _) = build_graph(3, &edges);
    let proj = build_proj(&shared);
    let result = triangle_count(&proj, sequential_triangle_config());
    for &(_, count) in &result {
        assert_eq!(
            count, 1,
            "parallel edges collapse → still exactly 1 triangle"
        );
    }
}

#[test]
fn triangle_count_result_sorted_desc_count_then_asc_node_id() {
    // §E27 sort comparator: DESC by count, ASC by NodeId tie-break.
    // Construct a graph where n0..n2 have count 1 each (one triangle) and
    // n3 has count 0. Expected order: n0, n1, n2 (DESC count + ASC NodeId
    // tie-break) followed by n3.
    let (shared, nodes) = build_graph(4, &[(0, 1), (1, 2), (2, 0)]);
    let proj = build_proj(&shared);
    let result = triangle_count(&proj, sequential_triangle_config());
    let ordered_ids: Vec<NodeId> = result.iter().map(|&(n, _)| n).collect();
    assert_eq!(ordered_ids[0], nodes[0]);
    assert_eq!(ordered_ids[1], nodes[1]);
    assert_eq!(ordered_ids[2], nodes[2]);
    assert_eq!(ordered_ids[3], nodes[3], "zero-count node at the tail");
    // Verify counts are DESC.
    let counts: Vec<usize> = result.iter().map(|&(_, c)| c).collect();
    assert_eq!(counts, vec![1, 1, 1, 0]);
}

#[test]
fn triangle_count_handles_sparse_row_projection() {
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
    // Triangle on rows {10, 50, 90}.
    for &(s, t) in &[(10, 50), (50, 90), (90, 10)] {
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
            name: "sparse-tri".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        Some(&scope),
    )
    .unwrap();
    assert_eq!(proj.node_count(), 3);
    let result = triangle_count(&proj, sequential_triangle_config());
    assert_eq!(result.len(), 3);
    for &(_, count) in &result {
        assert_eq!(count, 1, "triangle on sparse rows still counts");
    }
}

#[test]
fn triangle_count_parallel_modes_match_sequential_output() {
    let (shared, _) = build_graph(
        6,
        &[
            (0, 1),
            (1, 2),
            (2, 0),
            (0, 3),
            (1, 3),
            (2, 3),
            (3, 4),
            (4, 5),
            (5, 3),
        ],
    );
    let proj = build_proj(&shared);
    let sequential = triangle_count(&proj, sequential_triangle_config());
    let auto = triangle_count(&proj, triangle_config(Parallelism::Auto));
    let threaded = triangle_count(&proj, triangle_config(threads4()));

    assert_eq!(auto, sequential);
    assert_eq!(threaded, sequential);
}

#[test]
fn triangle_count_parallel_result_is_deterministic_under_repetition() {
    let (shared, _) = build_graph(
        7,
        &[
            (0, 1),
            (1, 2),
            (2, 0),
            (0, 3),
            (1, 3),
            (2, 3),
            (3, 4),
            (4, 5),
            (5, 3),
            (4, 6),
        ],
    );
    let proj = build_proj(&shared);
    let expected = triangle_count(&proj, triangle_config(threads4()));

    for _ in 0..50 {
        let observed = triangle_count(&proj, triangle_config(threads4()));
        assert_eq!(observed, expected);
    }
}
