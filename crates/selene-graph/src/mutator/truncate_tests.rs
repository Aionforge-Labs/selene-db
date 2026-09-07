//! Runtime tests for `IM_TRUNCATE` bulk truncate (BRIEF-150, audit Item 11).
//!
//! Invariants under test:
//! - TRUNCATE NODE TYPE :L is observationally identical to
//!   `MATCH (n:L) DETACH DELETE n` (same final graph state, no dangling edges).
//! - Exactly ONE declarative change is written regardless of N (O(1) WAL).
//! - Empty/absent label is a clean no-op; double-truncate is idempotent.

use selene_core::{Change, EdgeId, GraphId, NodeId, PropertyMap, Value, db_string};

use super::*;
use crate::SharedGraph;

fn prop(key: &str, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string(key).unwrap(), value)]).unwrap()
}

/// Build a fixture: K nodes of label :L (each carrying an extra label and a
/// property index target), incident edges of TWO edge types between them and to
/// some non-:L nodes, plus a non-:L node/edge that must survive truncation.
///
/// Returns the shared graph (uncommitted txn already committed) so callers can
/// open a fresh txn and truncate or detach-delete.
fn fixture() -> SharedGraph {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    {
        let mut m = txn.mutator();
        let l = db_string("trunc.L").unwrap();
        let other = db_string("trunc.Other").unwrap();
        let keep = db_string("trunc.Keep").unwrap();
        // 4 nodes of :L (rows 0..3), 1 keep node (row 4).
        let l0 = m
            .create_node(LabelSet::single(l.clone()), prop("k", Value::Int(0)))
            .unwrap();
        let l1 = m
            .create_node(
                LabelSet::from_iter([l.clone(), other]),
                prop("k", Value::Int(1)),
            )
            .unwrap();
        let l2 = m
            .create_node(LabelSet::single(l.clone()), prop("k", Value::Int(2)))
            .unwrap();
        let l3 = m
            .create_node(LabelSet::single(l), prop("k", Value::Int(3)))
            .unwrap();
        let keep_node = m
            .create_node(LabelSet::single(keep), prop("k", Value::Int(9)))
            .unwrap();
        let e1 = db_string("trunc.E1").unwrap();
        let e2 = db_string("trunc.E2").unwrap();
        // Incident edges of two types: L->L, L->keep, keep->L.
        m.create_edge(e1.clone(), l0, l1, PropertyMap::new())
            .unwrap();
        m.create_edge(e2.clone(), l1, l2, PropertyMap::new())
            .unwrap();
        m.create_edge(e1.clone(), l3, keep_node, PropertyMap::new())
            .unwrap();
        m.create_edge(e2, keep_node, l0, PropertyMap::new())
            .unwrap();
        // Edge entirely among survivors must be untouched.
        m.create_edge(e1, keep_node, keep_node, PropertyMap::new())
            .unwrap();
    }
    txn.commit().unwrap();
    shared
}

fn compacted_fixture_with_non_identity_ids() -> (
    SharedGraph,
    DbString,
    NodeId,
    NodeId,
    NodeId,
    NodeId,
    EdgeId,
) {
    let shared = SharedGraph::new(GraphId::new(2));
    let target_label = db_string("trunc.compact.Target").unwrap();
    let keep_label = db_string("trunc.compact.Keep").unwrap();
    let edge_label = db_string("trunc.compact.Edge").unwrap();
    let (keep_a, target_a, target_b, keep_b, survivor_edge) = {
        let mut txn = shared.begin_write();
        let ids = {
            let mut m = txn.mutator();
            let keep_a = m
                .create_node(
                    LabelSet::single(keep_label.clone()),
                    prop("k", Value::Int(1)),
                )
                .unwrap();
            let dead = m
                .create_node(
                    LabelSet::single(keep_label.clone()),
                    prop("k", Value::Int(2)),
                )
                .unwrap();
            let target_a = m
                .create_node(
                    LabelSet::single(target_label.clone()),
                    prop("k", Value::Int(3)),
                )
                .unwrap();
            let target_b = m
                .create_node(
                    LabelSet::single(target_label.clone()),
                    prop("k", Value::Int(4)),
                )
                .unwrap();
            let keep_b = m
                .create_node(LabelSet::single(keep_label), prop("k", Value::Int(5)))
                .unwrap();
            m.create_edge(edge_label.clone(), keep_a, target_a, PropertyMap::new())
                .unwrap();
            m.create_edge(edge_label.clone(), target_a, target_b, PropertyMap::new())
                .unwrap();
            let survivor_edge = m
                .create_edge(edge_label, keep_a, keep_b, PropertyMap::new())
                .unwrap();
            m.delete_node(dead).unwrap();
            (keep_a, target_a, target_b, keep_b, survivor_edge)
        };
        txn.commit().unwrap();
        ids
    };

    shared.compact().unwrap();
    {
        let g = shared.read();
        let row = g
            .node_row_for_id(target_a)
            .expect("target_a survives compaction");
        assert_ne!(
            u64::from(row.get()) + 1,
            target_a.get(),
            "fixture must prove row/id identity is false after compaction"
        );
    }

    (
        shared,
        target_label,
        keep_a,
        target_a,
        target_b,
        keep_b,
        survivor_edge,
    )
}

fn live_node_ids_with_label(graph: &crate::SeleneGraph, label: &DbString) -> Vec<NodeId> {
    graph
        .node_candidates_with_label(label)
        .map(|candidates| candidates.iter().collect())
        .unwrap_or_default()
}

