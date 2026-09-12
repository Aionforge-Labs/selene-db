use super::*;
use selene_core::{Record, VectorValue, db_string};

fn key(value: &Value) -> Result<Vec<u8>, E> {
    let mut bytes = Vec::new();
    write(value, &mut bytes, 1)?;
    Ok(bytes)
}
fn text(value: &str) -> Value {
    Value::String(db_string(value).unwrap())
}
fn record(fields: &[(&str, Value)]) -> Value {
    Value::Record(Box::new(Record::Open(
        fields
            .iter()
            .map(|(n, v)| (db_string(n).unwrap(), v.clone()))
            .collect(),
    )))
}

#[test]
fn independent_semantic_pairs_share_keys_only_when_equal() {
    let equal = [
        (Value::Int(1), Value::Float32(1.0)),
        (
            Value::Uint128(1),
            Value::Decimal(rust_decimal::Decimal::new(100, 2)),
        ),
        (Value::Float(-0.0), Value::Float32(0.0)),
        (
            Value::Float(f64::NAN),
            Value::Float(f64::from_bits(0x7ff8_0000_0000_0011)),
        ),
        (
            Value::Duration(Box::new("PT60S".parse().unwrap())),
            Value::Duration(Box::new("PT1M".parse().unwrap())),
        ),
        (
            Value::ZonedDateTime(Box::new(
                "2024-01-01T01:00:00+01:00[+01:00]".parse().unwrap(),
            )),
            Value::ZonedDateTime(Box::new("2024-01-01T00:00:00Z[UTC]".parse().unwrap())),
        ),
        (
            record(&[("a", Value::Int(1)), ("b", text("x"))]),
            record(&[("b", text("x")), ("a", Value::Float(1.0))]),
        ),
        (
            Value::Vector(VectorValue::new(vec![-0.0, 1.0]).unwrap()),
            Value::Vector(VectorValue::new(vec![0.0, 1.0]).unwrap()),
        ),
        (
            Value::List(vec![Value::Null, Value::Int(1)]),
            Value::List(vec![Value::Null, Value::Float(1.0)]),
        ),
    ];
    for (a, b) in equal {
        assert_eq!(key(&a).unwrap(), key(&b).unwrap(), "{a:?}, {b:?}");
    }
    let distinct = [
        (Value::Bool(true), Value::Int(1)),
        (Value::Null, Value::List(vec![])),
        (
            Value::List(vec![text("ab"), text("c")]),
            Value::List(vec![text("a"), text("bc")]),
        ),
        (record(&[("ab", text("c"))]), record(&[("a", text("bc"))])),
        (
            Value::Date("2024-01-01".parse().unwrap()),
            Value::LocalDateTime("2024-01-01T00:00:00".parse().unwrap()),
        ),
        (
            Value::LocalTime("01:02:03.000000001".parse().unwrap()),
            Value::LocalTime("01:02:03.000000002".parse().unwrap()),
        ),
        (
            Value::Uuid(uuid::Uuid::nil()),
            Value::Bytes(vec![0; 16].into()),
        ),
        (
            Value::Duration(Box::new("P1M".parse().unwrap())),
            Value::Duration(Box::new("P30D".parse().unwrap())),
        ),
    ];
    for (a, b) in distinct {
        assert_ne!(key(&a).unwrap(), key(&b).unwrap(), "{a:?}, {b:?}");
    }
}

#[test]
fn query_unsupported_and_deep_values_return_typed_errors_before_domain_observation() {
    for value in [
        Value::NodeRef(selene_core::NodeId::new(1)),
        Value::EdgeRef(selene_core::EdgeId::new(1)),
        Value::GraphRef(selene_core::GraphId::new(1)),
        Value::TableRef(selene_core::BindingTableId::new(1)),
        Value::Duration(Box::new("P1MT1S".parse().unwrap())),
    ] {
        assert_eq!(key(&value), Err(E::NotComparable));
        assert_eq!(key(&Value::List(vec![value])), Err(E::NotComparable));
    }
    let mut value = Value::Null;
    for _ in 0..selene_core::MAX_STRUCTURAL_TYPE_DEPTH {
        value = Value::List(vec![value]);
    }
    assert_eq!(key(&value), Err(E::TooDeep));
    assert_eq!(
        key(&record(&[("x", Value::Null), ("x", Value::Null)])),
        Err(E::NotComparable)
    );
}
