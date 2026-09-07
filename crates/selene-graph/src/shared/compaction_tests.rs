//! BRIEF-Item-4c live-densify tests for [`SharedGraph::compact`].
//!
//! `compact()` is the in-memory half of snapshot-time compaction: it rebuilds
//! the live graph dense and republishes it through the ArcSwap so RAM held by
//! deleted rows is reclaimed immediately, while external ids and the monotonic
//! high-water marks stay fixed. These tests drive churn → `compact()` → reads,
//! and the append-create-after-compact path that keeps the store dense.

use super::*;
use selene_core::{
    EdgeId, GraphId, LabelSet, NodeId, PropertyMap, PropertyValueType, Value, db_string,
};

use crate::store::NodeRow;
use crate::{GraphTypeDef, NodeTypeDef, PropertyTypeDef, ValidationMode};

fn prop(key: &str, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string(key).unwrap(), value)]).unwrap()
}

/// 5 nodes (ids 1..=5), then delete ids 2 and 4 — leaving 1, 3, 5 alive across
/// dead rows 1 and 3.
fn churned_graph() -> SharedGraph {
    let shared = SharedGraph::new(GraphId::new(1));
    let la = db_string("c4c.node").unwrap();
    let mut txn = shared.begin_write();
    {
        let mut m = txn.mutator();
        for i in 1..=5 {
            m.create_node(LabelSet::single(la.clone()), prop("n", Value::Int(i)))
                .unwrap();
        }
        m.delete_node(NodeId::new(2)).unwrap();
        m.delete_node(NodeId::new(4)).unwrap();
    }
    txn.commit().unwrap();
    shared
}

#[test]
fn compact_densifies_live_graph_and_reclaims_dead_rows() {
    let shared = churned_graph();
    {
        let before = shared.read();
        assert_eq!(before.node_store.len(), 5, "dead rows still occupy RAM");
        assert_eq!(before.node_count(), 3);
    }

    let report = shared.compact().unwrap();
    assert_eq!(
        report.reclaimed_nodes, 2,
        "the two deleted rows are reclaimed"
    );

    // The lock-free snapshot reflects the dense graph immediately (RAM reclaimed
    // in place — not deferred to a process restart).
    let g = shared.read();
    assert_eq!(g.node_store.len(), 3, "store densified in place");
    assert_eq!(g.node_count(), 3);
    // Survivors renumber dense in ascending old-row order: 1@0, 3@1, 5@2.
    assert_eq!(g.node_row_for_id(NodeId::new(1)), Some(NodeRow::new(0)));
    assert_eq!(g.node_row_for_id(NodeId::new(3)), Some(NodeRow::new(1)));
    assert_eq!(g.node_row_for_id(NodeId::new(5)), Some(NodeRow::new(2)));
    // Reclaimed ids resolve NotFound; the high-water is preserved (no reuse).
    assert!(g.node_row_for_id(NodeId::new(2)).is_none());
    assert!(g.node_row_for_id(NodeId::new(4)).is_none());
    assert_eq!(g.meta.next_node_id, 6);
}

#[test]
fn compaction_stats_track_delete_pressure_and_dense_rebuild() {
    let shared = churned_graph();

    let before = shared.compaction_stats();
    assert_eq!(before.allocated_nodes, 5);
    assert_eq!(before.live_nodes, 3);
    assert_eq!(before.reclaimable_nodes, 2);
    assert_eq!(before.allocated_edges, 0);
    assert_eq!(before.live_edges, 0);
    assert_eq!(before.reclaimable_edges, 0);
    assert_eq!(before.allocated_rows(), 5);
    assert_eq!(before.live_rows(), 3);
    assert_eq!(before.reclaimable_rows(), 2);
    assert!(!before.is_dense());

    let report = shared.compact().unwrap();
    assert_eq!(report.reclaimed_nodes, before.reclaimable_nodes);
    assert_eq!(report.reclaimed_edges, before.reclaimable_edges);

    let after = shared.compaction_stats();
    assert_eq!(after.allocated_nodes, 3);
    assert_eq!(after.live_nodes, 3);
    assert_eq!(after.reclaimable_nodes, 0);
    assert_eq!(after.allocated_rows(), 3);
    assert_eq!(after.live_rows(), 3);
    assert_eq!(after.reclaimable_rows(), 0);
    assert!(after.is_dense());
}

