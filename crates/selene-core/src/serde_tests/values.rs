use std::sync::Arc;

use proptest::prelude::*;
use smallvec::smallvec;

use super::{dbs, decode_map, encode_map, rt, rt_value};
use crate::*;

fn all_values() -> Vec<Value> {
    let path = Path {
        graph: GraphId::new(1),
        start: NodeId::new(1),
        segments: smallvec![PathSegment {
            edge: EdgeId::new(1),
            direction: EdgeDirection::Outgoing,
            node: NodeId::new(2),
        }],
    };
    vec![
        Value::Bool(true),
        Value::Int(-1),
        Value::Uint(1),
        Value::Int128(-2),
        Value::Uint128(2),
        Value::Float(1.5),
        Value::Float32(2.5),
        Value::Decimal(rust_decimal::Decimal::new(1234, 2)),
        Value::String(dbs("serde.value.string")),
        Value::Bytes(Arc::from([1_u8, 2, 3])),
        Value::List(vec![Value::Int(1), Value::Null]),
        Value::Record(Box::new(Record::Open(smallvec![(
            dbs("serde.record.open"),
            Value::Bool(false),
        )]))),
        Value::RecordTyped(Box::new(RecordTyped {
            type_id: RecordTypeId::new(1),
            values: smallvec![Some(Value::String(dbs("serde.record.typed"))), None],
        })),
        Value::Path(Box::new(path)),
        Value::NodeRef(NodeId::new(1)),
        Value::EdgeRef(EdgeId::new(1)),
        Value::GraphRef(GraphId::new(1)),
        Value::TableRef(BindingTableId::new(1)),
        Value::ZonedDateTime(Box::new(
            "2026-05-07T12:34:56-04:00[America/New_York]"
                .parse()
                .unwrap(),
        )),
        Value::LocalDateTime("2026-05-07T12:34:56".parse().unwrap()),
        Value::Date("2026-05-07".parse().unwrap()),
        Value::ZonedTime(Box::new(
            "2026-05-07T12:34:56-04:00[America/New_York]"
                .parse()
                .unwrap(),
        )),
        Value::LocalTime("12:34:56".parse().unwrap()),
        Value::Duration(Box::new("PT1H2S".parse().unwrap())),
        Value::Extended {
            type_id: ExtensionTypeId(0x100),
            payload: Arc::from([1_u8, 2, 3, 4]),
        },
        Value::Null,
        Value::Uuid(uuid::Uuid::nil()),
        Value::Vector(VectorValue::new(vec![1.0, 2.0, 3.0]).unwrap()),
        Value::Json(JsonValue::new(serde_json::json!({"serde": ["json", 1]})).unwrap()),
    ]
}

#[test]
fn stored_value_format2_round_trip_and_query_rejection() {
    for value in all_values() {
        if matches!(
            value,
            Value::RecordTyped(_)
                | Value::Path(_)
                | Value::NodeRef(_)
                | Value::EdgeRef(_)
                | Value::GraphRef(_)
                | Value::TableRef(_)
                | Value::Extended { .. }
        ) {
            assert!(StoredValue::try_from(value).is_err());
        } else {
            rt_value(&value);
        }
    }
}

#[test]
fn property_map_format2_round_trip_omits_compact_layout() {
    let standard = PropertyMap::from_pairs([
        (dbs("serde.pm.a"), Value::Int(1)),
        (dbs("serde.pm.b"), Value::Null),
    ])
    .unwrap();
    assert_eq!(
        decode_map(&encode_map(&standard).unwrap()).unwrap(),
        standard
    );

    let compact = PropertyMap::compact(
        [dbs("serde.pm.a"), dbs("serde.pm.b")],
        [Some(Value::Int(1)), None],
    )
    .unwrap();
    let round = decode_map(&encode_map(&compact).unwrap()).unwrap();
    assert_eq!(
        round.iter().collect::<Vec<_>>(),
        compact.iter().collect::<Vec<_>>()
    );
}

#[test]
fn label_set_postcard_round_trip() {
    rt(&LabelSet::from_iter([
        dbs("serde.label.b"),
        dbs("serde.label.a"),
    ]));
}

#[test]
fn db_string_postcard_round_trips() {
    let value = dbs("serde.db_string.canonical");
    let bytes = postcard::to_allocvec(&value).unwrap();
    let decoded: DbString = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.as_str(), "serde.db_string.canonical");
}

#[test]
fn db_string_deserialize_from_str_preserves_content() {
    let bytes = postcard::to_allocvec("serde.db_string.fresh").unwrap();
    let decoded: DbString = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.as_str(), "serde.db_string.fresh");
}

#[test]
fn extended_value_payload_is_not_storable() {
    let value = Value::Extended {
        type_id: ExtensionTypeId(0x100),
        payload: Arc::from([1_u8, 2, 3, 4]),
    };
    assert!(StoredValue::try_from(value).is_err());
}

#[test]
fn vector_value_postcard_rejects_invalid_payloads() {
    let empty = postcard::to_allocvec(&Vec::<f32>::new()).unwrap();
    assert!(postcard::from_bytes::<VectorValue>(&empty).is_err());

    let non_finite = postcard::to_allocvec(&vec![1.0_f32, f32::INFINITY]).unwrap();
    assert!(postcard::from_bytes::<VectorValue>(&non_finite).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn simple_values_format2_round_trip(value in simple_value_strategy()) {
        rt_value(&value);
    }
}

fn simple_value_strategy() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int),
        any::<u64>().prop_map(Value::Uint),
        any::<f64>()
            .prop_filter("finite", |value| value.is_finite())
            .prop_map(Value::Float),
        any::<f32>()
            .prop_filter("finite", |value| value.is_finite())
            .prop_map(Value::Float32),
        (0_u8..32).prop_map(|idx| {
            let name = format!("serde.prop.string.{idx}");
            Value::String(dbs(&name))
        }),
        proptest::collection::vec(any::<u8>(), 0..32)
            .prop_map(|bytes| Value::Bytes(Arc::from(bytes))),
        proptest::collection::vec(
            any::<f32>().prop_filter("finite", |value| value.is_finite()),
            1..32,
        )
        .prop_map(|components| Value::Vector(VectorValue::new(components).unwrap())),
        any::<u128>().prop_map(Value::Uint128),
        any::<i128>().prop_map(Value::Int128),
        Just(Value::Null),
        any::<u128>().prop_map(|raw| Value::Uuid(uuid::Uuid::from_u128(raw))),
    ]
}
