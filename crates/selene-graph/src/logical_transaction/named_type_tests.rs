//! Named catalog type authority must not be replaced by an instance's own claim.

use super::*;
use selene_core::{LabelDiff, PredefinedValueType, PropertyDef, PropertyDiff, ValueType};
use selene_persist::{
    control::{StoreEpoch, StoreId},
    logical_frame::{self as frame, Boundary, Compression, Context},
};

fn required(name: &str, immutable: bool) -> PropertyDef {
    let mut value_type = ValueType::predefined(PredefinedValueType::Int);
    value_type.not_null = true;
    PropertyDef {
        name: db_string(name).unwrap(),
        value_type,
        nullable: false,
        default: None,
        immutable,
        unique: false,
        record_fields: None,
    }
}

fn named_transaction(seed: &ReplayState) -> LogicalTransaction {
    let mut tx = transaction(seed);
    let mut node_type =
        selene_core::NodeTypeDef::new(LabelSet::from_iter([db_string("L").unwrap()]));
    node_type.properties.push(required("v", true));
    let definition = GraphDefinition {
        name: db_string("Blueprint").unwrap(),
        nodes: vec![(db_string("Thing").unwrap(), node_type)],
        edges: vec![(
            db_string("LinkType").unwrap(),
            selene_core::EdgeTypeDef::new(
                db_string("LINK").unwrap(),
                selene_core::NodeTypeRef(db_string("Thing").unwrap()),
                selene_core::NodeTypeRef(db_string("Thing").unwrap()),
            ),
        )],
    };
    let type_id = GraphTypeId::new(1).unwrap();
    let next = tx.catalog.apply(&seed.catalog).unwrap();
    let mut descriptors: Vec<_> = next
        .descriptors()
        .iter()
        .map(|d| {
            if matches!(d.id(), CatalogObjectId::Graph(_)) {
                CatalogDescriptor::new(
                    d.id(),
                    d.kind(),
                    d.name().clone(),
                    d.parent(),
                    d.generation(),
                    d.creation().clone(),
                    CatalogPayload::Graph {
                        graph_type: Some(type_id),
                    },
                )
                .unwrap()
            } else {
                d.clone()
            }
        })
        .collect();
    descriptors.push(
        CatalogDescriptor::graph_type(
            type_id,
            CatalogName::regular("Blueprint").unwrap(),
            SchemaId::new(1).unwrap(),
            generation(2),
            CreationMetadata::new(generation(2), None),
        )
        .unwrap(),
    );
    let mut water = next.high_water().clone();
    water.insert(selene_catalog::CatalogObjectKind::GraphType, 1);
    let records = CatalogLogicalRecords::new(generation(2), water, descriptors).unwrap();
    tx.catalog = CatalogDelta::between(&seed.catalog.reconstruct().unwrap(), &records).unwrap();
    tx.graph_types.push(TypeDelta {
        id: type_id,
        definition: Some(definition.clone()),
    });
    for graph in &mut tx.graphs {
        graph.definition = Some(definition.clone());
        for change in &mut graph.changes {
            if let Change::NodeCreated { labels, .. } = change {
                *labels = LabelSet::from_iter([db_string("L").unwrap()]);
            }
        }
    }
    tx
}

fn framed(state: &ReplayState, tx: &LogicalTransaction) -> Result<ReplayState, ReplayError> {
    let mut id = [0; 16];
    id[6] = 0x40;
    id[8] = 0x80;
    let context = Context {
        store: StoreId::from_bytes(id).unwrap(),
        epoch: StoreEpoch::new(1).unwrap(),
        sequence: state.catalog.reconstruct().unwrap().generation().get(),
        segment: [0; 32],
        previous: [0; 32],
    };
    let bytes = frame::encode(
        &tx.encode(Limits::default()).unwrap(),
        context,
        Compression::Raw,
        frame::MAX_PAYLOAD,
    )
    .unwrap();
    match state.apply_frame(&bytes, context, Boundary::SealedEnd, Limits::default())? {
        FrameCandidate::Complete { state, .. } => Ok(state),
        FrameCandidate::Incomplete { .. } => panic!("complete sealed fixture"),
    }
}