#[test]
fn compact_preserves_observable_reads() {
    let shared = churned_graph();
    let before = shared.read();
    shared.compact().unwrap();
    let g = shared.read();
    for id in [NodeId::new(1), NodeId::new(3), NodeId::new(5)] {
        assert_eq!(g.node_labels(id), before.node_labels(id), "labels {id}");
        assert_eq!(
            g.node_properties(id),
            before.node_properties(id),
            "properties {id}"
        );
    }
}

#[test]
fn create_after_compact_appends_without_rebloat() {
    // Live-densify analogue of the from_graph republish test: a create against
    // the freshly-compacted LIVE graph appends at the dense end and allocates the
    // preserved high-water id — it never reuses a reclaimed id nor re-pads holes.
    let shared = churned_graph();
    shared.compact().unwrap();

    let new_id = {
        let mut txn = shared.begin_write();
        let id = txn
            .mutator()
            .create_node(
                LabelSet::single(db_string("c4c.node").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
        txn.commit().unwrap();
        id
    };

    let g = shared.read();
    assert_eq!(new_id, NodeId::new(6), "preserved high-water, no reuse");
    assert_eq!(
        g.node_row_for_id(new_id),
        Some(NodeRow::new(3)),
        "appended at the dense end, not the high-water arith row"
    );
    assert_eq!(g.node_store.len(), 4, "dense, no re-bloat");
    assert!(g.is_node_alive(NodeId::new(1)) && g.is_node_alive(NodeId::new(6)));
    assert!(g.node_row_for_id(NodeId::new(2)).is_none());
}

#[test]
fn compact_on_a_dense_graph_is_a_noop() {
    let shared = SharedGraph::new(GraphId::new(1));
    let la = db_string("c4c.dense").unwrap();
    {
        let mut txn = shared.begin_write();
        {
            let mut m = txn.mutator();
            m.create_node(LabelSet::single(la.clone()), PropertyMap::new())
                .unwrap();
            m.create_node(LabelSet::single(la), PropertyMap::new())
                .unwrap();
        }
        txn.commit().unwrap();
    }
    let report = shared.compact().unwrap();
    assert_eq!(report.reclaimed_nodes, 0);
    assert_eq!(report.reclaimed_edges, 0);
    let g = shared.read();
    assert_eq!(g.node_store.len(), 2);
    assert_eq!(g.node_row_for_id(NodeId::new(1)), Some(NodeRow::new(0)));
    assert_eq!(g.node_row_for_id(NodeId::new(2)), Some(NodeRow::new(1)));
}

#[test]
fn compact_preserves_edges_and_adjacency() {
    // Live densify with edges: a cascade-deleted edge is reclaimed and the
    // surviving edges + adjacency rebuild correctly under the renumber (edges key
    // by stable external NodeId, so endpoints survive; adjacency is rebuilt).
    let shared = SharedGraph::new(GraphId::new(1));
    let la = db_string("c4c.n").unwrap();
    let el = db_string("c4c.e").unwrap();
    let (n1, n3, n4) = {
        let mut txn = shared.begin_write();
        let ids = {
            let mut m = txn.mutator();
            let n1 = m
                .create_node(LabelSet::single(la.clone()), PropertyMap::new())
                .unwrap();
            let n2 = m
                .create_node(LabelSet::single(la.clone()), PropertyMap::new())
                .unwrap();
            let n3 = m
                .create_node(LabelSet::single(la.clone()), PropertyMap::new())
                .unwrap();
            let n4 = m
                .create_node(LabelSet::single(la), PropertyMap::new())
                .unwrap();
            m.create_edge(el.clone(), n1, n2, PropertyMap::new())
                .unwrap(); // id 1 — cascade-deleted
            m.create_edge(el.clone(), n3, n4, PropertyMap::new())
                .unwrap(); // id 2 — survives
            m.create_edge(el, n1, n4, PropertyMap::new()).unwrap(); // id 3 — survives
            m.delete_node(n2).unwrap(); // cascades edge id 1
            (n1, n3, n4)
        };
        txn.commit().unwrap();
        ids
    };

    let before = shared.compaction_stats();
    assert_eq!(before.reclaimable_nodes, 1);
    assert_eq!(before.reclaimable_edges, 1);
    assert_eq!(before.reclaimable_rows(), 2);

    let report = shared.compact().unwrap();
    assert_eq!(report.reclaimed_nodes, before.reclaimable_nodes);
    assert_eq!(report.reclaimed_edges, before.reclaimable_edges);
    let g = shared.read();

    // Surviving edges resolve to renumbered dense rows with intact endpoints.
    assert_eq!(g.edge_endpoints(EdgeId::new(2)), Some((n3, n4)));
    assert_eq!(g.edge_endpoints(EdgeId::new(3)), Some((n1, n4)));
    assert!(
        g.edge_row_for_id(EdgeId::new(1)).is_none(),
        "the cascade-deleted edge was reclaimed"
    );
    // Adjacency rebuilt across the renumber.
    assert!(
        g.outgoing_edges(n1)
            .unwrap()
            .iter()
            .any(|e| e.edge_id == EdgeId::new(3) && e.neighbor == n4)
    );
    let n4_incoming: Vec<_> = g
        .incoming_edges(n4)
        .unwrap()
        .iter()
        .map(|e| e.edge_id)
        .collect();
    assert!(n4_incoming.contains(&EdgeId::new(2)) && n4_incoming.contains(&EdgeId::new(3)));
}

/// Minimal GG02 closed graph: one `Person` node type with a required `name`.
fn person_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("c4c.closed").unwrap(),
        node_types: vec![NodeTypeDef {
            name: db_string("c4c.person").unwrap(),
            key_labels: LabelSet::single(db_string("Person").unwrap()),
            properties: vec![PropertyTypeDef {
                name: db_string("name").unwrap(),
                value_type: PropertyValueType::String,
                list_element_type: None,
                required: true,
                default: None,
                immutable: false,
                unique: false,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                record_field_types: None,
            }],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    }
}

#[test]
fn compact_preserves_closed_graph_binding_on_the_live_path() {
    // The live densify republishes the dense graph directly into the ArcSwap
    // (not through the from_graph re-validation path), so prove the GG02 binding
    // survives structurally AND keeps validating live writes afterwards.
    let shared = SharedGraph::builder(GraphId::new(9))
        .bound_to(person_graph_type())
        .unwrap()
        .build()
        .unwrap();
    let person = db_string("Person").unwrap();
    let name = db_string("name").unwrap();
    let mk = |n: &str| {
        PropertyMap::from_pairs([(name.clone(), Value::String(db_string(n).unwrap()))]).unwrap()
    };
    {
        let mut txn = shared.begin_write();
        {
            let mut m = txn.mutator();
            m.create_node(LabelSet::single(person.clone()), mk("ann"))
                .unwrap();
            let bob = m
                .create_node(LabelSet::single(person.clone()), mk("bob"))
                .unwrap();
            m.create_node(LabelSet::single(person.clone()), mk("cy"))
                .unwrap();
            m.delete_node(bob).unwrap();
        }
        txn.commit().unwrap();
    }

    shared.compact().unwrap();

    {
        let g = shared.read();
        assert!(g.meta.bound_type.is_some(), "GG02 binding survived densify");
        assert!(g.is_node_alive(NodeId::new(1)));
        assert!(g.node_row_for_id(NodeId::new(2)).is_none(), "bob reclaimed");
    }
    // A conforming insert commits; a non-conforming one is still rejected.
    {
        let mut txn = shared.begin_write();
        {
            let mut m = txn.mutator();
            m.create_node(LabelSet::single(person.clone()), mk("dot"))
                .unwrap();
        }
        txn.commit()
            .expect("conforming insert commits post-compact");
    }
    {
        let mut txn = shared.begin_write();
        {
            let mut m = txn.mutator();
            let _ = m.create_node(LabelSet::single(person), PropertyMap::new());
        }
        assert!(
            txn.commit().is_err(),
            "the bound type still rejects a non-conforming node after live compaction"
        );
    }
}
