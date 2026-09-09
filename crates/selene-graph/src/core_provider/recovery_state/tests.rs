use std::sync::Arc;

use selene_core::{
    Change, EdgeId, EdgeTypeDefV1, GraphId, GraphTypeId, LabelSet, NodeId, NodeTypeDefV1,
    NodeTypeRef, PredefinedValueType, PropertyDefV1, PropertyMap, SchemaChange,
    SchemaPropertyIndexKind, Value, ValueType, ValueTypeCardinality, VectorValue, db_string,
};
use selene_persist::RecoveryProvider;
use smallvec::smallvec;

use crate::core_provider::sections::{encode_graph_types, encode_meta, encode_schemas};
use crate::core_provider::{CORE_GTYP_SUB, CORE_META_SUB, CORE_SCMA_SUB, CoreProvider};
use crate::{
    EdgeEndpointDef, EdgeTypeDef, GraphError, GraphTypeDef, IndexProvider, NodeTypeDef,
    ProviderError, SharedGraph, SubTag, TypedIndexKind, ValidationMode,
};

#[path = "tests/graph_identity.rs"]
mod graph_identity;
#[path = "tests/intent.rs"]
mod intent;

fn test_graph_type_id() -> GraphTypeId {
    GraphTypeId::new(1).unwrap()
}

fn empty_runtime_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("core.recovery.graph").unwrap(),
        node_types: Vec::new(),
        edge_types: Vec::new(),
    }
}

fn person_runtime_graph_type() -> GraphTypeDef {
    let person = db_string("Person").unwrap();
    GraphTypeDef {
        name: db_string("core.recovery.person.graph").unwrap(),
        node_types: vec![NodeTypeDef {
            name: person.clone(),
            key_labels: LabelSet::single(person),
            properties: Vec::new(),
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    }
}

fn person_knows_runtime_graph_type() -> GraphTypeDef {
    let knows = db_string("KNOWS").unwrap();
    let mut graph_type = person_runtime_graph_type();
    graph_type.edge_types.push(EdgeTypeDef {
        name: knows.clone(),
        label: knows,
        source_node_type: EdgeEndpointDef::NodeType(0),
        target_node_type: EdgeEndpointDef::NodeType(0),
        properties: Vec::new(),
        validation_mode: ValidationMode::Strict,
    });
    graph_type
}

fn closed_graph_snapshot(graph_type: GraphTypeDef) -> crate::SeleneGraph {
    SharedGraph::builder(GraphId::new(1))
        .bound_to(graph_type)
        .unwrap()
        .build()
        .unwrap()
        .read()
        .as_ref()
        .clone()
}

fn load_closed_snapshot(provider: &CoreProvider, graph: &crate::SeleneGraph) {
    IndexProvider::read_section(
        provider,
        SubTag(CORE_GTYP_SUB),
        &encode_graph_types(graph).unwrap(),
    )
    .unwrap();
    IndexProvider::read_section(
        provider,
        SubTag(CORE_META_SUB),
        &encode_meta(&graph.meta, 0).unwrap(),
    )
    .unwrap();
}

fn core_string_property(name: &str, required: bool) -> PropertyDefV1 {
    PropertyDefV1 {
        name: db_string(name).unwrap(),
        value_type: ValueType {
            predefined: Some(PredefinedValueType::String),
            decimal_type: None,
            character_string_type: None,
            byte_string_type: None,
            union: None,
            list_of: None,
            record: None,
            not_null: required,
            cardinality: ValueTypeCardinality::ExactlyOne,
        },
        nullable: !required,
        default: None,
    }
}

fn props(pairs: impl IntoIterator<Item = (selene_core::DbString, Value)>) -> PropertyMap {
    PropertyMap::from_pairs(pairs).unwrap()
}

fn vector(values: &[f32]) -> Value {
    Value::Vector(VectorValue::new(values.to_vec()).unwrap())
}

#[test]
fn wal_replay_applies_node_type_added_to_graph_type() {
    let base = empty_runtime_graph_type();
    let snapshot = closed_graph_snapshot(base.clone());
    let provider = CoreProvider::new_for_recovery();
    load_closed_snapshot(provider.as_ref(), &snapshot);
    let sensor = db_string("Sensor").unwrap();

    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::NodeTypeAdded {
                graph_type: test_graph_type_id(),
                label: sensor.clone(),
                def: NodeTypeDefV1 {
                    labels: LabelSet::single(sensor.clone()),
                    properties: smallvec![core_string_property("serial", true)],
                    key: None,
                },
            },
        },
    )
    .unwrap();

    let recovered = provider
        .finish_recovery(GraphId::new(1), Some(Arc::new(base)))
        .unwrap();
    let graph_type = recovered.meta.bound_type.as_ref().unwrap();
    assert_eq!(graph_type.node_types[0].name, sensor);
    assert_eq!(
        graph_type.node_types[0].properties[0].name.as_str(),
        "serial"
    );
    assert!(graph_type.node_types[0].properties[0].required);
}

