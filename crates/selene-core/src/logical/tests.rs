use super::*;
use crate::{StoredValue, Value};
use proptest::prelude::*;

#[test]
fn independent_scalar_vector() {
    // tag 0x02 = i64; eight little-endian two's-complement bytes, no serde tag.
    let bytes = [2, 0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
    let value = StoredValue::try_from(Value::Int(-2)).unwrap();
    assert_eq!(decode_value(&bytes, Limits::default()).unwrap(), value);
    assert_eq!(encode_value(&value, Limits::default()).unwrap(), bytes);
}

#[test]
fn independent_temporal_decimal_vector_and_named_record_bytes() {
    for (tag, text) in [
        (16, "2026-09-10"),
        (17, "12:34:56.123456789"),
        (18, "2026-09-10T12:34:56"),
        (19, "2026-09-10T12:34:56+00:00[UTC]"),
        (20, "2026-09-10T12:34:56+00:00[UTC]"),
    ] {
        let mut bytes = vec![tag];
        bytes.extend((text.len() as u32).to_le_bytes());
        bytes.extend(text.as_bytes());
        let value = decode_value(&bytes, Limits::default()).unwrap();
        assert_eq!(encode_value(&value, Limits::default()).unwrap(), bytes);
    }
    let decimal = [8, 0, 0, 2, 0, 210, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(
        decode_value(&decimal, Limits::default())
            .unwrap()
            .as_value(),
        &Value::Decimal("12.34".parse().unwrap())
    );
    let vector = [14, 2, 0, 0, 0, 0, 0, 128, 63, 0, 0, 0, 128];
    let value = decode_value(&vector, Limits::default()).unwrap();
    assert_eq!(encode_value(&value, Limits::default()).unwrap(), vector);
    let named = [12, 1, 0, 0, 0, 1, 0, 0, 0, b'a', 0];
    let value = decode_value(&named, Limits::default()).unwrap();
    let Value::Record(record) = value.as_value() else {
        panic!("record")
    };
    let crate::Record::Open(fields) = record.as_ref();
    assert_eq!(fields[0], (crate::db_string("a").unwrap(), Value::Null));
}

#[test]
fn independent_empty_null_integer_and_float_representations() {
    let cases = [
        (vec![0], Value::Null),
        (vec![1, 1], Value::Bool(true)),
        (
            vec![9, 0, 0, 0, 0],
            Value::String(crate::db_string("").unwrap()),
        ),
        (vec![10, 0, 0, 0, 0], Value::Bytes([].into())),
        (vec![11, 0, 0, 0, 0], Value::List(vec![])),
        (
            vec![12, 0, 0, 0, 0],
            Value::Record(Box::new(crate::Record::Open(smallvec::smallvec![]))),
        ),
        (vec![7, 0, 0, 0, 128], Value::Float32(-0.0)),
        (vec![6, 0, 0, 0, 0, 0, 0, 0, 128], Value::Float(-0.0)),
    ];
    for (bytes, expected) in cases {
        let value = decode_value(&bytes, Limits::default()).unwrap();
        assert_eq!(value.as_value(), &expected);
        assert_eq!(encode_value(&value, Limits::default()).unwrap(), bytes);
    }
    let mut signed = vec![4];
    signed.extend([255; 16]);
    assert_eq!(
        decode_value(&signed, Limits::default()).unwrap().as_value(),
        &Value::Int128(-1)
    );
    signed[0] = 5;
    assert_eq!(
        decode_value(&signed, Limits::default()).unwrap().as_value(),
        &Value::Uint128(u128::MAX)
    );
    let bytes = [7, 1, 0, 192, 127]; // A particular binary32 NaN payload, not normalized.
    let value = decode_value(&bytes, Limits::default()).unwrap();
    assert_eq!(encode_value(&value, Limits::default()).unwrap(), bytes);
}

#[test]
fn selected_value_inventory_roundtrips_without_legacy_enum_layout() {
    let mut admitted = 0;
    let mut rejected = 0;
    for factory in Value::ALL {
        let value = factory();
        match StoredValue::try_from(value) {
            Ok(value) => {
                let bytes = encode_value(&value, Limits::default()).unwrap();
                let decoded = decode_value(&bytes, Limits::default()).unwrap();
                assert_eq!(decoded, value);
                assert_eq!(encode_value(&decoded, Limits::default()).unwrap(), bytes);
                admitted += 1;
            }
            Err(_) => rejected += 1,
        }
    }
    assert_eq!(admitted, 22);
    assert_eq!(rejected, 7);
}

#[test]
fn recursive_named_records_and_duration_component_forms() {
    let value = Value::Record(Box::new(crate::Record::Open(smallvec::smallvec![
        (
            crate::db_string("later").unwrap(),
            Value::List(vec![Value::Null, Value::Uint128(u128::MAX)])
        ),
        (
            crate::db_string("first").unwrap(),
            Value::Float32(f32::from_bits(1))
        ),
    ])));
    let value = StoredValue::try_from(value).unwrap();
    let bytes = encode_value(&value, Limits::default()).unwrap();
    assert_eq!(decode_value(&bytes, Limits::default()).unwrap(), value);
    let span = jiff::Span::new()
        .years(1)
        .months(13)
        .weeks(2)
        .days(8)
        .hours(25)
        .minutes(61)
        .seconds(62)
        .milliseconds(1001)
        .microseconds(1002)
        .nanoseconds(1003);
    let value = StoredValue::try_from(Value::Duration(Box::new(span))).unwrap();
    let bytes = encode_value(&value, Limits::default()).unwrap();
    let decoded = decode_value(&bytes, Limits::default()).unwrap();
    assert_eq!(encode_value(&decoded, Limits::default()).unwrap(), bytes);
    assert_eq!(bytes.len(), 81);
}

#[test]
fn unknown_tags_duplicates_noncanonical_and_cumulative_limits_reject() {
    for bytes in [&[1, 2][..], &[255], &[0, 0], &[11, 255, 255, 255, 255]] {
        assert!(decode_value(bytes, Limits::default()).is_err());
    }
    // Two same-named record fields, each NULL. Header count must not deduplicate.
    let duplicate = [12, 2, 0, 0, 0, 1, 0, 0, 0, b'a', 0, 1, 0, 0, 0, b'a', 0];
    assert_eq!(
        decode_value(&duplicate, Limits::default()).unwrap_err(),
        CodecError::Invalid("duplicate record field")
    );
    let value = StoredValue::try_from(Value::List(vec![Value::Null; 20])).unwrap();
    let bytes = encode_value(&value, Limits::default()).unwrap();
    let limits = Limits {
        allocation: 2048,
        ..Limits::default()
    };
    assert_eq!(decode_value(&bytes, limits).unwrap_err(), CodecError::Limit);
    assert_eq!(encode_value(&value, limits).unwrap_err(), CodecError::Limit);
    let mut depth = vec![];
    for _ in 0..256 {
        depth.extend([11, 1, 0, 0, 0]);
    }
    depth.push(0);
    assert_eq!(
        decode_value(&depth, Limits::default()).unwrap_err(),
        CodecError::Limit
    );
    let mut decimal = vec![8];
    decimal.extend([0; 16]);
    decimal[1] = 1;
    assert_eq!(
        decode_value(&decimal, Limits::default()).unwrap_err(),
        CodecError::Invalid("decimal flags")
    );
    let json = b"{\"z\":1,\"a\":2}";
    let mut noncanonical = vec![15];
    noncanonical.extend((json.len() as u32).to_le_bytes());
    noncanonical.extend(json);
    assert_eq!(
        decode_value(&noncanonical, Limits::default()).unwrap_err(),
        CodecError::Invalid("JSON canonical text")
    );
}

#[test]
fn maximum_stored_depth_is_supported_without_native_stack_growth() {
    let mut value = Value::Null;
    for _ in 1..256 {
        value = Value::List(vec![value]);
    }
    let value = StoredValue::try_from(value).unwrap();
    let bytes = encode_value(&value, Limits::default()).unwrap();
    let decoded = decode_value(&bytes, Limits::default()).unwrap();
    assert_eq!(encode_value(&decoded, Limits::default()).unwrap(), bytes);
}

#[test]
fn independent_overdeep_schema_bytes_reject_within_the_metadata_budget() {
    let mut bytes = Vec::new();
    bytes.extend([1, 0, 0, 0, b'g']);
    bytes.extend(1u32.to_le_bytes()); // one node type
    bytes.extend([1, 0, 0, 0, b'N']);
    bytes.extend([1, 0, 0, 0, 1, 0, 0, 0, b'L']); // one label
    bytes.extend([1, 0, 0, 0, 1, 0, 0, 0, b'p']); // one property
    for _ in 0..256 {
        bytes.extend([0, 0, 0, 0, 1]);
    } // nested LIST type wrapper
    bytes.extend([1, 1, 0, 0, 0, 0, 0, 0]); // BOOL leaf
    for _ in 0..256 {
        bytes.extend([0, 0]);
    } // nullability/cardinality
    bytes.extend([1, 0, 0, 0, 0, 0]); // property flags and node mode
    bytes.extend(0u32.to_le_bytes()); // no edge types
    let mut budget = Budget::new(Limits::default()).unwrap();
    let mut decoder = Decoder::new(&bytes, &mut budget).unwrap();
    assert_eq!(decoder.graph_definition().unwrap_err(), CodecError::Limit);
}

#[test]
fn allocation_budget_accounts_for_large_resident_change_carriers_before_reserve() {
    let count = 32;
    assert!(std::mem::size_of::<crate::Change>() > 256);
    let delta = GraphDelta {
        id: crate::GraphId::new(1),
        previous: Some(1),
        generation: 2,
        next_node_id: 33,
        next_edge_id: 1,
        definition: None,
        backing_indexes: vec![],
        changes: (1..=count)
            .map(|id| crate::Change::NodeDeleted {
                id: crate::NodeId::new(id as u64),
            })
            .collect(),
    };
    let mut e = Encoder::new(Limits::default()).unwrap();
    delta.encode(&mut e).unwrap();
    let bytes = e.finish();
    // The wire is small, but reserving even one full Vec<Change> would exceed this
    // allocation budget. A flat 256-byte-per-entry charge would falsely admit it.
    let limits = Limits {
        allocation: count * std::mem::size_of::<crate::Change>() - 1,
        ..Limits::default()
    };
    let mut budget = Budget::new(limits).unwrap();
    let mut d = Decoder::new(&bytes, &mut budget).unwrap();
    assert_eq!(GraphDelta::decode(&mut d).unwrap_err(), CodecError::Limit);
    assert_eq!(
        delta
            .encode(&mut Encoder::new(limits).unwrap())
            .unwrap_err(),
        CodecError::Limit
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn bounded_nested_values_and_all_partial_inputs(values in prop::collection::vec((any::<i128>(), any::<u128>(), any::<u32>()), 0..24)) {
        let value = StoredValue::try_from(Value::List(values.into_iter().map(|(a,b,c)|
            Value::List(vec![Value::Int128(a), Value::Uint128(b), Value::Float32(f32::from_bits(c))])).collect())).unwrap();
        let bytes = encode_value(&value, Limits::default()).unwrap();
        prop_assert_eq!(decode_value(&bytes, Limits::default()).unwrap(), value);
        for cut in 0..bytes.len() { prop_assert!(decode_value(&bytes[..cut], Limits::default()).is_err()); }
    }
}
