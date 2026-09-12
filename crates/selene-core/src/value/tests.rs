use std::sync::Arc;

use proptest::prelude::*;
use smallvec::SmallVec;

use super::*;
use crate::{PropertyMap, db_string};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn value_is_send_sync() {
    assert_send_sync::<Value>();
}

#[test]
fn representative_variants_clone_and_compare() {
    let values = [
        Value::Bool(true),
        Value::Int(42),
        Value::String(db_string("name").unwrap()),
        Value::Bytes(Arc::from([1_u8, 2, 3])),
        Value::NodeRef(NodeId::new(1)),
        Value::Null,
        Value::Uuid(uuid::Uuid::nil()),
        Value::Vector(VectorValue::new(vec![1.0, 2.0]).unwrap()),
        Value::Json(JsonValue::new(serde_json::json!({"ok": true})).unwrap()),
    ];
    for value in values {
        assert_eq!(value.clone(), value);
    }
}

#[test]
fn edge_direction_variants_are_distinct() {
    assert_ne!(EdgeDirection::Outgoing, EdgeDirection::Incoming);
    assert_ne!(EdgeDirection::Outgoing, EdgeDirection::Undirected);
    assert_ne!(EdgeDirection::Incoming, EdgeDirection::Undirected);
}

#[test]
fn path_clone_round_trips() {
    let mut segments = SmallVec::new();
    segments.push(PathSegment {
        edge: EdgeId::new(3),
        direction: EdgeDirection::Outgoing,
        node: NodeId::new(4),
    });
    let path = Path {
        graph: GraphId::new(1),
        start: NodeId::new(2),
        segments,
    };
    assert_eq!(path.clone(), path);
}

#[test]
fn value_discriminant_size_is_stable_on_this_target() {
    assert!(std::mem::size_of::<Value>() >= std::mem::size_of::<usize>());
}

#[test]
fn value_all_covers_every_variant() {
    assert_eq!(Value::VARIANT_COUNT, 29);
    let mut discriminants = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for factory in Value::ALL {
        let value = factory();
        assert!(
            discriminants.insert(std::mem::discriminant(&value)),
            "Value::ALL has duplicate variant: {}",
            value.variant_name()
        );
        let name = value.variant_name();
        assert!(!name.is_empty(), "Value::variant_name must not be empty");
        assert!(names.insert(name), "Value::variant_name collision: {name}");
    }
    assert_eq!(discriminants.len(), Value::ALL.len());
    assert_eq!(names.len(), Value::ALL.len());
}

#[test]
fn value_float_nan_eq_bit_exact() {
    assert_eq!(Value::Float(f64::NAN), Value::Float(f64::NAN));
}

#[test]
fn value_float32_nan_eq_bit_exact() {
    assert_eq!(Value::Float32(f32::NAN), Value::Float32(f32::NAN));
}

#[test]
fn value_float_signed_zero_eq_preserved() {
    assert_eq!(Value::Float(0.0), Value::Float(-0.0));
}

#[test]
fn value_property_map_round_trip_nan() {
    let original = PropertyMap::from_pairs([(db_string("x").unwrap(), Value::Float(f64::NAN))])
        .expect("property map builds");
    let bytes = crate::serde_tests::encode_map(&original).expect("format2 properties encode");
    let decoded = crate::serde_tests::decode_map(&bytes).expect("format2 properties decode");

    assert_eq!(original, decoded);
}

#[test]
fn vector_value_exposes_validated_components() {
    let vector = VectorValue::new(vec![1.0, -0.0, 3.5]).unwrap();
    assert_eq!(vector.dimension(), 3);
    assert_eq!(vector.as_slice(), &[1.0, -0.0, 3.5]);
    assert_eq!(vector.as_arc().as_ref(), &[1.0, -0.0, 3.5]);
}

#[test]
fn vector_value_rejects_empty_payload() {
    assert!(matches!(
        VectorValue::new(Vec::<f32>::new()),
        Err(CoreError::VectorEmpty)
    ));
}

#[test]
fn vector_value_rejects_non_finite_component() {
    assert!(matches!(
        VectorValue::new(vec![1.0, f32::NAN]),
        Err(CoreError::VectorComponentNotFinite { index: 1, .. })
    ));
    assert!(matches!(
        VectorValue::new(vec![f32::NEG_INFINITY]),
        Err(CoreError::VectorComponentNotFinite { index: 0, .. })
    ));
}

#[test]
fn vector_value_rejects_overlarge_dimension() {
    let components = vec![0.0; MAX_VECTOR_DIMENSION + 1];
    assert!(matches!(
        VectorValue::new(components),
        Err(CoreError::VectorTooLarge {
            got,
            max: MAX_VECTOR_DIMENSION
        }) if got == MAX_VECTOR_DIMENSION + 1
    ));
}

#[test]
fn json_value_exposes_canonical_rendering_and_type() {
    let json = JsonValue::new(serde_json::json!({"b": [2, true], "a": null})).unwrap();
    assert_eq!(json.json_type_name(), "object");
    assert_eq!(json.to_canonical_string(), r#"{"a":null,"b":[2,true]}"#);
    assert_eq!(
        json.as_serde()
            .as_object()
            .expect("fixture is object")
            .get("b"),
        Some(&serde_json::json!([2, true]))
    );
}

proptest! {
    #[test]
    fn random_short_path_clones(segment_count in 0_usize..=4) {
        let mut segments = SmallVec::<[PathSegment; 4]>::new();
        for idx in 0..segment_count {
            segments.push(PathSegment {
                edge: EdgeId::new(idx as u64 + 1),
                direction: EdgeDirection::Outgoing,
                node: NodeId::new(idx as u64 + 2),
            });
        }
        let path = Path {
            graph: GraphId::new(1),
            start: NodeId::new(1),
            segments,
        };
        prop_assert_eq!(path.clone(), path);
    }
}
