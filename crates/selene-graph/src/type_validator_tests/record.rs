use selene_core::{Record, RecordTypeId, RecordTyped};

use super::*;
use crate::{RecordFieldType, RecordFieldTypeDef, RecordFieldTypes};

// --- C7: closed/typed RECORD property validation (ISO 39075:2024 §4.15.4) ---

fn open_record(fields: &[(&str, Value)]) -> Value {
    Value::Record(Box::new(Record::Open(
        fields
            .iter()
            .map(|(name, value)| (db_string(name), value.clone()))
            .collect(),
    )))
}

/// A `config` RECORD descriptor with `host :: STRING NOT NULL` and
/// `port :: INT`. Closed-record field names are mandatory by set equality;
/// `required` controls whether the field value itself may be NULL.
fn closed_record_declaration() -> PropertyTypeDef {
    PropertyTypeDef {
        name: db_string("config"),
        value_type: PropertyValueType::RecordTyped,
        list_element_type: None,
        required: false,
        default: None,
        immutable: false,
        unique: false,
        decimal_type: None,
        character_string_type: None,
        byte_string_type: None,
        record_field_types: Some(RecordFieldTypes(vec![
            RecordFieldTypeDef {
                name: db_string("host"),
                field_type: RecordFieldType::Scalar(PropertyValueType::String),
                required: true,
            },
            RecordFieldTypeDef {
                name: db_string("port"),
                field_type: RecordFieldType::Scalar(PropertyValueType::Int),
                required: false,
            },
        ])),
    }
}

#[test]
fn closed_record_accepts_conforming_value() {
    let declaration = closed_record_declaration();
    assert!(property_value_matches(
        &declaration,
        &open_record(&[
            ("host", Value::String(db_string("h"))),
            ("port", Value::Int(8080)),
        ])
    ));
    // Nullable fields still must be present in a closed record value.
    assert!(!property_value_matches(
        &declaration,
        &open_record(&[("host", Value::String(db_string("h")))])
    ));
}

#[test]
fn closed_record_rejects_missing_declared_field() {
    let declaration = closed_record_declaration();
    assert!(!property_value_matches(
        &declaration,
        &open_record(&[("port", Value::Int(8080))])
    ));
}

#[test]
fn closed_record_rejects_wrong_field_type() {
    let declaration = closed_record_declaration();
    assert!(!property_value_matches(
        &declaration,
        &open_record(&[("host", Value::Int(1)), ("port", Value::Int(80))])
    ));
}

#[test]
fn closed_record_rejects_extra_undeclared_field() {
    // ISO §4.15.4 closed record = field-name-set EQUALITY: an extra field is rejected.
    let declaration = closed_record_declaration();
    assert!(!property_value_matches(
        &declaration,
        &open_record(&[
            ("host", Value::String(db_string("h"))),
            ("port", Value::Int(8080)),
            ("extra", Value::Bool(true)),
        ])
    ));
}

#[test]
fn closed_record_accepts_and_rejects_nested() {
    // config :: RECORD { id :: INT, tags :: LIST<STRING>, meta :: RECORD { live :: BOOL } }
    let declaration = PropertyTypeDef {
        name: db_string("config"),
        value_type: PropertyValueType::RecordTyped,
        list_element_type: None,
        required: true,
        default: None,
        immutable: false,
        unique: false,
        decimal_type: None,
        character_string_type: None,
        byte_string_type: None,
        record_field_types: Some(RecordFieldTypes(vec![
            RecordFieldTypeDef {
                name: db_string("id"),
                field_type: RecordFieldType::Scalar(PropertyValueType::Int),
                required: true,
            },
            RecordFieldTypeDef {
                name: db_string("tags"),
                field_type: RecordFieldType::List(Box::new(RecordFieldType::Scalar(
                    PropertyValueType::String,
                ))),
                required: true,
            },
            RecordFieldTypeDef {
                name: db_string("meta"),
                field_type: RecordFieldType::Record(Box::new(RecordFieldTypes(vec![
                    RecordFieldTypeDef {
                        name: db_string("live"),
                        field_type: RecordFieldType::Scalar(PropertyValueType::Bool),
                        required: true,
                    },
                ]))),
                required: true,
            },
        ])),
    };
    let conforming = open_record(&[
        ("id", Value::Int(1)),
        (
            "tags",
            Value::List(vec![
                Value::String(db_string("a")),
                Value::String(db_string("b")),
            ]),
        ),
        ("meta", open_record(&[("live", Value::Bool(true))])),
    ]);
    assert!(property_value_matches(&declaration, &conforming));

    // Nested record-of-list violation: a tag is not a string.
    let bad_list = open_record(&[
        ("id", Value::Int(1)),
        ("tags", Value::List(vec![Value::Int(2)])),
        ("meta", open_record(&[("live", Value::Bool(true))])),
    ]);
    assert!(!property_value_matches(&declaration, &bad_list));

    // Nested record-of-record violation: inner field wrong type.
    let bad_inner = open_record(&[
        ("id", Value::Int(1)),
        ("tags", Value::List(vec![Value::String(db_string("a"))])),
        ("meta", open_record(&[("live", Value::Int(0))])),
    ]);
    assert!(!property_value_matches(&declaration, &bad_inner));
}

