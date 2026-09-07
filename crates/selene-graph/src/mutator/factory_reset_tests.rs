//! Runtime tests for `DROP GRAPH` factory-reset (BRIEF-152, audit Item 10).
//!
//! Invariants under test:
//! - factory_reset wipes ALL nodes/edges INCLUDING untyped/arbitrary-label rows
//!   (uses live_nodes/live_edges, not per-type) — observationally equal to
//!   `MATCH (n) DETACH DELETE n` + full schema drop.
//! - The schema resets to open (bound_type -> None): a previously closed GG02
//!   graph becomes open and accepts a previously-invalid insert.
//! - Exactly ONE `Change::GraphReset` is written regardless of N (O(1) WAL).
//! - Idempotent: a second DROP GRAPH on an empty + open graph is a clean no-op.

use selene_core::{Change, GraphId, NodeId, PropertyMap, Value, db_string};

use super::*;
use crate::SharedGraph;

fn prop(key: &str, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string(key).unwrap(), value)]).unwrap()
}

/// Open (GG01) fixture mixing TYPED-looking and UNTYPED nodes/edges, plus an
/// edge between two untyped nodes whose label is not a declared type. A per-type
/// truncate would never reach the untyped rows; factory_reset must.
fn open_fixture() -> SharedGraph {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    {
        let mut m = txn.mutator();
        let labelled = db_string("fr.Labelled").unwrap();
        let other = db_string("fr.Other").unwrap();
        let a = m
            .create_node(LabelSet::single(labelled.clone()), prop("k", Value::Int(0)))
            .unwrap();
        let b = m
            .create_node(
                LabelSet::from_iter([labelled, other]),
                prop("k", Value::Int(1)),
            )
            .unwrap();
        // An UNTYPED node: empty label set, legal under GG01.
        let untyped = m
            .create_node(LabelSet::new(), prop("k", Value::Int(2)))
            .unwrap();
        let e = db_string("fr.E").unwrap();
        m.create_edge(e.clone(), a, b, PropertyMap::new()).unwrap();
        // Edge touching the untyped node — must still be wiped.
        m.create_edge(e.clone(), b, untyped, PropertyMap::new())
            .unwrap();
        m.create_edge(e, untyped, a, PropertyMap::new()).unwrap();
    }
    txn.commit().unwrap();
    shared
}

fn compacted_open_fixture_with_non_identity_ids() -> (SharedGraph, NodeId, NodeId) {
    let shared = SharedGraph::new(GraphId::new(4));
    let label = db_string("fr.compact.Labelled").unwrap();
    let edge_label = db_string("fr.compact.Edge").unwrap();
    let (keep_a, keep_b) = {
        let mut txn = shared.begin_write();
        let ids = {
            let mut m = txn.mutator();
            let keep_a = m
                .create_node(LabelSet::single(label.clone()), prop("k", Value::Int(1)))
                .unwrap();
            let dead = m
                .create_node(LabelSet::single(label.clone()), prop("k", Value::Int(2)))
                .unwrap();
            let keep_b = m
                .create_node(LabelSet::new(), prop("k", Value::Int(3)))
                .unwrap();
            m.create_edge(edge_label, keep_a, keep_b, PropertyMap::new())
                .unwrap();
            m.delete_node(dead).unwrap();
            (keep_a, keep_b)
        };
        txn.commit().unwrap();
        ids
    };

    shared.compact().unwrap();
    {
        let g = shared.read();
        let row = g
            .node_row_for_id(keep_b)
            .expect("keep_b survives compaction");
        assert_ne!(
            u64::from(row.get()) + 1,
            keep_b.get(),
            "fixture must prove row/id identity is false after compaction"
        );
    }

    (shared, keep_a, keep_b)
}

fn live_node_ids(graph: &crate::SeleneGraph) -> Vec<NodeId> {
    graph.live_node_candidates().unwrap().iter().collect()
}

#[test]
fn factory_reset_wipes_all_nodes_and_edges_including_untyped() {
    let shared = open_fixture();
    assert_eq!(shared.read().node_count(), 3);
    assert_eq!(shared.read().edge_count(), 3);

    let mut txn = shared.begin_write();
    txn.mutator().factory_reset().unwrap();
    txn.commit().unwrap();

    let g = shared.read();
    assert_eq!(g.node_count(), 0, "all nodes wiped, incl untyped");
    assert_eq!(g.edge_count(), 0, "all edges wiped, incl untyped-incident");
    assert!(
        (&g.idx_label)
            .into_iter()
            .all(|(_, bitmap)| bitmap.is_empty())
    );
    assert!(
        (&g.idx_edge_label)
            .into_iter()
            .all(|(_, bitmap)| bitmap.is_empty()),
        "edge-label index buckets cleared"
    );
    assert_eq!(g.adjacency_out.len(), 0, "outgoing adjacency cleared");
    assert_eq!(g.adjacency_in.len(), 0, "incoming adjacency cleared");
}

