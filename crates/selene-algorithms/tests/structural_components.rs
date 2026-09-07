//! Integration tests for WCC and SCC under the projection's undirected /
//! directed views respectively (spec 16 §E09–§E12; BRIEF-52).

use proptest::prelude::*;
use selene_algorithms::{GraphProjection, ProjectionConfig, scc, scc_count, wcc, wcc_count};
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

/// Build a graph with `count` nodes and the supplied directed edges (by index).
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

// ---------------------------------------------------------------------------
// WCC
// ---------------------------------------------------------------------------

#[test]
fn wcc_empty_projection() {
    let shared = SharedGraph::new(GraphId::new(1));
    let proj = build_proj(&shared);
    assert!(wcc(&proj).is_empty(), "wcc(empty) must be empty per §E09");
    assert_eq!(wcc_count(&proj), 0);
}

#[test]
fn wcc_single_node_no_edges() {
    let (shared, nodes) = build_graph(1, &[]);
    let proj = build_proj(&shared);
    let result = wcc(&proj);
    assert_eq!(result, vec![(nodes[0], nodes[0].get())]);
    assert_eq!(wcc_count(&proj), 1);
}

#[test]
fn wcc_disconnected_components() {
    // Two components: {1, 2} and {3, 4}, plus isolated {5}.
    let (shared, nodes) = build_graph(5, &[(0, 1), (2, 3)]);
    let proj = build_proj(&shared);
    let result = wcc(&proj);

    // Pairs are ASC by NodeId; component_id is smallest NodeId in component.
    assert_eq!(
        result,
        vec![
            (nodes[0], nodes[0].get()),
            (nodes[1], nodes[0].get()),
            (nodes[2], nodes[2].get()),
            (nodes[3], nodes[2].get()),
            (nodes[4], nodes[4].get()),
        ]
    );
    assert_eq!(wcc_count(&proj), 3);
}

#[test]
fn wcc_directed_edge_creates_single_component() {
    // n1 -> n2 (directed). WCC treats this as undirected → one component.
    let (shared, nodes) = build_graph(2, &[(0, 1)]);
    let proj = build_proj(&shared);
    let result = wcc(&proj);
    assert_eq!(
        result,
        vec![(nodes[0], nodes[0].get()), (nodes[1], nodes[0].get())]
    );
    assert_eq!(wcc_count(&proj), 1);
}

#[test]
fn wcc_component_id_is_smallest_node_id_per_e12() {
    // Build a chain n1 -> n2 -> n3 but connect via a back-edge to expose the
    // canonical "smallest NodeId in component" rule even when union-find roots
    // pick a different representative internally.
    let (shared, nodes) = build_graph(4, &[(1, 2), (2, 3), (3, 1)]);
    let proj = build_proj(&shared);
    let result = wcc(&proj);
    // nodes[1..4] form one component with smallest id = nodes[1].get();
    // nodes[0] is isolated.
    assert_eq!(
        result,
        vec![
            (nodes[0], nodes[0].get()),
            (nodes[1], nodes[1].get()),
            (nodes[2], nodes[1].get()),
            (nodes[3], nodes[1].get()),
        ]
    );
}

// ---------------------------------------------------------------------------
// SCC
// ---------------------------------------------------------------------------

#[test]
fn scc_empty_projection() {
    let shared = SharedGraph::new(GraphId::new(1));
    let proj = build_proj(&shared);
    assert!(scc(&proj).is_empty(), "scc(empty) must be empty per §E09");
    assert_eq!(scc_count(&proj), 0);
}

#[test]
fn scc_single_node() {
    let (shared, nodes) = build_graph(1, &[]);
    let proj = build_proj(&shared);
    assert_eq!(scc(&proj), vec![(nodes[0], nodes[0].get())]);
    assert_eq!(scc_count(&proj), 1);
}

#[test]
fn scc_dag_each_node_is_own_scc() {
    // n1 -> n2 -> n3 directed acyclic — each node is its own SCC.
    let (shared, nodes) = build_graph(3, &[(0, 1), (1, 2)]);
    let proj = build_proj(&shared);
    let result = scc(&proj);
    assert_eq!(
        result,
        vec![
            (nodes[0], nodes[0].get()),
            (nodes[1], nodes[1].get()),
            (nodes[2], nodes[2].get()),
        ]
    );
    assert_eq!(scc_count(&proj), 3);
}