#[test]
fn nested_open_record_field_accepts_any_record_shape() {
    let declaration = PropertyTypeDef {
        name: db_string("payload"),
        value_type: PropertyValueType::RecordTyped,
        list_element_type: None,
        required: true,
        default: None,
        immutable: false,
        unique: false,
        decimal_type: None,
        character_string_type: None,
        byte_string_type: None,
        record_field_types: Some(RecordFieldTypes(vec![
            RecordFieldTypeDef {
                name: db_string("meta"),
                field_type: RecordFieldType::OpenRecord,
                required: true,
            },
            RecordFieldTypeDef {
                name: db_string("snapshots"),
                field_type: RecordFieldType::List(Box::new(RecordFieldType::OpenRecord)),
                required: true,
            },
        ])),
    };
    let conforming = open_record(&[
        (
            "meta",
            open_record(&[("kind", Value::String(db_string("agent")))]),
        ),
        (
            "snapshots",
            Value::List(vec![
                open_record(&[("id", Value::String(db_string("a")))]),
                open_record(&[("id", Value::String(db_string("b")))]),
            ]),
        ),
    ]);
    assert!(property_value_matches(&declaration, &conforming));

    let bad_meta = open_record(&[
        ("meta", Value::String(db_string("not-record"))),
        (
            "snapshots",
            Value::List(vec![open_record(&[("id", Value::String(db_string("a")))])]),
        ),
    ]);
    assert!(!property_value_matches(&declaration, &bad_meta));

    let bad_snapshot = open_record(&[
        (
            "meta",
            open_record(&[("kind", Value::String(db_string("agent")))]),
        ),
        (
            "snapshots",
            Value::List(vec![Value::String(db_string("not-record"))]),
        ),
    ]);
    assert!(!property_value_matches(&declaration, &bad_snapshot));
}

#[test]
fn closed_record_rejects_positional_legacy_records() {
    let declaration = closed_record_declaration();
    // Even matching positional slots have no semantic field names.
    let conforming = Value::RecordTyped(Box::new(RecordTyped {
        type_id: RecordTypeId::new(1),
        values: [Some(Value::String(db_string("h"))), Some(Value::Int(80))]
            .into_iter()
            .collect(),
    }));
    assert!(!property_value_matches(&declaration, &conforming));

    // Positional NULL does not supply a semantic descriptor.
    let optional_omitted = Value::RecordTyped(Box::new(RecordTyped {
        type_id: RecordTypeId::new(1),
        values: [Some(Value::String(db_string("h"))), None]
            .into_iter()
            .collect(),
    }));
    assert!(!property_value_matches(&declaration, &optional_omitted));

    // Required host omitted (None at position 0) is rejected.
    let required_omitted = Value::RecordTyped(Box::new(RecordTyped {
        type_id: RecordTypeId::new(1),
        values: [None, Some(Value::Int(80))].into_iter().collect(),
    }));
    assert!(!property_value_matches(&declaration, &required_omitted));

    // Wrong arity is rejected.
    let wrong_arity = Value::RecordTyped(Box::new(RecordTyped {
        type_id: RecordTypeId::new(1),
        values: [Some(Value::String(db_string("h")))].into_iter().collect(),
    }));
    assert!(!property_value_matches(&declaration, &wrong_arity));
}

#[test]
fn closed_record_optional_field_accepts_explicit_null() {
    // A nullable field set to explicit NULL conforms, while a NOT NULL field set
    // to NULL is rejected. Missing nullable fields are still rejected by closed
    // record field-name-set equality.
    let declaration = closed_record_declaration();

    // Open form: optional `port` present as explicit NULL conforms.
    let port_null = open_record(&[
        ("host", Value::String(db_string("h"))),
        ("port", Value::Null),
    ]);
    assert!(property_value_matches(&declaration, &port_null));

    // Open form: required `host` present as explicit NULL is rejected.
    let host_null = open_record(&[("host", Value::Null), ("port", Value::Int(80))]);
    assert!(!property_value_matches(&declaration, &host_null));

    // Positional form still lacks named-field semantics.
    let positional_null = Value::RecordTyped(Box::new(RecordTyped {
        type_id: RecordTypeId::new(1),
        values: [Some(Value::String(db_string("h"))), Some(Value::Null)]
            .into_iter()
            .collect(),
    }));
    assert!(!property_value_matches(&declaration, &positional_null));
}

#[test]
fn open_bare_record_declaration_accepts_any_record() {
    // value_type RecordTyped with no declared field structure is permissive (mirrors the
    // legacy untyped LIST path).
    let declaration = PropertyTypeDef {
        name: db_string("anything"),
        value_type: PropertyValueType::RecordTyped,
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
        &open_record(&[("whatever", Value::Int(1))])
    ));
    assert!(property_value_matches(&declaration, &open_record(&[])));
    // A non-record value is still rejected by the coarse tag gate.
    assert!(!property_value_matches(&declaration, &Value::Int(1)));
}

#[test]
fn closed_record_violation_is_graph_type_violation_g2000() {
    // End-to-end: a non-conforming record property surfaces as G2000.
    let record_graph_type = GraphTypeDef {
        name: db_string("record.validator.graph"),
        node_types: vec![crate::NodeTypeDef {
            name: db_string("record.host"),
            key_labels: LabelSet::single(db_string("Host")),
            properties: vec![closed_record_declaration()],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    };
    let shared = SharedGraph::builder(GraphId::new(8)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(db_string("Host")),
                // host has the wrong type -> closed-record violation.
                prop("config", open_record(&[("host", Value::Int(1))])),
            )
            .unwrap();
    }
    txn.commit().unwrap();
    let violation = validate_entity_state(shared.read().as_ref(), &record_graph_type)
        .expect_err("non-conforming record must be rejected");
    assert!(matches!(
        violation,
        TypeViolation::PropertyTypeMismatch { ref property, .. } if *property == db_string("config")
    ));
    assert_eq!(GraphError::from(violation).gqlstatus(), "G2000");
}
