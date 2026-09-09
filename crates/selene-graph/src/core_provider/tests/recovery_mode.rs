use super::*;

#[test]
fn recovery_mode_read_section_populates_state() {
    let graph = graph_with_node();
    let bytes = encode_nodes(&graph).unwrap();
    let provider = CoreProvider::new_for_recovery();
    IndexProvider::read_section(provider.as_ref(), SubTag(CORE_NODE_SUB), &bytes).unwrap();
    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert_eq!(recovered.node_count(), 1);
    assert!(recovered.is_node_alive(NodeId::new(1)));
}

#[test]
fn recovery_mode_on_change_applies_node_created() {
    let provider = CoreProvider::new_for_recovery();
    IndexProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(db_string("core.created").unwrap()),
            properties: prop("core.created.prop", Value::Int(1)),
        },
    )
    .unwrap();
    let graph = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert_eq!(graph.node_count(), 1);
    assert!(graph.is_node_alive(NodeId::new(1)));
}

#[test]
fn recovery_mode_on_change_applies_each_change_variant() {
    let add_label = db_string("core.added").unwrap();
    let base_label = db_string("core.base").unwrap();
    let prop_key = db_string("core.k").unwrap();
    let provider = CoreProvider::new_for_recovery();

    IndexProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(base_label.clone()),
            properties: PropertyMap::new(),
        },
    )
    .unwrap();
    IndexProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(2),
            labels: LabelSet::single(base_label),
            properties: PropertyMap::new(),
        },
    )
    .unwrap();
    IndexProvider::on_change(
        provider.as_ref(),
        &Change::EdgeCreated {
            directionality: selene_core::EdgeDirectionality::Directed,
            id: EdgeId::new(1),
            label: db_string("core.connects").unwrap(),
            source: NodeId::new(1),
            target: NodeId::new(2),
            properties: PropertyMap::new(),
        },
    )
    .unwrap();
    IndexProvider::on_change(
        provider.as_ref(),
        &Change::NodeUpdated {
            id: NodeId::new(1),
            labels_diff: LabelDiff::new([add_label], []).unwrap(),
            properties_diff: PropertyDiff::new([(prop_key.clone(), Value::Int(42))], []).unwrap(),
        },
    )
    .unwrap();
    IndexProvider::on_change(
        provider.as_ref(),
        &Change::EdgeUpdated {
            id: EdgeId::new(1),
            properties_diff: PropertyDiff::new([(prop_key, Value::Int(7))], []).unwrap(),
        },
    )
    .unwrap();
    IndexProvider::on_change(
        provider.as_ref(),
        &Change::EdgeDeleted { id: EdgeId::new(1) },
    )
    .unwrap();
    IndexProvider::on_change(
        provider.as_ref(),
        &Change::NodeDeleted { id: NodeId::new(1) },
    )
    .unwrap();

    let graph = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert!(!graph.is_node_alive(NodeId::new(1)));
    assert!(graph.is_node_alive(NodeId::new(2)));
    assert!(!graph.is_edge_alive(EdgeId::new(1)));
    assert_eq!(graph.node_store.len(), 2);
    assert_eq!(graph.edge_store.len(), 1);
}

#[test]
fn recovery_mode_write_section_returns_inconsistent_error() {
    let provider = CoreProvider::new_for_recovery();
    assert!(matches!(
        IndexProvider::write_section(provider.as_ref(), SubTag(CORE_NODE_SUB)),
        Err(ProviderError::Inconsistent { reason })
            if reason.contains("write_section called on recovery-mode")
    ));
}

#[test]
fn recovery_provider_read_section_round_trips_via_typed_path() {
    let graph = graph_with_node();
    let bytes = encode_nodes(&graph).unwrap();
    let provider = CoreProvider::new_for_recovery();
    RecoveryProvider::read_section(provider.as_ref(), CORE_NODE_SUB, &bytes).unwrap();
    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert!(recovered.is_node_alive(NodeId::new(1)));
}

#[test]
fn recovery_provider_on_change_calls_typed_path() {
    let provider = CoreProvider::new_for_recovery();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(db_string("core.raw").unwrap()),
            properties: PropertyMap::new(),
        },
    )
    .unwrap();
    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert!(recovered.is_node_alive(NodeId::new(1)));
}