#[test]
fn scc_simple_cycle_is_one_scc() {
    // n1 -> n2 -> n3 -> n1.
    let (shared, nodes) = build_graph(3, &[(0, 1), (1, 2), (2, 0)]);
    let proj = build_proj(&shared);
    let result = scc(&proj);
    let root = nodes[0].get();
    assert_eq!(
        result,
        vec![(nodes[0], root), (nodes[1], root), (nodes[2], root),]
    );
    assert_eq!(scc_count(&proj), 1);
}

#[test]
fn scc_mixed_cycle_and_tail() {
    // Cycle {n1, n2, n3} with tail n3 -> n4. SCCs: {n1, n2, n3} and {n4}.
    let (shared, nodes) = build_graph(4, &[(0, 1), (1, 2), (2, 0), (2, 3)]);
    let proj = build_proj(&shared);
    let result = scc(&proj);
    let cycle_root = nodes[0].get();
    assert_eq!(
        result,
        vec![
            (nodes[0], cycle_root),
            (nodes[1], cycle_root),
            (nodes[2], cycle_root),
            (nodes[3], nodes[3].get()),
        ]
    );
    assert_eq!(scc_count(&proj), 2);
}

#[test]
fn scc_two_independent_cycles() {
    // Cycle A: n1 -> n2 -> n1; Cycle B: n3 -> n4 -> n3; no inter-cycle edges.
    let (shared, nodes) = build_graph(4, &[(0, 1), (1, 0), (2, 3), (3, 2)]);
    let proj = build_proj(&shared);
    let result = scc(&proj);
    assert_eq!(
        result,
        vec![
            (nodes[0], nodes[0].get()),
            (nodes[1], nodes[0].get()),
            (nodes[2], nodes[2].get()),
            (nodes[3], nodes[2].get()),
        ]
    );
    assert_eq!(scc_count(&proj), 2);
}

#[test]
fn scc_self_loop_is_own_scc() {
    // n1 -> n1; n2 isolated.
    let (shared, nodes) = build_graph(2, &[(0, 0)]);
    let proj = build_proj(&shared);
    let result = scc(&proj);
    assert_eq!(
        result,
        vec![(nodes[0], nodes[0].get()), (nodes[1], nodes[1].get())]
    );
    assert_eq!(scc_count(&proj), 2);
}

// ---------------------------------------------------------------------------
// Sparse-row sizing (spec 16 §E11 dense-row remap)
// ---------------------------------------------------------------------------

#[test]
fn wcc_handles_sparse_row_projection() {
    // Build 100 nodes; restrict the projection via a scope bitmap to rows
    // {0, 50, 99}. After the §E11 amendment, state arrays MUST be sized by
    // live count (3), not max_row + 1 (100). The test asserts correctness;
    // memory savings are implicit (allocating 100x for 3 nodes would not
    // fail the test but would be observable under heap profiling).
    let (shared, nodes) = build_graph(100, &[]);
    let snapshot = shared.read();
    let scope = snapshot
        .bind_node_candidates([nodes[0], nodes[50], nodes[99]])
        .unwrap();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "sparse".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        Some(&scope),
    )
    .unwrap();

    assert_eq!(proj.node_count(), 3);
    let result = wcc(&proj);
    // Three isolated nodes → three components; each is its own component_id.
    assert_eq!(
        result,
        vec![
            (nodes[0], nodes[0].get()),
            (nodes[50], nodes[50].get()),
            (nodes[99], nodes[99].get()),
        ]
    );
    assert_eq!(wcc_count(&proj), 3);
}

