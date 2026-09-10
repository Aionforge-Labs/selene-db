use std::sync::Arc;

use selene_core::{ExtensionTypeId, GraphId, VectorValue};

use super::*;
use crate::{GraphError, SharedGraph};

#[path = "type_validator_tests/unique.rs"]
mod unique;

#[path = "type_validator_tests/unique_domains.rs"]
mod unique_domains;

#[path = "type_validator_tests/change.rs"]
mod change;

#[path = "type_validator_tests/record.rs"]
mod record;

fn db_string(name: &str) -> DbString {
    selene_core::db_string(name).unwrap()
}

fn prop(name: &str, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string(name), value)]).unwrap()
}

fn graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("validator.graph"),
        node_types: vec![
            crate::NodeTypeDef {
                name: db_string("validator.person"),
                key_labels: LabelSet::single(db_string("Person")),
                properties: vec![PropertyTypeDef {
                    name: db_string("name"),
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
            },
            crate::NodeTypeDef {
                name: db_string("validator.company"),
                key_labels: LabelSet::single(db_string("Company")),
                properties: vec![PropertyTypeDef {
                    name: db_string("name"),
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
            },
        ],
        edge_types: vec![crate::EdgeTypeDef {
            name: db_string("validator.works_at"),
            label: db_string("WORKS_AT"),
            source_node_type: EdgeEndpointDef::NodeType(0),
            target_node_type: EdgeEndpointDef::NodeType(1),
            properties: vec![PropertyTypeDef {
                name: db_string("since"),
                value_type: PropertyValueType::Int,
                list_element_type: None,
                required: false,
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
    }
}

fn valid_graph() -> SeleneGraph {
    let shared = SharedGraph::builder(GraphId::new(1)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let person = mutator
            .create_node(
                LabelSet::single(db_string("Person")),
                prop("name", Value::String(db_string("Alice"))),
            )
            .unwrap();
        let company = mutator
            .create_node(
                LabelSet::single(db_string("Company")),
                prop("name", Value::String(db_string("Acme"))),
            )
            .unwrap();
        mutator
            .create_edge(
                db_string("WORKS_AT"),
                person,
                company,
                prop("since", Value::Int(2026)),
            )
            .unwrap();
    }
    txn.commit().unwrap();
    shared.read().as_ref().clone()
}

#[test]
fn validate_entity_state_accepts_valid_graph() {
    validate_entity_state(&valid_graph(), &graph_type()).unwrap();
}

#[test]
fn validate_entity_state_accepts_vector_property() {
    let graph_type = GraphTypeDef {
        name: db_string("validator.vector.graph"),
        node_types: vec![crate::NodeTypeDef {
            name: db_string("validator.embedding"),
            key_labels: LabelSet::single(db_string("Embedding")),
            properties: vec![PropertyTypeDef {
                name: db_string("embedding"),
                value_type: PropertyValueType::Vector,
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
    };
    let shared = SharedGraph::builder(GraphId::new(11)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(db_string("Embedding")),
                prop(
                    "embedding",
                    Value::Vector(VectorValue::new(vec![0.1, 0.2, 0.3]).unwrap()),
                ),
            )
            .unwrap();
    }
    txn.commit().unwrap();
    validate_entity_state(shared.read().as_ref(), &graph_type).unwrap();
}

#[test]
fn rejects_unknown_node_label() {
    let shared = SharedGraph::builder(GraphId::new(2)).build().unwrap();
    let mut txn = shared.begin_write();
    let node = {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::single(db_string("Project")), PropertyMap::new())
            .unwrap()
    };
    txn.commit().unwrap();
    assert!(matches!(
        validate_entity_state(shared.read().as_ref(), &graph_type()),
        Err(TypeViolation::UnknownNodeLabel { id, .. }) if id == node
    ));
}

#[test]
fn rejects_unknown_edge_label() {
    let mut graph = valid_graph();
    graph.edge_store.label.set(0, db_string("KNOWS"));
    assert!(matches!(
        validate_entity_state(&graph, &graph_type()),
        Err(TypeViolation::UnknownEdgeLabel { id, label })
            if id == EdgeId::new(1) && label == db_string("KNOWS")
    ));
}

#[test]
fn rejects_edge_endpoint_mismatch() {
    let shared = SharedGraph::builder(GraphId::new(3)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let a = mutator
            .create_node(
                LabelSet::single(db_string("Company")),
                prop("name", Value::String(db_string("A"))),
            )
            .unwrap();
        let b = mutator
            .create_node(
                LabelSet::single(db_string("Person")),
                prop("name", Value::String(db_string("B"))),
            )
            .unwrap();
        mutator
            .create_edge(db_string("WORKS_AT"), a, b, PropertyMap::new())
            .unwrap();
    }
    txn.commit().unwrap();
    assert!(matches!(
        validate_entity_state(shared.read().as_ref(), &graph_type()),
        Err(TypeViolation::EdgeEndpointTypeMismatch {
            observed_source_type: 1,
            observed_target_type: 0,
            ..
        })
    ));
}

#[test]
fn rejects_missing_required_property() {
    let shared = SharedGraph::builder(GraphId::new(4)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::single(db_string("Person")), PropertyMap::new())
            .unwrap();
    }
    txn.commit().unwrap();
    assert!(matches!(
        validate_entity_state(shared.read().as_ref(), &graph_type()),
        Err(TypeViolation::MissingRequiredProperty { property, .. }) if property == db_string("name")
    ));
}

#[test]
fn rejects_property_type_mismatch() {
    let shared = SharedGraph::builder(GraphId::new(5)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(db_string("Person")),
                prop("name", Value::Int(7)),
            )
            .unwrap();
    }
    txn.commit().unwrap();
    assert!(matches!(
        validate_entity_state(shared.read().as_ref(), &graph_type()),
        Err(TypeViolation::PropertyTypeMismatch {
            expected: PropertyValueType::String,
            observed: "Int",
            ..
        })
    ));
}

#[test]
fn legacy_untyped_list_declaration_accepts_any_list_elements() {
    let declaration = PropertyTypeDef {
        name: db_string("legacy"),
        value_type: PropertyValueType::List,
        list_element_type: None,
        required: false,
        default: None,
        immutable: false,
        unique: false,
        decimal_type: None,
        character_string_type: None,
        byte_string_type: None,
        record_field_types: None,
    };

    assert!(property_value_matches(
        &declaration,
        &Value::List(vec![Value::Int(1), Value::String(db_string("two"))])
    ));
    assert!(!property_value_matches(&declaration, &Value::Int(1)));
}

#[test]
fn vector_declaration_matches_only_vector_values() {
    let declaration = PropertyTypeDef {
        name: db_string("embedding"),
        value_type: PropertyValueType::Vector,
        list_element_type: None,
        required: false,
        default: None,
        immutable: false,
        unique: false,
        decimal_type: None,
        character_string_type: None,
        byte_string_type: None,
        record_field_types: None,
    };

    assert!(property_value_matches(
        &declaration,
        &Value::Vector(VectorValue::new(vec![1.0, 2.0]).unwrap())
    ));
    assert!(!property_value_matches(
        &declaration,
        &Value::List(vec![Value::Float32(1.0), Value::Float32(2.0)])
    ));
}

#[test]
fn rejects_extension_value_before_property_admission() {
    let error = PropertyMap::from_pairs([(
        db_string("name"),
        Value::Extended {
            type_id: ExtensionTypeId(0x100),
            payload: Arc::from([1_u8]),
        },
    )])
    .unwrap_err();
    assert!(matches!(
        error,
        selene_core::CoreError::StoredValue(selene_core::StoredValueError::QueryOnly {
            family: "Extended"
        })
    ));
}

#[test]
fn rejects_undeclared_property() {
    let shared = SharedGraph::builder(GraphId::new(7)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let mut props = prop("name", Value::String(db_string("Alice")));
        props.set(db_string("extra"), Value::Bool(true)).unwrap();
        mutator
            .create_node(LabelSet::single(db_string("Person")), props)
            .unwrap();
    }
    txn.commit().unwrap();
    assert!(matches!(
        validate_entity_state(shared.read().as_ref(), &graph_type()),
        Err(TypeViolation::UndeclaredProperty { property, .. }) if property == db_string("extra")
    ));
}

#[test]
fn graph_error_wraps_type_violation() {
    let error = GraphError::from(TypeViolation::UnknownEdgeLabel {
        id: EdgeId::new(1),
        label: db_string("BAD"),
    });
    assert_eq!(error.gqlstatus(), "G2000");
}
