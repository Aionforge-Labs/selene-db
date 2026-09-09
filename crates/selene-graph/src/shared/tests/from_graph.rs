use super::*;

#[test]
fn from_graph_floor_derives_allocator_from_storage_when_meta_is_stale() {
    use crate::SeleneGraph;
    use selene_core::{LabelSet, PropertyMap};

    let mut graph = SeleneGraph::new(GraphId::new(1));
    graph.node_store.labels.push(LabelSet::new());
    graph.node_store.properties.push(PropertyMap::new());
    graph.node_store.alive_mut().insert(0);
    graph
        .edge_store
        .label
        .push(selene_core::db_string("e").unwrap());
    graph.edge_store.source.push(selene_core::NodeId::new(1));
    graph
        .edge_store
        .directionality
        .push(selene_core::EdgeDirectionality::Directed);
    graph.edge_store.target.push(selene_core::NodeId::new(1));
    graph.edge_store.properties.push(PropertyMap::new());
    graph.edge_store.alive_mut().insert(0);
    // Stale meta: still says next is 1 even though one row of each exists.
    graph.meta.next_node_id = 1;
    graph.meta.next_edge_id = 1;

    let shared = SharedGraph::from_graph(graph);
    let mut txn = shared.begin_write();
    let id = {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok")
    };
    // Storage floor (len + 1 = 2) overrode the stale meta floor of 1, so
    // the allocator returned NodeId(2), not the colliding NodeId(1).
    assert_eq!(id, selene_core::NodeId::new(2));
    txn.commit().unwrap();
}

#[test]
fn from_graph_rebuilds_label_indexes_from_stores() {
    use selene_core::{LabelSet, NodeId as CoreNodeId, PropertyMap, db_string};

    let label_a = db_string("idx.a").unwrap();
    let label_b = db_string("idx.b").unwrap();

    let mut graph = SeleneGraph::new(GraphId::new(1));
    // Row 0: alive, labels {a, b}.
    let mut labels0 = LabelSet::new();
    labels0.insert(label_a.clone());
    labels0.insert(label_b.clone());
    graph.node_store.labels.push(labels0);
    graph.node_store.properties.push(PropertyMap::new());
    graph.node_store.alive_mut().insert(0);
    // Row 1: dead, label {a} — must NOT appear in the rebuilt index.
    graph
        .node_store
        .labels
        .push(LabelSet::single(label_a.clone()));
    graph.node_store.properties.push(PropertyMap::new());
    // alive bit deliberately not set on row 1.

    // Edge row 0: alive, label.
    let edge_label = db_string("idx.edge").unwrap();
    graph.edge_store.label.push(edge_label.clone());
    graph.edge_store.source.push(CoreNodeId::new(1));
    graph.edge_store.target.push(CoreNodeId::new(1));
    graph
        .edge_store
        .directionality
        .push(selene_core::EdgeDirectionality::Directed);
    graph.edge_store.properties.push(PropertyMap::new());
    graph.edge_store.alive_mut().insert(0);

    // Caller-supplied indexes are intentionally empty / stale.
    graph.idx_label = Default::default();
    graph.idx_edge_label = Default::default();

    let shared = SharedGraph::from_graph(graph);
    let snapshot = shared.read();
    let nodes_a = snapshot.nodes_with_label(&label_a).expect("idx.a present");
    let nodes_b = snapshot.nodes_with_label(&label_b).expect("idx.b present");
    // Only row 0 (alive) appears for label a; row 1 (dead) is filtered.
    assert_eq!(nodes_a.iter().collect::<Vec<_>>(), vec![0]);
    assert_eq!(nodes_b.iter().collect::<Vec<_>>(), vec![0]);
    let edges = snapshot
        .edges_with_label(&edge_label)
        .expect("edge label present");
    assert_eq!(edges.iter().collect::<Vec<_>>(), vec![0]);
}

#[test]
fn from_graph_rebuilds_adjacency_from_edge_store() {
    use selene_core::{LabelSet, NodeId as CoreNodeId, PropertyMap, db_string};

    let edge_label = db_string("idx.edge.adj").unwrap();
    let mut graph = SeleneGraph::new(GraphId::new(1));
    for label in ["adj.node.a", "adj.node.b"] {
        graph
            .node_store
            .labels
            .push(LabelSet::single(db_string(label).unwrap()));
        graph.node_store.properties.push(PropertyMap::new());
    }
    graph.node_store.alive_mut().insert(0);
    graph.node_store.alive_mut().insert(1);
    graph.edge_store.label.push(edge_label.clone());
    graph.edge_store.source.push(CoreNodeId::new(1));
    graph.edge_store.target.push(CoreNodeId::new(2));
    graph
        .edge_store
        .directionality
        .push(selene_core::EdgeDirectionality::Directed);
    graph.edge_store.properties.push(PropertyMap::new());
    graph.edge_store.alive_mut().insert(0);
    graph.adjacency_out = crate::id_map::engine_id_map();
    graph.adjacency_in = crate::id_map::engine_id_map();

    let shared = SharedGraph::from_graph(graph);
    let snapshot = shared.read();
    let outgoing = snapshot
        .outgoing_edges(CoreNodeId::new(1))
        .expect("outgoing adjacency rebuilt");
    let incoming = snapshot
        .incoming_edges(CoreNodeId::new(2))
        .expect("incoming adjacency rebuilt");
    assert_eq!(outgoing.len(), 1);
    assert_eq!(incoming.len(), 1);
    assert_eq!(outgoing.iter().next().unwrap().label, edge_label);
}

#[test]
fn try_from_graph_returns_ok_for_well_formed_input() {
    use selene_core::{LabelSet, PropertyMap, db_string};

    let label = db_string("idx.ok").unwrap();
    let mut graph = SeleneGraph::new(GraphId::new(1));
    graph
        .node_store
        .labels
        .push(LabelSet::single(label.clone()));
    graph.node_store.properties.push(PropertyMap::new());
    graph.node_store.alive_mut().insert(0);

    let shared =
        SharedGraph::try_from_graph(graph).expect("well-formed graph rebuilds successfully");
    let snapshot = shared.read();
    assert!(snapshot.nodes_with_label(&label).is_some());
}

#[test]
fn from_graph_discards_caller_supplied_index_drift() {
    use selene_core::{LabelSet, PropertyMap, db_string};

    let real_label = db_string("idx.real").unwrap();
    let phantom_label = db_string("idx.phantom").unwrap();

    let mut graph = SeleneGraph::new(GraphId::new(1));
    graph
        .node_store
        .labels
        .push(LabelSet::single(real_label.clone()));
    graph.node_store.properties.push(PropertyMap::new());
    graph.node_store.alive_mut().insert(0);

    // Caller injects a phantom index entry that doesn't match storage.
    let mut phantom_bitmap = roaring::RoaringBitmap::new();
    phantom_bitmap.insert(99);
    graph
        .idx_label
        .insert_cow(phantom_label.clone(), phantom_bitmap);

    let shared = SharedGraph::from_graph(graph);
    let snapshot = shared.read();
    // Phantom index is rebuilt away — only the real label remains.
    assert!(snapshot.nodes_with_label(&phantom_label).is_none());
    assert!(snapshot.nodes_with_label(&real_label).is_some());
}