#[test]
fn wal_replay_applies_edge_type_added() {
    let base = person_runtime_graph_type();
    let snapshot = closed_graph_snapshot(base.clone());
    let provider = CoreProvider::new_for_recovery();
    load_closed_snapshot(provider.as_ref(), &snapshot);
    let knows = db_string("KNOWS").unwrap();
    let since = core_string_property("since", false);

    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::EdgeTypeAdded {
                graph_type: test_graph_type_id(),
                label: knows.clone(),
                def: EdgeTypeDefV1 {
                    label: knows.clone(),
                    source_node_type: NodeTypeRef(db_string("Person").unwrap()),
                    target_node_type: NodeTypeRef(db_string("Person").unwrap()),
                    properties: smallvec![since],
                },
            },
        },
    )
    .unwrap();

    let recovered = provider
        .finish_recovery(GraphId::new(1), Some(Arc::new(base)))
        .unwrap();
    let edge_type = &recovered.meta.bound_type.as_ref().unwrap().edge_types[0];
    assert_eq!(edge_type.name, knows);
    assert_eq!(edge_type.label, knows);
    assert_eq!(edge_type.source_node_type, EdgeEndpointDef::NodeType(0));
    assert_eq!(edge_type.target_node_type, EdgeEndpointDef::NodeType(0));
    assert_eq!(edge_type.properties[0].name.as_str(), "since");
}

#[test]
fn wal_replay_applies_node_type_dropped() {
    let base = person_runtime_graph_type();
    let snapshot = closed_graph_snapshot(base.clone());
    let provider = CoreProvider::new_for_recovery();
    load_closed_snapshot(provider.as_ref(), &snapshot);

    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::NodeTypeDropped {
                graph_type: test_graph_type_id(),
                name: db_string("Person").unwrap(),
            },
        },
    )
    .unwrap();

    let recovered = provider
        .finish_recovery(GraphId::new(1), Some(Arc::new(base)))
        .unwrap();
    assert!(
        recovered
            .meta
            .bound_type
            .as_ref()
            .unwrap()
            .node_types
            .is_empty()
    );
}

#[test]
fn wal_replay_applies_edge_type_dropped() {
    let base = person_knows_runtime_graph_type();
    let snapshot = closed_graph_snapshot(base.clone());
    let provider = CoreProvider::new_for_recovery();
    load_closed_snapshot(provider.as_ref(), &snapshot);

    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::EdgeTypeDropped {
                graph_type: test_graph_type_id(),
                name: db_string("KNOWS").unwrap(),
            },
        },
    )
    .unwrap();

    let recovered = provider
        .finish_recovery(GraphId::new(1), Some(Arc::new(base)))
        .unwrap();
    assert!(
        recovered
            .meta
            .bound_type
            .as_ref()
            .unwrap()
            .edge_types
            .is_empty()
    );
}