/// Compare the observable state of two graphs that should be identical after a
/// truncate vs DETACH DELETE of the same set.
fn assert_same_observable_state(a: &crate::SeleneGraph, b: &crate::SeleneGraph) {
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
    // Surviving node labels/properties must match row-for-row.
    for row in a.node_store.alive.iter() {
        let row = row as usize;
        assert_eq!(
            a.node_store.labels.get(row),
            b.node_store.labels.get(row),
            "surviving node row {row} labels differ"
        );
        assert_eq!(
            a.node_store.properties.get(row),
            b.node_store.properties.get(row),
            "surviving node row {row} properties differ"
        );
    }
}

#[test]
fn truncate_node_type_matches_detach_delete_observable_state() {
    let truncated = fixture();
    let detached = fixture();
    let l = db_string("trunc.L").unwrap();

    // TRUNCATE NODE TYPE :L
    {
        let mut txn = truncated.begin_write();
        txn.mutator().truncate_node_type(l.clone()).unwrap();
        txn.commit().unwrap();
    }
    // MATCH (n:L) DETACH DELETE n — resolve matched ids from the label index,
    // then per-row delete_node (which cascades incident edges).
    {
        let mut txn = detached.begin_write();
        let matched = live_node_ids_with_label(txn.read(), &l);
        {
            let mut m = txn.mutator();
            for id in matched {
                m.delete_node(id).unwrap();
            }
        }
        txn.commit().unwrap();
    }

    assert_same_observable_state(&truncated.read(), &detached.read());
    // No node of :L survives, and no dangling edge: every alive edge's
    // endpoints must be alive nodes.
    let g = truncated.read();
    assert!(g.nodes_with_label(&l).is_none(), "no :L nodes survive");
    for row in g.edge_store.alive.iter() {
        let row = row as usize;
        let source = *g.edge_store.source.get(row).unwrap();
        let target = *g.edge_store.target.get(row).unwrap();
        assert!(
            g.is_node_alive(source),
            "edge row {row} has dangling source"
        );
        assert!(
            g.is_node_alive(target),
            "edge row {row} has dangling target"
        );
    }
}

#[test]
fn truncate_node_type_after_compaction_uses_external_id_maps() {
    let (shared, target_label, keep_a, target_a, target_b, keep_b, survivor_edge) =
        compacted_fixture_with_non_identity_ids();

    let mut txn = shared.begin_write();
    txn.mutator()
        .truncate_node_type(target_label.clone())
        .unwrap();
    let outcome = txn.commit().unwrap();

    assert_eq!(outcome.changes.len(), 1);
    assert!(matches!(
        &outcome.changes[0],
        Change::NodesOfTypeTruncated { label } if *label == target_label
    ));
    let g = shared.read();
    assert!(g.is_node_alive(keep_a));
    assert!(g.is_node_alive(keep_b));
    assert!(!g.is_node_alive(target_a));
    assert!(!g.is_node_alive(target_b));
    assert!(
        g.node_row_for_id(target_a).is_some(),
        "truncate leaves the deleted id mapped to its dead row until compaction"
    );
    assert!(
        g.nodes_with_label(&target_label).is_none(),
        "target label index must be fully cleared"
    );
    assert!(
        g.is_edge_alive(survivor_edge),
        "edge between surviving non-target nodes must remain alive"
    );
    assert_eq!(g.edge_count(), 1, "all target-incident edges cascade away");
}

#[test]
fn truncate_writes_exactly_one_change_regardless_of_n() {
    let shared = fixture();
    let l = db_string("trunc.L").unwrap();
    let mut txn = shared.begin_write();
    txn.mutator().truncate_node_type(l.clone()).unwrap();
    let outcome = txn.commit().unwrap();
    // 4 :L nodes + 4 incident edges removed, but the persisted changeset carries
    // exactly ONE declarative change (the O(1)-WAL invariant).
    assert_eq!(
        outcome.changes.len(),
        1,
        "expected exactly one persisted change"
    );
    assert!(matches!(
        &outcome.changes[0],
        Change::NodesOfTypeTruncated { label } if *label == l
    ));
}

#[test]
fn truncate_edge_type_writes_one_change_and_removes_only_that_type() {
    let shared = fixture();
    let e1 = db_string("trunc.E1").unwrap();
    let mut txn = shared.begin_write();
    txn.mutator().truncate_edge_type(e1.clone()).unwrap();
    let outcome = txn.commit().unwrap();
    assert_eq!(outcome.changes.len(), 1);
    assert!(matches!(
        &outcome.changes[0],
        Change::EdgesOfTypeTruncated { label } if *label == e1
    ));
    let g = shared.read();
    assert!(g.edges_with_label(&e1).is_none(), "all E1 edges removed");
    // E2 edges and all nodes untouched.
    let e2 = db_string("trunc.E2").unwrap();
    assert!(g.edges_with_label(&e2).is_some(), "E2 edges survive");
    assert_eq!(g.node_count(), 5, "truncate edge type leaves nodes intact");
}

#[test]
fn truncate_absent_label_is_clean_noop() {
    let shared = fixture();
    let absent = db_string("trunc.NoSuchLabel").unwrap();
    let mut txn = shared.begin_write();
    txn.mutator().truncate_node_type(absent).unwrap();
    let outcome = txn.commit().unwrap();
    assert!(outcome.changes.is_empty(), "absent label writes no change");
    assert_eq!(shared.read().node_count(), 5);
}

#[test]
fn double_truncate_is_idempotent() {
    let shared = fixture();
    let l = db_string("trunc.L").unwrap();
    {
        let mut txn = shared.begin_write();
        txn.mutator().truncate_node_type(l.clone()).unwrap();
        txn.commit().unwrap();
    }
    let mut txn = shared.begin_write();
    txn.mutator().truncate_node_type(l).unwrap();
    let outcome = txn.commit().unwrap();
    // Second truncate finds no :L rows (index removed), so it is a no-op.
    assert!(outcome.changes.is_empty(), "second truncate writes nothing");
}