fn revise_type(state: &ReplayState, body: Option<GraphDefinition>) -> LogicalTransaction {
    let old = state.catalog.reconstruct().unwrap();
    let id = CatalogObjectId::GraphType(GraphTypeId::new(1).unwrap());
    let descriptors = old
        .descriptors()
        .map(|d| {
            if d.id() == id {
                CatalogDescriptor::new(
                    id,
                    d.kind(),
                    d.name().clone(),
                    d.parent(),
                    generation(3),
                    d.creation().clone(),
                    d.payload().clone(),
                )
                .unwrap()
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
    LogicalTransaction {
        catalog: CatalogDelta::between(&old, &next).unwrap(),
        graph_types: body
            .into_iter()
            .map(|definition| TypeDelta {
                id: GraphTypeId::new(1).unwrap(),
                definition: Some(definition),
            })
            .collect(),
        graphs: vec![],
    }
}

#[test]
fn named_binding_accepts_conforming_data_and_independent_instance_schema() {
    let seed = seed();
    let mut tx = named_transaction(&seed);
    // Graph-local CREATE NODE TYPE changes the instance definition, not the
    // registered type body. An unused local declaration is not byte equality.
    tx.graphs[1].definition.as_mut().unwrap().nodes.push((
        db_string("LocalOnly").unwrap(),
        selene_core::NodeTypeDef::new(LabelSet::from_iter([db_string("UNUSED").unwrap()])),
    ));
    let candidate = framed(&seed, &tx).unwrap();
    for id in [1, 2] {
        assert_eq!(
            candidate.graph_summary(GraphId::new(id)),
            Some((2, 1, 3, 2))
        );
        assert_eq!(
            candidate.node_property(GraphId::new(id), NodeId::new(1), &db_string("v").unwrap()),
            Some(&Value::Int(1))
        );
    }
    assert!(seed.graphs.is_empty());
}

#[test]
fn named_binding_rejects_missing_or_weakened_instance_with_invalid_data() {
    let seed = seed();
    let good = named_transaction(&seed);
    for case in 0..4 {
        let mut bad = good.clone();
        match case {
            0 => bad.graphs[1].definition = None,
            1 => {
                let property = &mut bad.graphs[1].definition.as_mut().unwrap().nodes[0]
                    .1
                    .properties[0];
                property.nullable = true;
                property.value_type.not_null = false;
            }
            2 => {
                bad.graphs[1].definition.as_mut().unwrap().nodes[0].1.labels =
                    LabelSet::from_iter([db_string("WRONG").unwrap()])
            }
            _ => {
                bad.graphs[1].definition.as_mut().unwrap().nodes[0]
                    .1
                    .properties[0]
                    .value_type
                    .predefined = Some(PredefinedValueType::Bool)
            }
        }
        for change in &mut bad.graphs[1].changes {
            if let Change::NodeCreated {
                labels, properties, ..
            } = change
            {
                match case {
                    0 | 3 => {
                        properties
                            .set(db_string("v").unwrap(), Value::Bool(true))
                            .unwrap();
                    }
                    1 => {
                        properties.remove(&db_string("v").unwrap());
                    }
                    _ => *labels = LabelSet::from_iter([db_string("WRONG").unwrap()]),
                }
            }
        }
        assert!(
            framed(&seed, &bad).is_err(),
            "case {case}: named constraints were bypassed"
        );
        assert!(seed.graphs.is_empty());
        assert_eq!(seed.catalog.descriptors().len(), 2);
    }
}

#[test]
fn named_binding_checks_immutable_operations_even_when_final_data_conforms() {
    let seed = seed();
    let creation = named_transaction(&seed);
    let state = framed(&seed, &creation).unwrap();
    let mut tx = followup(
        &state,
        vec![Change::NodeUpdated {
            id: NodeId::new(1),
            labels_diff: LabelDiff::new([], []).unwrap(),
            properties_diff: PropertyDiff::new([(db_string("v").unwrap(), Value::Int(9))], [])
                .unwrap(),
        }],
    );
    tx.graphs[0].definition = creation.graphs[0].definition.clone();
    tx.graphs[0].definition.as_mut().unwrap().nodes[0]
        .1
        .properties[0]
        .immutable = false;
    assert!(framed(&state, &tx).is_err());
    assert_eq!(
        state.node_property(GraphId::new(1), NodeId::new(1), &db_string("v").unwrap()),
        Some(&Value::Int(1))
    );
}

#[test]
fn named_binding_checks_type_and_instance_names_without_empty_name_shortcuts() {
    let seed = seed();
    let good = named_transaction(&seed);
    for (type_body, name) in [(true, "Other"), (true, ""), (false, "Other"), (false, "")] {
        let mut bad = good.clone();
        if type_body {
            bad.graph_types[0].definition.as_mut().unwrap().name = db_string(name).unwrap();
        } else {
            bad.graphs[1].definition.as_mut().unwrap().name = db_string(name).unwrap();
        }
        assert!(
            framed(&seed, &bad).is_err(),
            "type_body={type_body}, name={name:?}"
        );
    }
}

#[test]
fn named_type_revision_revalidates_untouched_referencing_graphs_atomically() {
    let seed = seed();
    let creation = named_transaction(&seed);
    let state = framed(&seed, &creation).unwrap();
    let original_catalog = state.catalog.clone();
    let first = state.graphs[&GraphId::new(1)].clone();
    let second = state.graphs[&GraphId::new(2)].clone();
    let mut revised = creation.graph_types[0].definition.clone().unwrap();
    revised.nodes[0].1.properties.push(required("next", false));
    let mut tx = revise_type(&state, Some(revised.clone()));
    let mut touched = followup(
        &state,
        (1..=2)
            .map(|id| Change::NodeUpdated {
                id: NodeId::new(id),
                labels_diff: LabelDiff::new([], []).unwrap(),
                properties_diff: PropertyDiff::new(
                    [(db_string("next").unwrap(), Value::Int(5))],
                    [],
                )
                .unwrap(),
            })
            .collect(),
    );
    touched.graphs[0].definition = Some(revised);
    tx.graphs = touched.graphs;
    // Graph one fits both definitions in this transaction; untouched graph two
    // still lacks the newly required field. No part may become a candidate.
    assert!(framed(&state, &tx).is_err());
    assert_eq!(state.catalog, original_catalog);
    assert!(Arc::ptr_eq(&state.graphs[&GraphId::new(1)], &first));
    assert!(Arc::ptr_eq(&state.graphs[&GraphId::new(2)], &second));
    assert!(
        state
            .node_property(GraphId::new(1), NodeId::new(1), &db_string("next").unwrap())
            .is_none()
    );
}

#[test]
fn changed_named_type_descriptor_requires_its_explicit_body_even_when_identical() {
    let seed = seed();
    let creation = named_transaction(&seed);
    let state = framed(&seed, &creation).unwrap();
    assert!(framed(&state, &revise_type(&state, None)).is_err());
    let revision = revise_type(&state, creation.graph_types[0].definition.clone());
    let candidate = framed(&state, &revision).unwrap();
    assert_eq!(
        candidate.catalog.reconstruct().unwrap().generation(),
        generation(3)
    );
    assert_eq!(candidate.graph_summary(GraphId::new(2)), Some((2, 1, 3, 2)));
    assert_eq!(
        state.catalog.reconstruct().unwrap().generation(),
        generation(2)
    );
}

#[test]
fn named_validation_keeps_create_update_delete_and_referential_order_semantics() {
    let seed = seed();
    let mut tx = named_transaction(&seed);
    let mut temporary = node(3);
    if let Change::NodeCreated { labels, .. } = &mut temporary {
        *labels = LabelSet::from_iter([db_string("L").unwrap()]);
    }
    tx.graphs[1].changes.extend([
        temporary,
        Change::NodeUpdated {
            id: NodeId::new(3),
            labels_diff: LabelDiff::new([], []).unwrap(),
            properties_diff: PropertyDiff::new([(db_string("v").unwrap(), Value::Bool(false))], [])
                .unwrap(),
        },
        Change::NodeDeleted { id: NodeId::new(3) },
    ]);
    tx.graphs[1].next_node_id = 4;
    let candidate = framed(&seed, &tx).unwrap();
    assert_eq!(candidate.graph_summary(GraphId::new(2)), Some((2, 1, 4, 2)));
    let len = tx.graphs[1].changes.len();
    tx.graphs[1].changes.swap(len - 3, len - 1);
    assert!(framed(&seed, &tx).is_err());
}

#[test]
fn named_validation_budget_is_cumulative_for_retained_graphs() {
    let seed = seed();
    let creation = named_transaction(&seed);
    let state = framed(&seed, &creation).unwrap();
    let revision = revise_type(&state, creation.graph_types[0].definition.clone());
    let bytes = revision.encode(Limits::default()).unwrap();
    let limits = Limits {
        items: 40,
        ..Limits::default()
    };
    assert!(
        LogicalTransaction::decode(&bytes, limits).is_ok(),
        "the bounded body itself fits"
    );
    assert_eq!(state.apply_body(&bytes, limits).err(), Some(E::Limit));
    assert_eq!(
        state.catalog.reconstruct().unwrap().generation(),
        generation(2)
    );
    assert_eq!(state.graph_summary(GraphId::new(2)), Some((2, 1, 3, 2)));
    let mut empty_creation = creation;
    for graph in &mut empty_creation.graphs {
        graph.changes.clear();
        graph.next_node_id = 1;
        graph.next_edge_id = 1;
    }
    let empty = framed(&seed, &empty_creation).unwrap();
    let revision = revise_type(&empty, empty_creation.graph_types[0].definition.clone());
    assert!(
        empty
            .apply_body(&revision.encode(Limits::default()).unwrap(), limits)
            .is_ok(),
        "the same descriptor/body revision fits without retained entity scans"
    );
}

#[test]
fn metadata_only_graph_payload_keeps_the_owner_advanced_generation() {
    use selene_catalog::{
        DeclarationMetadata, DeclarationState, NativeBinding, NativeDeclaration, NativeProjection,
        ProcedureId,
    };
    let seed = seed();
    let creation = named_transaction(&seed);
    let state = framed(&seed, &creation).unwrap();
    let old = state.catalog.reconstruct().unwrap();
    let mut descriptors: Vec<_> = old.descriptors().cloned().collect();
    descriptors.push(
        CatalogDescriptor::procedure(
            ProcedureId::new(1).unwrap(),
            CatalogName::regular("projection").unwrap(),
            CatalogParent::Graph(selene_catalog::GraphId::new(1).unwrap()),
            generation(3),
            CreationMetadata::new(generation(3), None),
            NativeDeclaration {
                metadata: DeclarationMetadata::new(DeclarationState::Inactive),
                binding: NativeBinding::Projection(NativeProjection {
                    node_labels: vec!["L".into()],
                    edge_labels: vec![],
                    weight_property: None,
                }),
            },
        )
        .unwrap(),
    );
    let mut water = state.catalog.high_water().clone();
    water.insert(selene_catalog::CatalogObjectKind::Procedure, 1);
    let next = CatalogLogicalRecords::new(generation(3), water, descriptors).unwrap();
    let mut tx = followup(&state, vec![]);
    tx.catalog = CatalogDelta::between(&old, &next).unwrap();
    tx.graphs[0].definition = creation.graphs[0].definition.clone();
    let candidate = framed(&state, &tx).unwrap();
    assert_eq!(candidate.graphs[&GraphId::new(1)].meta.generation, 2);
    assert_eq!(state.graphs[&GraphId::new(1)].meta.generation, 1);
    assert_eq!(candidate.graphs[&GraphId::new(2)].meta.generation, 1);
}

#[test]
fn reference_id_cannot_select_a_different_named_body_with_compatible_shape() {
    let seed = seed();
    let mut tx = named_transaction(&seed);
    let next = tx.catalog.apply(&seed.catalog).unwrap();
    let other = GraphTypeId::new(2).unwrap();
    let mut descriptors: Vec<_> = next
        .descriptors()
        .iter()
        .map(|d| {
            if matches!(d.id(), CatalogObjectId::Graph(id) if id.get() == 1) {
                CatalogDescriptor::new(
                    d.id(),
                    d.kind(),
                    d.name().clone(),
                    d.parent(),
                    d.generation(),
                    d.creation().clone(),
                    CatalogPayload::Graph {
                        graph_type: Some(other),
                    },
                )
                .unwrap()
            } else {
                d.clone()
            }
        })
        .collect();
    descriptors.push(
        CatalogDescriptor::graph_type(
            other,
            CatalogName::regular("Other").unwrap(),
            SchemaId::new(1).unwrap(),
            generation(2),
            CreationMetadata::new(generation(2), None),
        )
        .unwrap(),
    );
    let mut water = next.high_water().clone();
    water.insert(selene_catalog::CatalogObjectKind::GraphType, 2);
    let next = CatalogLogicalRecords::new(generation(2), water, descriptors).unwrap();
    tx.catalog = CatalogDelta::between(&seed.catalog.reconstruct().unwrap(), &next).unwrap();
    let mut definition = tx.graph_types[0].definition.clone().unwrap();
    definition.name = db_string("Other").unwrap();
    tx.graph_types.push(TypeDelta {
        id: other,
        definition: Some(definition),
    });
    assert!(framed(&seed, &tx).is_err());
}
