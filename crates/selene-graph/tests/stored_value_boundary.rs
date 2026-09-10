//! Raw/native mutation and schema admission cannot bypass the stored boundary.

use selene_core::{
    GraphId, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value, db_string,
};
use selene_graph::SharedGraph;

#[test]
fn forged_schema_defaults_fail_before_wal_or_publication() {
    use selene_core::{NodeTypeDef, PredefinedValueType, PropertyDef, SchemaChange, ValueType};
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-stored-boundary-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let wal = dir.join("wal.log");
    let graph = SharedGraph::builder(GraphId::new(1))
        .with_wal(&wal, selene_persist::WalConfig::default())
        .unwrap()
        .build()
        .unwrap();
    let length = std::fs::metadata(&wal).unwrap().len();
    let before = graph.read();
    let mut tx = graph.begin_write();
    tx.mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let mut def = NodeTypeDef::new(LabelSet::single(db_string("N").unwrap()));
    def.properties.push(PropertyDef {
        name: db_string("bad").unwrap(),
        value_type: ValueType::list_of(ValueType::predefined(PredefinedValueType::Int)),
        nullable: true,
        default: Some(Value::List(vec![Value::NodeRef(NodeId::new(1))])),
        immutable: false,
        unique: false,
        record_fields: None,
    });
    tx.mutator().schema_change(SchemaChange::NodeTypeAddedV2 {
        graph_type: selene_core::GraphTypeId::new(1).unwrap(),
        label: db_string("N").unwrap(),
        def,
    });
    let error = tx.commit().unwrap_err();
    assert_eq!(error.gqlstatus(), "22G03");
    assert_eq!(graph.read().node_count(), 0);
    assert_eq!(graph.read().meta.generation, before.meta.generation);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), length);
    drop(graph);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn direct_recovery_rejects_invalid_change_before_any_field_is_applied() {
    use selene_core::Change;
    use selene_graph::{CoreProvider, IndexProvider};
    let provider = CoreProvider::new_for_recovery();
    IndexProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::new(),
            properties: PropertyMap::new(),
        },
    )
    .unwrap();
    let change = Change::NodeUpdated {
        id: NodeId::new(1),
        labels_diff: LabelDiff::new([db_string("bad_label").unwrap()], []).unwrap(),
        properties_diff: PropertyDiff {
            set: [
                (db_string("first").unwrap(), Value::Int(1)),
                (db_string("last").unwrap(), Value::NodeRef(NodeId::new(1))),
            ]
            .into_iter()
            .collect(),
            removed: Default::default(),
        },
    };
    assert!(IndexProvider::on_change(provider.as_ref(), &change).is_err());
    assert!(
        IndexProvider::on_change(
            provider.as_ref(),
            &Change::NodeCreated {
                id: NodeId::new(2),
                labels: LabelSet::new(),
                properties: forged_map(),
            }
        )
        .is_err()
    );
    let graph = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert_eq!(graph.node_count(), 1);
    assert!(graph.node_labels(NodeId::new(1)).unwrap().is_empty());
    assert!(graph.node_properties(NodeId::new(1)).unwrap().is_empty());
}

#[test]
fn query_only_property_types_are_rejected_even_with_null_defaults() {
    use selene_core::PropertyValueType as P;
    use selene_graph::{
        GraphTypeDef, NodeTypeDef, PropertyDefaultValue, PropertyElementType, PropertyTypeDef,
        RecordFieldType, RecordFieldTypeDef, RecordFieldTypes, ValidationMode,
    };
    for forbidden in [P::NodeRef, P::EdgeRef, P::Path, P::GraphRef, P::TableRef] {
        for nesting in 0..3 {
            let property = PropertyTypeDef {
                name: db_string("value").unwrap(),
                value_type: match nesting {
                    0 => forbidden,
                    1 => P::List,
                    _ => P::RecordTyped,
                },
                required: false,
                default: Some(PropertyDefaultValue::Null),
                immutable: false,
                unique: false,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                list_element_type: (nesting == 1).then(|| {
                    PropertyElementType::List(Box::new(PropertyElementType::Scalar(forbidden)))
                }),
                record_field_types: (nesting == 2).then(|| {
                    RecordFieldTypes(vec![RecordFieldTypeDef {
                        name: db_string("nested").unwrap(),
                        required: false,
                        field_type: RecordFieldType::List(Box::new(RecordFieldType::Scalar(
                            forbidden,
                        ))),
                    }])
                }),
            };
            let graph_type = GraphTypeDef {
                name: db_string("schema").unwrap(),
                edge_types: vec![],
                node_types: vec![NodeTypeDef {
                    name: db_string("N").unwrap(),
                    key_labels: LabelSet::single(db_string("N").unwrap()),
                    properties: vec![property],
                    validation_mode: ValidationMode::Strict,
                }],
            };
            assert_eq!(graph_type.validate_ref().unwrap_err().gqlstatus(), "22G03");
        }
    }
}

fn forged_map() -> PropertyMap {
    PropertyMap::Standard(
        [(
            db_string("bad").unwrap(),
            Value::List(vec![Value::NodeRef(NodeId::new(1))]),
        )]
        .into_iter()
        .collect(),
    )
}

