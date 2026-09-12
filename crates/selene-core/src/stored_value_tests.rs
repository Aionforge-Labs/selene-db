//! Independent stored-family and no-partial-container-write fixtures.

use crate::{
    CoreError, NodeId, PropertyDiff, PropertyMap, Record, StoredValue, StoredValueError, Value,
    db_string,
};
use proptest::prelude::*;

#[test]
fn format2_nested_list_bytes_reject_before_stack_exhaustion_and_reset_depth() {
    // Independent format-2 fixture: LIST tag 11, u32-LE count, then NULL tag 0.
    let mut bytes = Vec::new();
    for _ in 0..crate::MAX_STORED_VALUE_DEPTH {
        bytes.extend([11, 1, 0, 0, 0]);
    }
    bytes.push(0);
    assert!(crate::logical::decode_value(&bytes, Default::default()).is_err());
    assert_eq!(
        crate::logical::decode_value(&[0], Default::default())
            .unwrap()
            .as_value(),
        &Value::Null
    );
    let mut value = Value::Null;
    for _ in 1..crate::MAX_STORED_VALUE_DEPTH {
        value = Value::List(vec![value]);
    }
    assert!(StoredValue::validate(&value).is_ok());
    assert!(encode(&value).is_ok());
    let over = Value::List(vec![value]);
    assert!(StoredValue::validate(&over).is_err());
    assert!(encode(&over).is_err());
    assert_eq!(encode(&Value::Null).unwrap(), vec![0]);
}

fn encode(value: &Value) -> crate::logical::CodecResult<Vec<u8>> {
    let mut encoder = crate::logical::Encoder::new(Default::default())?;
    encoder.value(value, 1)?;
    Ok(encoder.finish())
}

#[test]
fn runtime_family_census_is_partitioned_by_storage_admission() {
    let mut forbidden = Vec::new();
    for make in Value::ALL {
        let value = make();
        let expected = !matches!(
            value,
            Value::NodeRef(_)
                | Value::EdgeRef(_)
                | Value::GraphRef(_)
                | Value::TableRef(_)
                | Value::Path(_)
                | Value::Extended { .. }
                | Value::RecordTyped(_)
        );
        assert_eq!(
            StoredValue::try_from(value.clone()).is_ok(),
            expected,
            "{}",
            value.variant_name()
        );
        if !expected {
            forbidden.push(value.variant_name());
        }
    }
    assert_eq!(forbidden.len(), 7);
}

#[test]
fn positional_record_ids_are_not_stored_semantic_descriptors() {
    let value = Value::RecordTyped(Box::new(crate::RecordTyped {
        type_id: crate::RecordTypeId::new(1),
        values: [Some(Value::Int(7))].into_iter().collect(),
    }));
    assert!(matches!(
        StoredValue::try_from(value),
        Err(CoreError::StoredValue(
            StoredValueError::MissingRecordDescriptor
        ))
    ));
}

#[test]
fn named_record_values_preserve_names_without_a_catalog_or_type_arena() {
    let value = Value::Record(Box::new(Record::Open(
        [(
            db_string("ExactName").unwrap(),
            Value::List(vec![Value::Null]),
        )]
        .into_iter()
        .collect(),
    )));
    let stored = StoredValue::try_from(value.clone()).unwrap();
    assert_eq!(stored.as_value(), &value);
    assert_eq!(stored.into_value(), value);
}

#[test]
fn rejected_set_does_not_replace_or_widen_a_compact_property_map() {
    let key = db_string("kept").unwrap();
    let mut map = PropertyMap::compact([key.clone()], [Some(Value::Int(1))]).unwrap();
    let before = map.clone();
    for name in [key, db_string("new").unwrap()] {
        assert!(
            map.set(name, Value::List(vec![Value::NodeRef(NodeId::new(1))]))
                .is_err()
        );
        assert_eq!(map, before);
    }
}

#[test]
fn logical_property_wire_rejects_bypassed_public_legacy_constructors() {
    let key = db_string("p").unwrap();
    let value = Value::List(vec![Value::NodeRef(NodeId::new(1))]);
    let map = PropertyMap::Standard([(key.clone(), value.clone())].into_iter().collect());
    let diff = PropertyDiff {
        set: [(key, value)].into_iter().collect(),
        removed: Default::default(),
    };
    assert!(crate::serde_tests::encode_map(&map).is_err());
    assert!(
        crate::serde_tests::encode_changes(vec![crate::Change::NodeUpdated {
            id: NodeId::new(1),
            labels_diff: crate::LabelDiff::new([], []).unwrap(),
            properties_diff: diff
        }])
        .is_err()
    );
}

#[test]
fn format2_property_defaults_reject_query_only_payloads() {
    use crate::{PredefinedValueType, PropertyDef, ValueType};
    let name = db_string("p").unwrap();
    let ty = ValueType::predefined(PredefinedValueType::Int);
    for value in [
        Value::Int(7),
        Value::List(vec![Value::NodeRef(NodeId::new(1))]),
    ] {
        let mut node = crate::NodeTypeDef::new(crate::LabelSet::single(name.clone()));
        node.properties.push(PropertyDef {
            name: name.clone(),
            value_type: ty.clone(),
            nullable: true,
            default: Some(value.clone()),
            immutable: false,
            unique: false,
            record_fields: None,
        });
        let definition = crate::logical::GraphDefinition {
            name: name.clone(),
            nodes: vec![(name.clone(), node)],
            edges: vec![],
        };
        let mut encoder = crate::logical::Encoder::new(Default::default()).unwrap();
        let result = encoder.graph_definition(&definition);
        if matches!(value, Value::Int(_)) {
            result.unwrap();
            let bytes = encoder.finish();
            let mut budget = crate::logical::Budget::new(Default::default()).unwrap();
            let mut decoder = crate::logical::Decoder::new(&bytes, &mut budget).unwrap();
            assert_eq!(decoder.graph_definition().unwrap(), definition);
            decoder.finish().unwrap();
        } else {
            assert!(result.is_err());
        }
    }
}

proptest! {
    #[test]
    fn arbitrary_container_nesting_cannot_hide_a_reference(wrappers in prop::collection::vec(any::<bool>(), 0..32)) {
        let mut value = Value::NodeRef(NodeId::new(1));
        for list in wrappers {
            value = if list { Value::List(vec![Value::Int(0), value]) } else {
                Value::Record(Box::new(Record::Open([(db_string("nested").unwrap(), value)].into_iter().collect())))
            };
        }
        prop_assert!(StoredValue::try_from(value).is_err());
    }
}
