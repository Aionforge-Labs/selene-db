use super::*;
use crate::{GraphTypeDef, NodeTypeDef, PropertyDefaultValue, PropertyTypeDef, ValidationMode};
use selene_catalog::{
    ConstraintDeclaration, ConstraintId, ConstraintKind, DeclarationMetadata, DeclarationState,
    ElementKind, IndexConfiguration, IndexDeclaration, IndexId, PropertyTarget,
};
use selene_core::{PropertyDiff, PropertyValueType, SchemaPropertyIndexKind};

fn property(name: &str, kind: PropertyValueType, value: Option<Value>) -> PropertyTypeDef {
    PropertyTypeDef {
        name: db_string(name).unwrap(),
        value_type: kind,
        list_element_type: None,
        required: false,
        default: value.as_ref().and_then(PropertyDefaultValue::from_value),
        immutable: false,
        unique: name == "v",
        decimal_type: None,
        character_string_type: None,
        byte_string_type: None,
        record_field_types: None,
    }
}
fn constrained(seed: &ReplayState) -> LogicalTransaction {
    let mut tx = transaction(seed);
    let mut labels = LabelSet::new();
    labels.insert(db_string("L").unwrap());
    let definition = super::super::definition(&GraphTypeDef {
        name: db_string("Bound").unwrap(),
        node_types: vec![NodeTypeDef {
            name: db_string("Thing").unwrap(),
            key_labels: labels.clone(),
            properties: vec![property("v", PropertyValueType::Int, None)],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: vec![crate::EdgeTypeDef {
            name: db_string("LinkType").unwrap(),
            label: db_string("LINK").unwrap(),
            source_node_type: crate::EdgeEndpointDef::NodeType(0),
            target_node_type: crate::EdgeEndpointDef::NodeType(0),
            properties: vec![],
            validation_mode: ValidationMode::Strict,
        }],
    })
    .unwrap();
    let next = tx.catalog.apply(&seed.catalog).unwrap();
    let mut descriptors = next.descriptors().to_vec();
    for (index, graph) in tx.graphs.iter_mut().enumerate() {
        graph.definition = Some(definition.clone());
        graph.backing_indexes = vec![index as u64 + 1];
        for change in &mut graph.changes {
            if let Change::NodeCreated { labels: target, .. } = change {
                *target = labels.clone();
            }
        }
        let owner = CatalogParent::Graph(selene_catalog::GraphId::new(graph.id.get()).unwrap());
        let target = PropertyTarget {
            element: ElementKind::Node,
            label: "L".into(),
            properties: vec!["v".into()],
        };
        descriptors.push(
            CatalogDescriptor::index(
                IndexId::new(index as u64 + 1).unwrap(),
                CatalogName::regular("by_v").unwrap(),
                owner,
                generation(2),
                CreationMetadata::new(generation(2), None),
                IndexDeclaration {
                    metadata: DeclarationMetadata::new(DeclarationState::Ready),
                    target: target.clone(),
                    configuration: IndexConfiguration::Property(vec![SchemaPropertyIndexKind::I64]),
                },
            )
            .unwrap(),
        );
        descriptors.push(
            CatalogDescriptor::constraint(
                ConstraintId::new(index as u64 + 1).unwrap(),
                CatalogName::regular("unique_v").unwrap(),
                owner,
                generation(2),
                CreationMetadata::new(generation(2), None),
                ConstraintDeclaration {
                    metadata: DeclarationMetadata::new(DeclarationState::Ready),
                    target,
                    declaring_type: "Thing".into(),
                    kind: ConstraintKind::Unique,
                    backing_index: None,
                },
            )
            .unwrap(),
        );
    }
    let mut water = next.high_water().clone();
    water.insert(selene_catalog::CatalogObjectKind::Index, 2);
    water.insert(selene_catalog::CatalogObjectKind::Constraint, 2);
    let records = CatalogLogicalRecords::new(generation(2), water, descriptors).unwrap();
    tx.catalog = CatalogDelta::between(&seed.catalog.reconstruct().unwrap(), &records).unwrap();
    tx
}

#[test]
fn owning_schema_unique_and_backing_validators_run_before_returning_candidate() {
    let seed = seed();
    let tx = constrained(&seed);
    let state = apply(&seed, &tx).unwrap();
    assert!(
        state.graphs[&GraphId::new(1)].property_index.is_empty(),
        "isolated validation must not activate/rebuild optional indexes"
    );
    assert_eq!(&*state.backing_indexes[&GraphId::new(1)], &[1]);
    assert_eq!(state.graph_summary(GraphId::new(2)), Some((2, 1, 3, 2)));
    let mut bad = tx.clone();
    bad.graphs[1].changes.push(Change::NodeUpdated {
        id: NodeId::new(2),
        labels_diff: selene_core::LabelDiff::new([], []).unwrap(),
        properties_diff: PropertyDiff::new([(db_string("v").unwrap(), Value::Int(1))], []).unwrap(),
    });
    assert!(
        apply(&seed, &bad).is_err(),
        "invalid LAST update must trigger whole-state UNIQUE"
    );
    let mut bad = tx.clone();
    bad.graphs[1].backing_indexes.clear();
    assert!(
        apply(&seed, &bad).is_err(),
        "Ready without actual backing is not activation"
    );
    let mut bad = tx.clone();
    bad.graphs[1].backing_indexes = vec![1];
    assert!(apply(&seed, &bad).is_err(), "wrong graph backing identity");
    let mut bad = tx.clone();
    if let Change::NodeCreated { properties, .. } = &mut bad.graphs[1].changes[1] {
        properties
            .set(
                db_string("v").unwrap(),
                Value::String(db_string("wrong").unwrap()),
            )
            .unwrap();
    }
    assert!(
        apply(&seed, &bad).is_err(),
        "owning structural assignment validator"
    );
    assert_eq!(seed.catalog.descriptors().len(), 2);
    assert!(seed.graphs.is_empty());
}

#[test]
fn recursive_defaults_and_semantic_names_survive_schema_codec() {
    let record = Value::Record(Box::new(selene_core::Record::Open(smallvec::smallvec![(
        db_string("named").unwrap(),
        Value::List(vec![
            Value::Vector(selene_core::VectorValue::new(vec![0.25, 0.75]).unwrap()),
            Value::Null
        ])
    ),])));
    let mut list = property(
        "list",
        PropertyValueType::List,
        Some(Value::List(vec![Value::Int(1), Value::Null])),
    );
    list.list_element_type = Some(crate::PropertyElementType::Scalar(PropertyValueType::Int));
    let mut labels = LabelSet::new();
    labels.insert(db_string("LabelNotTypeName").unwrap());
    let graph_type = GraphTypeDef {
        name: db_string("SchemaName").unwrap(),
        node_types: vec![NodeTypeDef {
            name: db_string("TypeNotLabel").unwrap(),
            key_labels: labels,
            validation_mode: ValidationMode::Strict,
            properties: vec![
                property("record", PropertyValueType::RecordTyped, Some(record)),
                list,
                property(
                    "json",
                    PropertyValueType::Json,
                    Some(Value::Json(
                        selene_core::JsonValue::parse_str("{\"a\":1}").unwrap(),
                    )),
                ),
                property(
                    "u128",
                    PropertyValueType::Uint128,
                    Some(Value::Uint128(u128::MAX)),
                ),
            ],
        }],
        edge_types: vec![],
    };
    let definition = super::super::definition(&graph_type).unwrap();
    let mut e = Encoder::new(Limits::default()).unwrap();
    e.graph_definition(&definition).unwrap();
    let bytes = e.finish();
    let mut budget = Budget::new(Limits::default()).unwrap();
    let mut d = Decoder::new(&bytes, &mut budget).unwrap();
    let decoded = d.graph_definition().unwrap();
    d.finish().unwrap();
    let runtime = super::super::schema::materialize(&decoded).unwrap();
    assert_eq!(runtime, graph_type);
}

#[test]
fn schema_reference_names_reject_missing_and_forward_catalog_kind() {
    let seed = seed();
    let mut tx = constrained(&seed);
    tx.graphs[1].definition.as_mut().unwrap().edges[0]
        .1
        .source_node_type = selene_core::EdgeEndpointDef::NodeType(selene_core::NodeTypeRef(
        db_string("absent").unwrap(),
    ));
    assert!(apply(&seed, &tx).is_err());
    let mut tx = transaction(&seed);
    tx.graph_types.push(TypeDelta {
        id: GraphTypeId::new(1).unwrap(),
        definition: Some(GraphDefinition {
            name: db_string("no_catalog_type").unwrap(),
            nodes: vec![],
            edges: vec![],
        }),
    });
    assert!(apply(&seed, &tx).is_err());
}

#[test]
fn replace_and_drop_registration_revisions_are_atomic_and_never_reuse_ids() {
    let seed = seed();
    let tx = constrained(&seed);
    let state = apply(&seed, &tx).unwrap();
    let id = CatalogObjectId::Index(IndexId::new(1).unwrap());
    let old = state.catalog.reconstruct().unwrap();
    let original = old.descriptor(id).unwrap();
    let CatalogPayload::Index(mut index) = original.payload().clone() else {
        panic!("index")
    };
    index.metadata.state = DeclarationState::Inactive;
    let replacement = CatalogDescriptor::new(
        id,
        id.kind(),
        CatalogName::regular("renamed").unwrap(),
        original.parent(),
        generation(3),
        original.creation().clone(),
        CatalogPayload::Index(index),
    )
    .unwrap();
    let descriptors = old
        .descriptors()
        .map(|d| {
            if d.id() == id {
                replacement.clone()
            } else {
                d.clone()
            }
        })
        .collect();
    let next = CatalogLogicalRecords::new(
        generation(3),
        state.catalog.high_water().clone(),
        descriptors,
    )
    .unwrap();
    let mut delta = followup(&state, vec![]);
    delta.catalog = CatalogDelta::between(&old, &next).unwrap();
    delta.graphs[0].definition = tx.graphs[0].definition.clone();
    delta.graphs[0].backing_indexes = vec![1];
    let renamed = apply(&state, &delta).unwrap();
    assert_eq!(
        renamed
            .catalog
            .reconstruct()
            .unwrap()
            .descriptor(id)
            .unwrap()
            .name()
            .display(),
        "renamed"
    );
    let mut wrong = delta.clone();
    if let CatalogLogicalChange::Replaced { previous, .. } = &mut wrong.catalog.changes[0] {
        *previous = generation(1);
    }
    assert!(apply(&state, &wrong).is_err());
    assert_eq!(
        state
            .catalog
            .reconstruct()
            .unwrap()
            .descriptor(id)
            .unwrap()
            .name()
            .display(),
        "by_v"
    );
    let mut drop = followup(&renamed, vec![]);
    drop.graphs[0].definition = tx.graphs[0].definition.clone();
    drop.catalog.generation = generation(4);
    drop.catalog.changes = vec![CatalogLogicalChange::Dropped {
        id,
        generation: generation(3),
    }];
    let dropped = apply(&renamed, &drop).unwrap();
    assert!(
        dropped
            .catalog
            .reconstruct()
            .unwrap()
            .descriptor(id)
            .is_none()
    );
    assert_eq!(
        dropped.catalog.high_water()[&selene_catalog::CatalogObjectKind::Index],
        2
    );
}

#[test]
fn named_graph_type_payload_and_catalog_identity_are_paired() {
    let seed = seed();
    let mut tx = transaction(&seed);
    let next = tx.catalog.apply(&seed.catalog).unwrap();
    let mut descriptors = next.descriptors().to_vec();
    let id = GraphTypeId::new(1).unwrap();
    descriptors.push(
        CatalogDescriptor::graph_type(
            id,
            CatalogName::regular("Blueprint").unwrap(),
            SchemaId::new(1).unwrap(),
            generation(2),
            CreationMetadata::new(generation(2), None),
        )
        .unwrap(),
    );
    let mut water = next.high_water().clone();
    water.insert(selene_catalog::CatalogObjectKind::GraphType, 1);
    let next = CatalogLogicalRecords::new(generation(2), water, descriptors).unwrap();
    tx.catalog = CatalogDelta::between(&seed.catalog.reconstruct().unwrap(), &next).unwrap();
    tx.graph_types = vec![TypeDelta {
        id,
        definition: Some(GraphDefinition {
            name: db_string("Blueprint").unwrap(),
            nodes: vec![],
            edges: vec![],
        }),
    }];
    let state = apply(&seed, &tx).unwrap();
    assert!(state.graph_types.contains_key(&id));
    tx.graph_types.clear();
    assert!(
        apply(&seed, &tx).is_err(),
        "descriptor marker alone is incomplete"
    );
}

#[test]
fn native_producer_refuses_to_silently_drop_unbound_registrations() {
    let mut graph = SeleneGraph::new(GraphId::new(1));
    graph.property_index.insert(
        (db_string("L").unwrap(), db_string("v").unwrap()),
        crate::PropertyIndexEntry::new(crate::TypedIndex::new(crate::TypedIndexKind::I64), None),
    );
    assert_eq!(
        super::super::graph_delta(None, &graph, &[]).unwrap_err(),
        E::Invalid("unbound index registrations require catalog metadata")
    );
}