#[test]
fn wal_replay_node_type_added_against_open_snapshot_returns_inconsistent() {
    let provider = CoreProvider::new_for_recovery();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::NodeTypeAdded {
                graph_type: test_graph_type_id(),
                label: db_string("Sensor").unwrap(),
                def: NodeTypeDefV1::new(LabelSet::single(db_string("Sensor").unwrap())),
            },
        },
    )
    .unwrap();

    let err = provider
        .finish_recovery(GraphId::new(1), None)
        .expect_err("open graph cannot replay catalog DDL");

    assert!(matches!(
        err,
        GraphError::Provider(ProviderError::Inconsistent { reason })
            if reason.contains("WAL NodeTypeAdded references missing graph type index 0")
    ));
}

#[test]
fn wal_replay_restores_property_index_created_after_node_state() {
    let provider = CoreProvider::new_for_recovery();
    let label = db_string("RecoveredPerson").unwrap();
    let property = db_string("age").unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(label.clone()),
            properties: props([(property.clone(), Value::Int(42))]),
        },
    )
    .unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::PropertyIndexCreated {
                label: label.clone(),
                property: property.clone(),
                kind: SchemaPropertyIndexKind::I64,
            },
        },
    )
    .unwrap();

    // `finish_recovery` (into_graph) now produces only the index *registration*;
    // the bitmap contents are rebuilt by `SharedGraph::try_from_graph`'s single
    // rebuild pass (GRAPH-06). Drive the recovered graph through that public path,
    // then assert the registered index actually indexes the recovered content.
    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert_eq!(recovered.property_index_count(), 1);
    let shared = SharedGraph::try_from_graph(recovered).unwrap();
    let read = shared.read();
    let rows = read
        .nodes_with_property_eq(&label, &property, &Value::Int(42))
        .unwrap();
    assert_eq!(rows.iter().collect::<Vec<_>>(), vec![0]);
}

#[test]
fn wal_replay_restores_edge_property_index_created_after_edge_state() {
    let provider = CoreProvider::new_for_recovery();
    let edge_label = db_string("RecoveredEdge").unwrap();
    let property = db_string("port").unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::new(),
            properties: PropertyMap::new(),
        },
    )
    .unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(2),
            labels: LabelSet::new(),
            properties: PropertyMap::new(),
        },
    )
    .unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::EdgeCreated {
            id: EdgeId::new(1),
            directionality: selene_core::EdgeDirectionality::Directed,
            label: edge_label.clone(),
            source: NodeId::new(1),
            target: NodeId::new(2),
            properties: props([(property.clone(), Value::Int(42))]),
        },
    )
    .unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::EdgePropertyIndexCreated {
                label: edge_label.clone(),
                property: property.clone(),
                kind: SchemaPropertyIndexKind::I64,
                name: None,
            },
        },
    )
    .unwrap();

    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert_eq!(recovered.edge_property_index_count(), 1);
    let shared = SharedGraph::try_from_graph(recovered).unwrap();
    let read = shared.read();
    let rows = read
        .edges_with_property_eq(&edge_label, &property, &Value::Int(42))
        .unwrap();
    assert_eq!(rows.iter().collect::<Vec<_>>(), vec![0]);
}

#[test]
fn wal_replay_delete_clears_dead_node_payload() {
    let provider = CoreProvider::new_for_recovery();
    let id = NodeId::new(1);
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id,
            labels: LabelSet::single(db_string("WalPayloadNode").unwrap()),
            properties: props([(db_string("embedding").unwrap(), vector(&[1.0, 2.0, 3.0]))]),
        },
    )
    .unwrap();
    RecoveryProvider::on_change(provider.as_ref(), &Change::NodeDeleted { id }).unwrap();

    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    let row = recovered
        .node_row_for_id(id)
        .expect("deleted id stays mapped")
        .index();
    assert!(!recovered.is_node_alive(id));
    assert_eq!(recovered.node_store.row_to_id.get(row).copied(), Some(id));
    assert!(recovered.node_store.labels.get(row).unwrap().is_empty());
    assert!(recovered.node_store.properties.get(row).unwrap().is_empty());
}

