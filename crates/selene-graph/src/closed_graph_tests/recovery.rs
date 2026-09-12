use super::*;

#[test]
fn recover_round_trips_bound_graph_type_and_rearms_validator() {
    let graph_type = person_graph_type();
    let shared = SharedGraph::builder(GraphId::new(5))
        .bound_to(graph_type.clone())
        .unwrap()
        .build()
        .unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let alice = mutator
            .create_node(
                LabelSet::single(db_string("Person")),
                prop("name", Value::String(db_string("Alice"))),
            )
            .unwrap();
        let bob = mutator
            .create_node(
                LabelSet::single(db_string("Person")),
                prop("name", Value::String(db_string("Bob"))),
            )
            .unwrap();
        mutator
            .create_edge(
                db_string("KNOWS"),
                alice,
                bob,
                prop("since", Value::Int(2026)),
            )
            .unwrap();
    }
    txn.commit().unwrap();
    let original = shared.read();
    let delta = selene_core::logical::GraphDelta {
        id: GraphId::new(5),
        previous: Some(original.meta.generation),
        generation: original.meta.generation + 1,
        next_node_id: 4,
        next_edge_id: 2,
        definition: None,
        backing_indexes: vec![],
        changes: vec![Change::NodeCreated {
            id: NodeId::new(3),
            labels: LabelSet::single(db_string("Person")),
            properties: prop("name", Value::String(db_string("Carol"))),
        }],
    };
    let graph = crate::logical_transaction::graph_apply::logical_graph(
        Some(&original),
        &delta,
        Some(std::sync::Arc::new(graph_type.clone())),
        &mut selene_core::logical::Budget::new(Default::default()).unwrap(),
    )
    .unwrap();
    let recovered = SharedGraph::try_from_graph(graph).unwrap();
    assert!(recovered.is_closed());
    assert_eq!(recovered.graph_type().as_deref(), Some(&graph_type));
    assert!(recovered.read().is_node_alive(NodeId::new(3)));

    let mut txn = recovered.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_edge(
                db_string("KNOWS"),
                NodeId::new(1),
                NodeId::new(2),
                prop("since", Value::String(db_string("bad"))),
            )
            .unwrap();
    }
    assert!(matches!(
        txn.commit().unwrap_err(),
        GraphError::TypeViolation(TypeViolation::PropertyTypeMismatch {
            entity_id,
            property,
            expected: PropertyValueType::Int,
            observed: "String",
        }) if entity_id == EntityId::Edge(EdgeId::new(2)) && property == db_string("since")
    ));
}

#[test]
fn native_logical_creation_preserves_bound_type() {
    // F2 regression: WAL-only recovery (no snapshot) must accept the
    // caller's bound_type rather than silently defaulting to None.
    // Without this, a closed-graph crash before the first snapshot would
    // permanently downgrade to open and skip GG02 validation forever after.
    let graph_type = person_graph_type();
    let delta = selene_core::logical::GraphDelta {
        id: GraphId::new(14),
        previous: None,
        generation: 1,
        next_node_id: 2,
        next_edge_id: 1,
        definition: None,
        backing_indexes: vec![],
        changes: vec![Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(db_string("Person")),
            properties: prop("name", Value::String(db_string("Alice"))),
        }],
    };
    let graph = crate::logical_transaction::graph_apply::logical_graph(
        None,
        &delta,
        Some(std::sync::Arc::new(graph_type.clone())),
        &mut selene_core::logical::Budget::new(Default::default()).unwrap(),
    )
    .unwrap();
    let recovered = SharedGraph::try_from_graph(graph).unwrap();
    assert!(recovered.is_closed());
    assert_eq!(recovered.graph_type().as_deref(), Some(&graph_type));
    assert!(recovered.read().is_node_alive(NodeId::new(1)));
}

#[test]
fn native_logical_creation_rejects_bound_type_data_disagreement() {
    // F2 regression (drift case): if the snapshot's META declares one
    // bound_type and the caller asserts a different one, recovery must fail
    // rather than silently picking either side.
    let delta = selene_core::logical::GraphDelta {
        id: GraphId::new(15),
        previous: None,
        generation: 1,
        next_node_id: 2,
        next_edge_id: 1,
        definition: None,
        backing_indexes: vec![],
        changes: vec![Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(db_string("Person")),
            properties: PropertyMap::new(),
        }],
    };
    assert!(
        crate::logical_transaction::graph_apply::logical_graph(
            None,
            &delta,
            Some(std::sync::Arc::new(person_graph_type())),
            &mut selene_core::logical::Budget::new(Default::default()).unwrap()
        )
        .is_err()
    );
}