#[test]
fn defaults_obey_the_same_depth_limit_before_materialization() {
    use selene_graph::{PropertyDefaultRecordField, PropertyDefaultValue as D};
    for record in [false, true] {
        let mut value = Value::Int(1);
        let mut default = D::Integer(1);
        for _ in 1..selene_core::MAX_STORED_VALUE_DEPTH {
            if record {
                value = Value::Record(Box::new(selene_core::Record::Open(
                    [(db_string("field").unwrap(), value)].into_iter().collect(),
                )));
                default = D::Record(vec![PropertyDefaultRecordField {
                    name: db_string("field").unwrap(),
                    value: Box::new(default),
                }]);
            } else {
                value = Value::List(vec![value]);
                default = D::List(vec![Box::new(default)]);
            }
        }
        assert!(D::from_value(&value).is_some());
        assert_eq!(default.to_value().unwrap(), value);
        value = Value::List(vec![value]);
        default = D::List(vec![Box::new(default)]);
        assert!(D::from_value(&value).is_none());
        assert_eq!(default.to_value().unwrap_err().gqlstatus(), "22G03");
    }
}

#[test]
fn bare_list_declarations_do_not_skip_default_validation() {
    use selene_graph::{
        GraphTypeDef, NodeTypeDef, PropertyDefaultValue, PropertyTypeDef, ValidationMode,
    };
    let graph_type = GraphTypeDef {
        name: db_string("list_schema").unwrap(),
        edge_types: vec![],
        node_types: vec![NodeTypeDef {
            name: db_string("N").unwrap(),
            key_labels: LabelSet::single(db_string("N").unwrap()),
            validation_mode: ValidationMode::Strict,
            properties: vec![PropertyTypeDef {
                name: db_string("bad").unwrap(),
                value_type: selene_core::PropertyValueType::List,
                required: false,
                default: Some(PropertyDefaultValue::Integer(1)),
                immutable: false,
                unique: false,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                list_element_type: None,
                record_field_types: None,
            }],
        }],
    };
    assert!(graph_type.validate_ref().is_err());
}

#[test]
fn legacy_descriptor_adapters_reject_deep_wrappers_before_recursive_lowering() {
    use selene_graph::{PropertyElementType, RecordFieldType};
    let mut list = PropertyElementType::Scalar(selene_core::PropertyValueType::Int);
    let mut record = RecordFieldType::Scalar(selene_core::PropertyValueType::Int);
    for _ in 0..4096 {
        list = PropertyElementType::NotNull(Box::new(list));
        record = RecordFieldType::NotNull(Box::new(record));
    }
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn_scoped(scope, || {
                assert!(list.structural_type().is_err());
                assert!(record.structural_type().is_err());
            })
            .unwrap()
            .join()
            .unwrap();
    });
}

#[test]
fn independent_legacy_default_bytes_enforce_list_and_record_decode_limits() {
    use selene_graph::PropertyDefaultValue;
    for prefix in [vec![5, 1], vec![6, 1, 1, b'x']] {
        let mut bytes = prefix.repeat(selene_core::MAX_STORED_VALUE_DEPTH - 1);
        bytes.push(0); // Legacy Null tag; Box carries no extra postcard framing.
        let admitted: PropertyDefaultValue = postcard::from_bytes(&bytes).unwrap();
        assert!(admitted.to_value().is_ok());
        assert_eq!(postcard::to_allocvec(&admitted).unwrap(), bytes);
        let over = [prefix.as_slice(), bytes.as_slice()].concat();
        assert!(postcard::from_bytes::<PropertyDefaultValue>(&over).is_err());
        assert_eq!(
            postcard::from_bytes::<PropertyDefaultValue>(&[0]).unwrap(),
            PropertyDefaultValue::Null
        );
    }
}

#[test]
fn native_create_and_update_reject_forged_containers_before_any_change() {
    let graph = SharedGraph::new(GraphId::new(1));
    let mut tx = graph.begin_write();
    let a = tx
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let b = tx
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let edge = tx
        .mutator()
        .create_edge(db_string("E").unwrap(), a, b, PropertyMap::new())
        .unwrap();
    tx.commit().unwrap();
    let mut tx = graph.begin_write();
    let error = tx
        .mutator()
        .create_node(LabelSet::new(), forged_map())
        .unwrap_err();
    assert_eq!(error.gqlstatus(), "22G03");
    assert!(
        tx.mutator()
            .create_edge(db_string("E").unwrap(), a, b, forged_map())
            .is_err()
    );
    let diff = || PropertyDiff {
        set: [
            (db_string("first").unwrap(), Value::Int(7)),
            (db_string("last").unwrap(), Value::NodeRef(a)),
        ]
        .into_iter()
        .collect(),
        removed: Default::default(),
    };
    assert!(
        tx.mutator()
            .update_node(a, LabelDiff::new([], []).unwrap(), diff())
            .is_err()
    );
    assert!(tx.mutator().update_edge(edge, diff()).is_err());
    assert_eq!(tx.change_count(), 0);
    tx.commit().unwrap();
    let snapshot = graph.read();
    assert_eq!(snapshot.node_count(), 2);
    assert_eq!(snapshot.edge_count(), 1);
    assert!(snapshot.node_properties(a).unwrap().is_empty());
    assert!(snapshot.edge_properties(edge).unwrap().is_empty());
}
