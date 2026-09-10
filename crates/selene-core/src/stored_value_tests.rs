//! Independent stored-family and no-partial-container-write fixtures.

use crate::{
    CoreError, NodeId, PropertyDiff, PropertyMap, Record, StoredValue, StoredValueError, Value,
    db_string,
};
use proptest::prelude::*;

#[test]
fn legacy_nested_list_bytes_reject_before_stack_exhaustion_and_reset_depth() {
    // Independent postcard fixture: Value::List tag 10, length 1, repeatedly,
    // followed by Value::Null tag 25. No encoder shares the depth policy here.
    let mut bytes = Vec::new();
    for _ in 0..crate::MAX_STORED_VALUE_DEPTH {
        bytes.extend([10, 1]);
    }
    bytes.push(25);
    assert!(postcard::from_bytes::<Value>(&bytes).is_err());
    assert_eq!(postcard::from_bytes::<Value>(&[25]).unwrap(), Value::Null);
    let mut value = Value::Null;
    for _ in 1..crate::MAX_STORED_VALUE_DEPTH {
        value = Value::List(vec![value]);
    }
    assert!(StoredValue::validate(&value).is_ok());
    assert!(postcard::to_allocvec(&value).is_ok());
    let over = Value::List(vec![value]);
    assert!(StoredValue::validate(&over).is_err());
    assert!(postcard::to_allocvec(&over).is_err());
    assert_eq!(postcard::to_allocvec(&Value::Null).unwrap(), vec![25]);
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
    assert!(postcard::to_allocvec(&map).is_err());
    assert!(postcard::to_allocvec(&diff).is_err());
}

#[test]
fn legacy_property_default_bytes_reject_query_only_payloads_and_keep_scalar_layout() {
    use crate::{PredefinedValueType, PropertyDef, PropertyDefV1, ValueType};
    let name = db_string("p").unwrap();
    let ty = ValueType::predefined(PredefinedValueType::Int);
    for value in [
        Value::Int(7),
        Value::List(vec![Value::NodeRef(NodeId::new(1))]),
    ] {
        // Independent legacy field sequence, bypassing the guarded PropertyDef serializer.
        let bytes =
            postcard::to_allocvec(&(name.clone(), ty.clone(), true, Some(value.clone()))).unwrap();
        let old = postcard::from_bytes::<PropertyDefV1>(&bytes);
        let current = postcard::to_allocvec(&(
            name.clone(),
            ty.clone(),
            true,
            Some(value.clone()),
            false,
            false,
            None::<Box<crate::RecordFieldStructure>>,
        ))
        .unwrap();
        let decoded = postcard::from_bytes::<PropertyDef>(&current);
        if matches!(value, Value::Int(_)) {
            assert_eq!(postcard::to_allocvec(&old.unwrap()).unwrap(), bytes);
            assert_eq!(postcard::to_allocvec(&decoded.unwrap()).unwrap(), current);
        } else {
            assert!(old.is_err());
            assert!(decoded.is_err());
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