#[test]
fn wal_replay_delete_clears_dead_edge_payload() {
    let provider = CoreProvider::new_for_recovery();
    let source = NodeId::new(1);
    let target = NodeId::new(2);
    let edge = EdgeId::new(1);
    for id in [source, target] {
        RecoveryProvider::on_change(
            provider.as_ref(),
            &Change::NodeCreated {
                id,
                labels: LabelSet::new(),
                properties: PropertyMap::new(),
            },
        )
        .unwrap();
    }
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::EdgeCreated {
            id: edge,
            directionality: selene_core::EdgeDirectionality::Directed,
            label: db_string("WalPayloadEdge").unwrap(),
            source,
            target,
            properties: props([(db_string("embedding").unwrap(), vector(&[4.0, 5.0, 6.0]))]),
        },
    )
    .unwrap();
    RecoveryProvider::on_change(provider.as_ref(), &Change::EdgeDeleted { id: edge }).unwrap();

    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    let row = recovered
        .edge_row_for_id(edge)
        .expect("deleted id stays mapped")
        .index();
    assert!(recovered.is_node_alive(source));
    assert!(recovered.is_node_alive(target));
    assert!(!recovered.is_edge_alive(edge));
    assert_eq!(recovered.edge_store.row_to_id.get(row).copied(), Some(edge));
    assert_eq!(recovered.edge_store.label.get(row).unwrap().as_str(), "");
    assert_eq!(
        recovered.edge_store.source.get(row).copied(),
        Some(NodeId::TOMBSTONE)
    );
    assert_eq!(
        recovered.edge_store.target.get(row).copied(),
        Some(NodeId::TOMBSTONE)
    );
    assert!(recovered.edge_store.properties.get(row).unwrap().is_empty());
}

#[test]
fn wal_replay_restores_named_property_index_metadata() {
    let provider = CoreProvider::new_for_recovery();
    let label = db_string("NamedWalPerson").unwrap();
    let property = db_string("name").unwrap();
    let name = db_string("named_wal_person_name_idx").unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::PropertyIndexCreatedNamed {
                label: label.clone(),
                property: property.clone(),
                kind: SchemaPropertyIndexKind::String,
                name: Some(name.clone()),
            },
        },
    )
    .unwrap();

    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    let entries = recovered.iter_property_index_entries().collect::<Vec<_>>();
    assert_eq!(
        entries,
        vec![(label, property, TypedIndexKind::String, Some(name))]
    );
}

#[test]
fn wal_replay_drops_property_index_registered_in_snapshot_scma() {
    let shared = SharedGraph::new(GraphId::new(1));
    let label = db_string("SnapshotPerson").unwrap();
    let property = db_string("age").unwrap();
    shared
        .create_property_index(label.clone(), property.clone(), TypedIndexKind::I64)
        .unwrap();
    let snapshot = shared.read();
    let provider = CoreProvider::new_for_recovery();
    IndexProvider::read_section(
        provider.as_ref(),
        SubTag(CORE_SCMA_SUB),
        &encode_schemas(&snapshot).unwrap(),
    )
    .unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::PropertyIndexDropped {
                label: label.clone(),
                property: property.clone(),
            },
        },
    )
    .unwrap();

    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert_eq!(recovered.property_index_count(), 0);
    assert!(recovered.property_index_for(&label, &property).is_none());
}

