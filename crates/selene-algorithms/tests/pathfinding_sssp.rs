//! Integration tests for `sssp` per spec 16 §E17.

use selene_algorithms::{GraphProjection, PathfindingError, ProjectionConfig, sssp};
use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap, Value};
use selene_graph::SharedGraph;

fn db_string(name: &str) -> DbString {
    selene_core::db_string(name).unwrap()
}

fn weight_props(value: f64) -> PropertyMap {
    PropertyMap::from_pairs([(db_string("w"), Value::Float(value))]).unwrap()
}

fn build_proj(shared: &SharedGraph, weighted: bool) -> GraphProjection {
    let snapshot = shared.read();
    GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "test".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: weighted.then(|| db_string("w")),
        },
        None,
    )
    .unwrap()
}

fn build_graph(count: usize, edges: &[(usize, usize, f64)]) -> (SharedGraph, Vec<NodeId>) {
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
    for &(s, t, w) in edges {
        txn.mutator()
            .create_edge(rel.clone(), nodes[s], nodes[t], weight_props(w))
            .unwrap();
    }
    txn.commit().unwrap();
    (shared, nodes)
}

#[test]
fn sssp_empty_projection_returns_empty() {
    let shared = SharedGraph::new(GraphId::new(1));
    let proj = build_proj(&shared, false);
    assert!(sssp(&proj, NodeId::new(1)).unwrap().is_empty());
}

#[test]
fn sssp_source_not_in_projection_returns_empty() {
    let (shared, _) = build_graph(2, &[(0, 1, 1.0)]);
    let proj = build_proj(&shared, true);
    assert!(sssp(&proj, NodeId::new(999)).unwrap().is_empty());
}

#[test]
fn sssp_single_isolated_node_returns_just_source() {
    let (shared, nodes) = build_graph(1, &[]);
    let proj = build_proj(&shared, true);
    let result = sssp(&proj, nodes[0]).unwrap();
    assert_eq!(result, vec![(nodes[0], 0.0)]);
}

#[test]
fn sssp_chain_distances_are_cumulative() {
    let (shared, nodes) = build_graph(4, &[(0, 1, 1.0), (1, 2, 2.0), (2, 3, 3.0)]);
    let proj = build_proj(&shared, true);
    let result = sssp(&proj, nodes[0]).unwrap();
    assert_eq!(
        result,
        vec![
            (nodes[0], 0.0),
            (nodes[1], 1.0),
            (nodes[2], 3.0),
            (nodes[3], 6.0),
        ]
    );
}

#[test]
fn sssp_unreachable_nodes_excluded() {
    let (shared, nodes) = build_graph(3, &[(0, 1, 1.0)]);
    let proj = build_proj(&shared, true);
    let result = sssp(&proj, nodes[0]).unwrap();
    assert_eq!(result, vec![(nodes[0], 0.0), (nodes[1], 1.0)]);
    assert!(
        !result.iter().any(|&(n, _)| n == nodes[2]),
        "n2 is unreachable; must not appear"
    );
}

#[test]
fn sssp_source_self_distance_is_zero_with_self_loop() {
    // §E17 — self-loops at source MUST NOT affect the source's distance.
    let (shared, nodes) = build_graph(1, &[(0, 0, 7.0)]);
    let proj = build_proj(&shared, true);
    let result = sssp(&proj, nodes[0]).unwrap();
    assert_eq!(result, vec![(nodes[0], 0.0)]);
}

#[test]
fn sssp_chooses_shorter_route_in_diamond() {
    // n0 --10--> n1 --5--> n3 (cost 15)
    // n0 --20--> n2 --4--> n3 (cost 24)
    let (shared, nodes) = build_graph(4, &[(0, 1, 10.0), (1, 3, 5.0), (0, 2, 20.0), (2, 3, 4.0)]);
    let proj = build_proj(&shared, true);
    let result = sssp(&proj, nodes[0]).unwrap();
    let dist_to_n3 = result
        .iter()
        .find(|&&(n, _)| n == nodes[3])
        .map(|&(_, d)| d);
    assert_eq!(dist_to_n3, Some(15.0));
}

#[test]
fn sssp_negative_weight_raises_error() {
    let (shared, nodes) = build_graph(2, &[(0, 1, -2.0)]);
    let proj = build_proj(&shared, true);
    let err = sssp(&proj, nodes[0]).unwrap_err();
    let PathfindingError::NegativeWeight { weight, .. } = err else {
        panic!("expected NegativeWeight, got {err:?}");
    };
    assert_eq!(weight, -2.0);
}

#[test]
fn sssp_nan_weight_raises_error() {
    let (shared, nodes) = build_graph(2, &[(0, 1, f64::NAN)]);
    let proj = build_proj(&shared, true);
    let err = sssp(&proj, nodes[0]).unwrap_err();
    assert!(matches!(err, PathfindingError::NaNWeight { .. }));
}

#[test]
fn sssp_result_sorted_asc_by_nodeid() {
    // Multiple reachable targets at varying NodeIds; result must be ASC.
    let (shared, nodes) = build_graph(4, &[(0, 1, 1.0), (0, 2, 1.0), (0, 3, 1.0)]);
    let proj = build_proj(&shared, true);
    let result = sssp(&proj, nodes[0]).unwrap();
    for w in result.windows(2) {
        assert!(w[0].0.get() < w[1].0.get(), "sssp result not ASC by NodeId");
    }
}

#[test]
fn sssp_handles_sparse_row_projection() {
    // Same §E14 sparse-row trap test as BRIEF-52 components: 100 nodes,
    // scope bitmap restricting to rows {0, 50, 99}, expect state arrays
    // sized by 3 not 100.
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
    // Connect 0 -> 50 -> 99 within the scope (other rows are filtered out).
    txn.mutator()
        .create_edge(rel.clone(), nodes[0], nodes[50], weight_props(2.5))
        .unwrap();
    txn.mutator()
        .create_edge(rel, nodes[50], nodes[99], weight_props(3.5))
        .unwrap();
    txn.commit().unwrap();

    let snapshot = shared.read();
    let scope = snapshot
        .bind_node_candidates([nodes[0], nodes[50], nodes[99]])
        .unwrap();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "sparse-sssp".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: Some(db_string("w")),
        },
        Some(&scope),
    )
    .unwrap();

    assert_eq!(proj.node_count(), 3);
    let result = sssp(&proj, nodes[0]).unwrap();
    assert_eq!(
        result,
        vec![(nodes[0], 0.0), (nodes[50], 2.5), (nodes[99], 6.0)]
    );
}
