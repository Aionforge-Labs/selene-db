use super::*;
use selene_core::db_string;

#[test]
fn new_graph_is_empty() {
    let graph = SeleneGraph::new(GraphId::new(1));
    assert_eq!(graph.node_count(), 0);
    assert_eq!(graph.edge_count(), 0);
    assert_eq!(graph.label_count(), 0);
    assert_eq!(graph.edge_label_count(), 0);
    assert_eq!(graph.property_index_count(), 0);
    assert_eq!(graph.composite_property_index_count(), 0);
    assert_eq!(graph.vector_index_count(), 0);
    assert_eq!(graph.text_index_count(), 0);
    assert_eq!(graph.idx_label.len(), 0);
    assert_eq!(graph.idx_edge_label.len(), 0);
    assert!(graph.property_index.is_empty());
    assert!(graph.composite_property_index.is_empty());
    assert!(graph.vector_index.is_empty());
    assert!(graph.text_index.is_empty());
    assert_eq!(graph.meta.generation, 0);
    assert_eq!(graph.meta.next_node_id, 1);
    assert_eq!(graph.meta.next_edge_id, 1);
}

#[test]
fn read_accessors_return_none_for_unknown_ids() {
    let graph = SeleneGraph::new(GraphId::new(1));
    assert_eq!(graph.node_labels(NodeId::new(1)), None);
    assert_eq!(graph.edge_label(EdgeId::new(1)), None);
    assert_eq!(
        graph.nodes_with_label(&db_string("graph.missing").unwrap()),
        None
    );
    assert!(!graph.is_node_alive(NodeId::TOMBSTONE));
}

#[test]
fn node_labels_returns_some_for_alive_node() {
    let mut graph = SeleneGraph::new(GraphId::new(1));
    let label = db_string("graph.node").unwrap();
    graph
        .node_store
        .labels
        .push(LabelSet::single(label.clone()));
    graph.node_store.properties.push(PropertyMap::new());
    // BRIEF-Item-4a: a manually built row must also bind id <-> row, the way
    // create_node / rebuild_id_maps do — reads now resolve through the map.
    graph.node_store.row_to_id.push(NodeId::new(1));
    graph
        .node_rows
        .insert_cow(NodeId::new(1), crate::store::NodeRow::new(0));
    graph.node_store.alive_mut().insert(0);
    assert_eq!(
        graph
            .node_labels(NodeId::new(1))
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        vec![label]
    );
}

#[test]
fn label_count_reports_distinct_labels_only() {
    let mut graph = SeleneGraph::new(GraphId::new(1));
    let label = db_string("graph.same").unwrap();
    let mut bitmap = RoaringBitmap::new();
    bitmap.insert(0);
    bitmap.insert(1);
    graph.idx_label.insert_cow(label.clone(), bitmap);
    assert_eq!(graph.label_count(), 1);
    assert!(graph.nodes_with_label(&label).unwrap().contains(0));
    assert!(graph.nodes_with_label(&label).unwrap().contains(1));
}

#[test]
fn persistent_maps_share_on_clone_and_diverge_on_mutation() {
    let mut original = SeleneGraph::new(GraphId::new(1));
    let label = db_string("graph.shared").unwrap();
    let mut bitmap = RoaringBitmap::new();
    bitmap.insert(0);
    original.idx_label.insert_cow(label.clone(), bitmap);
    original
        .node_rows
        .insert_cow(NodeId::new(1), crate::store::NodeRow::new(0));

    let label_refs = original.idx_label.strong_count();
    let id_refs = original.node_rows.strong_count();
    let mut fork = original.clone();
    assert_eq!(original.idx_label.strong_count(), label_refs + 1);
    assert_eq!(original.node_rows.strong_count(), id_refs + 1);

    fork.idx_label.get_mut_cow(&label).unwrap().insert(1);
    fork.node_rows
        .insert_cow(NodeId::new(2), crate::store::NodeRow::new(1));

    assert_eq!(original.idx_label.strong_count(), label_refs);
    assert_eq!(fork.idx_label.strong_count(), 1);
    assert_eq!(original.node_rows.strong_count(), id_refs);
    assert_eq!(fork.node_rows.strong_count(), 1);
    assert!(!original.idx_label.get(&label).unwrap().contains(1));
    assert_eq!(original.node_rows.get(&NodeId::new(2)), None);
}
