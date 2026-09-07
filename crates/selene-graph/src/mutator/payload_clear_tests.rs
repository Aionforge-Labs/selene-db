use selene_core::{
    DbString, EdgeId, GraphId, LabelSet, NodeId, PropertyMap, Value, VectorValue, db_string,
};

use super::*;
use crate::SharedGraph;

fn key(name: &str) -> DbString {
    db_string(name).expect("test string is valid")
}

fn vector(values: &[f32]) -> Value {
    Value::Vector(VectorValue::new(values.to_vec()).expect("test vector is valid"))
}

fn props(pairs: impl IntoIterator<Item = (DbString, Value)>) -> PropertyMap {
    PropertyMap::from_pairs(pairs).expect("property map is valid")
}

fn empty_node(mutator: &mut Mutator<'_, '_>) -> NodeId {
    mutator
        .create_node(LabelSet::new(), PropertyMap::new())
        .expect("create_node ok")
}

fn payload_node(mutator: &mut Mutator<'_, '_>) -> NodeId {
    mutator
        .create_node(
            LabelSet::single(key("payload.clear.node")),
            props([(key("embedding"), vector(&[1.0, 2.0, 3.0]))]),
        )
        .expect("create_node ok")
}

fn payload_edge(mutator: &mut Mutator<'_, '_>, source: NodeId, target: NodeId) -> EdgeId {
    mutator
        .create_edge(
            key("payload.clear.edge"),
            source,
            target,
            props([(key("embedding"), vector(&[4.0, 5.0, 6.0]))]),
        )
        .expect("create_edge ok")
}

#[test]
fn delete_node_clears_dead_row_payload_but_keeps_id_mapping() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    let id = {
        let mut mutator = txn.mutator();
        let id = payload_node(&mut mutator);
        mutator.delete_node(id).expect("delete_node ok");
        id
    };

    txn.commit().expect("commit ok");
    let graph = shared.read();
    let row = graph
        .node_row_for_id(id)
        .expect("dead id remains mapped")
        .index();
    assert!(!graph.node_store.is_alive(row as u32));
    assert_eq!(graph.node_store.row_to_id.get(row).copied(), Some(id));
    assert!(
        graph
            .node_store
            .labels
            .get(row)
            .expect("dead row keeps column slot")
            .is_empty()
    );
    assert!(
        graph
            .node_store
            .properties
            .get(row)
            .expect("dead row keeps column slot")
            .is_empty()
    );
}

#[test]
fn delete_edge_clears_dead_row_payload_but_keeps_id_mapping() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    let (source, target, edge) = {
        let mut mutator = txn.mutator();
        let source = empty_node(&mut mutator);
        let target = empty_node(&mut mutator);
        let edge = payload_edge(&mut mutator, source, target);
        mutator.delete_edge(edge).expect("delete_edge ok");
        (source, target, edge)
    };

    txn.commit().expect("commit ok");
    let graph = shared.read();
    let row = graph
        .edge_row_for_id(edge)
        .expect("dead id remains mapped")
        .index();
    assert!(graph.is_node_alive(source));
    assert!(graph.is_node_alive(target));
    assert!(!graph.edge_store.is_alive(row as u32));
    assert_eq!(graph.edge_store.row_to_id.get(row).copied(), Some(edge));
    assert_eq!(
        graph
            .edge_store
            .label
            .get(row)
            .expect("dead row keeps column slot")
            .as_str(),
        ""
    );
    assert_eq!(
        graph.edge_store.source.get(row).copied(),
        Some(NodeId::TOMBSTONE)
    );
    assert_eq!(
        graph.edge_store.target.get(row).copied(),
        Some(NodeId::TOMBSTONE)
    );
    assert!(
        graph
            .edge_store
            .properties
            .get(row)
            .expect("dead row keeps column slot")
            .is_empty()
    );
}