#[test]
fn wal_replay_property_index_create_drop_create_sequence_uses_last_event() {
    let provider = CoreProvider::new_for_recovery();
    let label = db_string("SequencePerson").unwrap();
    let property = db_string("age").unwrap();
    for change in [
        SchemaChange::PropertyIndexCreated {
            label: label.clone(),
            property: property.clone(),
            kind: SchemaPropertyIndexKind::I64,
        },
        SchemaChange::PropertyIndexDropped {
            label: label.clone(),
            property: property.clone(),
        },
        SchemaChange::PropertyIndexCreated {
            label: label.clone(),
            property: property.clone(),
            kind: SchemaPropertyIndexKind::I64,
        },
    ] {
        RecoveryProvider::on_change(
            provider.as_ref(),
            &Change::SchemaChanged {
                graph: GraphId::new(1),
                change,
            },
        )
        .unwrap();
    }

    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert!(recovered.property_index_for(&label, &property).is_some());
    assert_eq!(recovered.property_index_count(), 1);
}

#[test]
fn wal_replay_applies_catalog_ddl_before_property_index_queue() {
    let base = empty_runtime_graph_type();
    let snapshot = closed_graph_snapshot(base.clone());
    let provider = CoreProvider::new_for_recovery();
    load_closed_snapshot(provider.as_ref(), &snapshot);
    let label = db_string("IndexedSensor").unwrap();
    let property = db_string("reading").unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::NodeTypeAdded {
                graph_type: test_graph_type_id(),
                label: label.clone(),
                def: NodeTypeDefV1::new(LabelSet::single(label.clone())),
            },
        },
    )
    .unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::PropertyIndexCreated {
                label: label.clone(),
                property: property.clone(),
                kind: SchemaPropertyIndexKind::I64,
            },
        },
    )
    .unwrap();

    let recovered = provider
        .finish_recovery(GraphId::new(1), Some(Arc::new(base)))
        .unwrap();
    assert_eq!(
        recovered.meta.bound_type.as_ref().unwrap().node_types[0].name,
        label
    );
    assert!(recovered.property_index_for(&label, &property).is_some());
}

#[test]
fn wal_replay_property_index_create_is_lenient_for_later_kind_drift() {
    let provider = CoreProvider::new_for_recovery();
    let label = db_string("DriftPerson").unwrap();
    let property = db_string("age").unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::PropertyIndexCreated {
                label: label.clone(),
                property: property.clone(),
                kind: SchemaPropertyIndexKind::I64,
            },
        },
    )
    .unwrap();
    RecoveryProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(label.clone()),
            properties: props([(
                property.clone(),
                Value::String(db_string("not-an-int").unwrap()),
            )]),
        },
    )
    .unwrap();

    // The lenient index rebuild now runs in `SharedGraph::try_from_graph`
    // (GRAPH-06): a String value under an I64-declared index is skipped, not
    // fatal. Route through the rebuild so the lenient path is actually exercised
    // (asserting on the bare `finish_recovery` placeholder would falsely pass).
    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert!(recovered.property_index_for(&label, &property).is_some());
    let shared = SharedGraph::try_from_graph(recovered).unwrap();
    let read = shared.read();
    assert!(
        read.property_index
            .contains_key(&(label.clone(), property.clone())),
        "the registration survives replay even though the index cannot answer"
    );
    assert!(
        read.property_index_for(&label, &property).is_none(),
        "the accessor declines in step with the probes, so the planner does not \
         cost a plan against an index whose probes will refuse it"
    );

    // The rebuild must carry its skip count into the entry. Without that a
    // recovered graph would answer this probe while the live graph it replaced
    // declined, so the same query would return different rows either side of a
    // restart — worse than the drift itself.
    let entry = read
        .property_index
        .get(&(label.clone(), property.clone()))
        .expect("registration survives recovery");
    assert_eq!(
        entry.drifted_rows, 1,
        "replay skipped one row it could not key and must say so"
    );
    assert!(
        read.nodes_with_property_eq(&label, &property, &Value::Int(42))
            .is_none(),
        "an index missing a live row declines, so the caller scans"
    );
    assert!(
        entry
            .lookup_eq_ignoring_drift(&Value::Int(42))
            .is_some_and(|rows| rows.is_empty()),
        "the index itself is intact and simply holds no I64 rows"
    );
}
