//! Sparse stable IDs, deleted rows and a large shared-prefix label vocabulary.

use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap, Value, db_string};
use selene_graph::{SeleneGraph, SharedGraph, TypedIndexKind};

pub const LABELS: usize = 1_024;
pub const FIRST_ID: u64 = 1 << 40;

pub fn label(index: usize) -> DbString {
    db_string(&format!("Guard_shared_prefix_{:04}", index % LABELS)).unwrap()
}

pub fn sparse(scale: usize) -> SeleneGraph {
    let mut empty = SeleneGraph::new(GraphId::new(71));
    empty.meta.next_node_id = FIRST_ID;
    let shared = SharedGraph::from_graph(empty);
    let key = db_string("value").unwrap();
    let mut tx = shared.begin_write();
    let mut ids = Vec::with_capacity(scale);
    {
        let mut m = tx.mutator();
        for index in 0..scale {
            ids.push(
                m.create_node(
                    LabelSet::single(label(index)),
                    PropertyMap::from_pairs([(key.clone(), Value::Int(index as i64))]).unwrap(),
                )
                .unwrap(),
            );
        }
        for index in 0..scale {
            m.create_edge(
                label(index),
                ids[index],
                ids[(index + 1) % scale],
                PropertyMap::new(),
            )
            .unwrap();
        }
    }
    tx.commit().unwrap();
    let mut tx = shared.begin_write();
    {
        let mut m = tx.mutator();
        // Delete an uneven pattern so every label still has live witnesses.
        for id in ids.iter().step_by(5) {
            m.delete_node(*id).unwrap();
        }
    }
    tx.commit().unwrap();
    shared
        .create_property_index(label(1), key, TypedIndexKind::I64)
        .unwrap();
    let graph = shared.read().as_ref().clone();
    assert!(graph.node_properties(NodeId::new(FIRST_ID)).is_none());
    assert!(graph.node_properties(NodeId::new(FIRST_ID + 1)).is_some());
    graph.assert_indexes_consistent().unwrap();
    graph
}