#[test]
fn scc_handles_sparse_row_projection() {
    // Same fixture as the WCC test, but with two edges among the scoped
    // nodes: n51 -> n52 and n52 -> n51 form a 2-cycle. Need to include 51/52
    // (rows 50/51) in scope along with row 99 (isolated).
    let (shared, nodes) = build_graph(100, &[(50, 51), (51, 50)]);
    let snapshot = shared.read();
    let scope = snapshot
        .bind_node_candidates([nodes[50], nodes[51], nodes[99]])
        .unwrap();
    let proj = GraphProjection::build(
        &snapshot,
        &ProjectionConfig {
            name: "sparse-scc".to_string(),
            node_labels: vec![],
            edge_labels: vec![],
            weight_property: None,
        },
        Some(&scope),
    )
    .unwrap();

    assert_eq!(proj.node_count(), 3);
    let result = scc(&proj);
    let cycle_root = nodes[50].get();
    assert_eq!(
        result,
        vec![
            (nodes[50], cycle_root),
            (nodes[51], cycle_root),
            (nodes[99], nodes[99].get()),
        ]
    );
    assert_eq!(scc_count(&proj), 2);
}

// ---------------------------------------------------------------------------
// Proptest invariants
// ---------------------------------------------------------------------------

proptest! {
    /// Determinism + partition invariants for WCC: result is sorted ASC by
    /// NodeId, every node appears exactly once, component IDs are valid node
    /// IDs in the projection.
    #[test]
    fn wcc_partition_invariant(
        edge_seed in 0u64..32u64,
        edge_count in 0usize..24usize,
    ) {
        let shared = SharedGraph::new(GraphId::new(1));
        let label = db_string("N");
        let rel = db_string("R");
        let mut txn = shared.begin_write();
        let mut nodes = Vec::with_capacity(8);
        for _ in 0..8 {
            let id = txn
                .mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .unwrap();
            nodes.push(id);
        }
        let mut state = edge_seed.wrapping_add(1);
        for _ in 0..edge_count {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let s = (state % 8) as usize;
            let t = ((state >> 8) % 8) as usize;
            txn.mutator()
                .create_edge(rel.clone(), nodes[s], nodes[t], PropertyMap::new())
                .unwrap();
        }
        txn.commit().unwrap();

        let proj = build_proj(&shared);
        let result = wcc(&proj);

        prop_assert_eq!(result.len(), 8);
        // Sorted ASC by NodeId.
        for w in result.windows(2) {
            prop_assert!(w[0].0.get() < w[1].0.get());
        }
        // Every reported component_id is the NodeId of some node in the result.
        let valid_ids: std::collections::HashSet<u64> =
            result.iter().map(|&(n, _)| n.get()).collect();
        for &(_, cid) in &result {
            prop_assert!(valid_ids.contains(&cid));
        }
        // wcc_count agrees with the distinct component_ids reported by wcc.
        let distinct: std::collections::HashSet<u64> =
            result.iter().map(|&(_, c)| c).collect();
        prop_assert_eq!(distinct.len(), wcc_count(&proj));
    }

    /// SCC partition invariants: each node appears once; result sorted ASC;
    /// component_id is a valid NodeId in the projection.
    #[test]
    fn scc_partition_invariant(
        edge_seed in 0u64..32u64,
        edge_count in 0usize..24usize,
    ) {
        let shared = SharedGraph::new(GraphId::new(1));
        let label = db_string("N");
        let rel = db_string("R");
        let mut txn = shared.begin_write();
        let mut nodes = Vec::with_capacity(8);
        for _ in 0..8 {
            let id = txn
                .mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .unwrap();
            nodes.push(id);
        }
        let mut state = edge_seed.wrapping_add(1);
        for _ in 0..edge_count {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let s = (state % 8) as usize;
            let t = ((state >> 8) % 8) as usize;
            txn.mutator()
                .create_edge(rel.clone(), nodes[s], nodes[t], PropertyMap::new())
                .unwrap();
        }
        txn.commit().unwrap();

        let proj = build_proj(&shared);
        let result = scc(&proj);

        prop_assert_eq!(result.len(), 8);
        for w in result.windows(2) {
            prop_assert!(w[0].0.get() < w[1].0.get());
        }
        let valid_ids: std::collections::HashSet<u64> =
            result.iter().map(|&(n, _)| n.get()).collect();
        for &(_, cid) in &result {
            prop_assert!(valid_ids.contains(&cid));
        }
        let distinct: std::collections::HashSet<u64> =
            result.iter().map(|&(_, c)| c).collect();
        prop_assert_eq!(distinct.len(), scc_count(&proj));
    }
}