#[test]
fn factory_reset_after_compaction_uses_external_id_maps() {
    let (shared, keep_a, keep_b) = compacted_open_fixture_with_non_identity_ids();

    let mut txn = shared.begin_write();
    txn.mutator().factory_reset().unwrap();
    let outcome = txn.commit().unwrap();

    assert_eq!(outcome.changes.len(), 1);
    assert!(matches!(outcome.changes[0], Change::GraphReset {}));
    let g = shared.read();
    assert_eq!(g.node_count(), 0);
    assert_eq!(g.edge_count(), 0);
    assert!(!g.is_node_alive(keep_a));
    assert!(!g.is_node_alive(keep_b));
    assert!(
        g.node_row_for_id(keep_b).is_some(),
        "reset leaves deleted ids mapped to dead rows until compaction"
    );
    assert_eq!(g.idx_label.len(), 0, "node label index cleared");
    assert_eq!(g.idx_edge_label.len(), 0, "edge label index cleared");
    assert_eq!(g.adjacency_out.len(), 0, "outgoing adjacency cleared");
    assert_eq!(g.adjacency_in.len(), 0, "incoming adjacency cleared");
}

#[test]
fn factory_reset_writes_exactly_one_change_regardless_of_n() {
    let shared = open_fixture();
    let mut txn = shared.begin_write();
    txn.mutator().factory_reset().unwrap();
    let outcome = txn.commit().unwrap();
    // 3 nodes + 3 edges removed, but the persisted changeset carries exactly ONE
    // declarative change (the O(1)-WAL invariant).
    assert_eq!(outcome.changes.len(), 1, "exactly one persisted change");
    assert!(matches!(outcome.changes[0], Change::GraphReset {}));
}

#[test]
fn factory_reset_resets_closed_graph_to_open() {
    use crate::{NodeTypeDef, PropertyTypeDef, ValidationMode};
    use selene_core::PropertyValueType;

    // A closed (GG02) graph with a strict Person type requiring `name`.
    let graph_type = GraphTypeDef {
        name: db_string("fr.person.graph").unwrap(),
        node_types: vec![NodeTypeDef {
            name: db_string("Person").unwrap(),
            key_labels: LabelSet::single(db_string("Person").unwrap()),
            properties: vec![PropertyTypeDef {
                name: db_string("name").unwrap(),
                value_type: PropertyValueType::String,
                list_element_type: None,
                required: true,
                immutable: false,
                default: None,
                unique: false,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                record_field_types: None,
            }],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: vec![],
    };
    let shared = SharedGraph::builder(GraphId::new(2))
        .bound_to(graph_type)
        .unwrap()
        .build()
        .unwrap();
    assert!(shared.is_closed());

    // Seed a valid Person, then factory-reset.
    {
        let mut txn = shared.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(db_string("Person").unwrap()),
                prop("name", Value::String(db_string("Alice").unwrap())),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    {
        let mut txn = shared.begin_write();
        txn.mutator().factory_reset().unwrap();
        txn.commit().unwrap();
    }

    let g = shared.read();
    assert_eq!(g.node_count(), 0);
    assert!(g.meta.bound_type.is_none(), "schema reset to open (None)");
    assert!(!shared.is_closed(), "graph is open after DROP GRAPH");

    // A node WITHOUT the required `name` would have been a GG02 violation before
    // the reset; under the now-open graph it commits cleanly.
    let mut txn = shared.begin_write();
    txn.mutator()
        .create_node(
            LabelSet::single(db_string("Person").unwrap()),
            PropertyMap::new(),
        )
        .unwrap();
    txn.commit()
        .expect("open graph accepts a node that violated the dropped GG02 type");
    assert_eq!(shared.read().node_count(), 1);
}

#[test]
fn factory_reset_on_empty_open_graph_is_clean_noop() {
    let shared = SharedGraph::new(GraphId::new(3));
    // First reset on an empty graph: still emits one GraphReset (observable
    // no-op), never an error.
    let mut txn = shared.begin_write();
    txn.mutator().factory_reset().unwrap();
    let outcome = txn.commit().unwrap();
    assert_eq!(
        outcome.changes.len(),
        1,
        "empty reset still writes GraphReset"
    );
    assert!(matches!(outcome.changes[0], Change::GraphReset {}));
    assert_eq!(shared.read().node_count(), 0);
    assert!(shared.read().meta.bound_type.is_none());

    // Double DROP GRAPH succeeds again.
    let mut txn = shared.begin_write();
    txn.mutator().factory_reset().unwrap();
    txn.commit().expect("double DROP GRAPH is idempotent");
}
#[test]
fn factory_reset_matches_detach_delete_plus_schema_drop_observable_state() {
    // factory_reset must leave the SAME alive bitmaps + indexes as manually
    // deleting every node and clearing the bound type.
    let reset = open_fixture();
    let manual = open_fixture();

    {
        let mut txn = reset.begin_write();
        txn.mutator().factory_reset().unwrap();
        txn.commit().unwrap();
    }
    {
        let mut txn = manual.begin_write();
        let ids = live_node_ids(txn.read());
        {
            let mut m = txn.mutator();
            for id in ids {
                m.delete_node(id).unwrap();
            }
        }
        txn.commit().unwrap();
    }

    let a = reset.read();
    let b = manual.read();
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
