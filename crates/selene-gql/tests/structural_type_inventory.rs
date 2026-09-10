//! Generated selected-family inventory and independent semantic fixtures.

use selene_core::{ScalarType as S, StructuralType as T, TypeKind as K, Value, db_string};
use selene_gql::{GqlType as G, lower_value_type, normalize_value_type};
use selene_profile::{CapabilityStatus, capabilities};

#[test]
fn every_runtime_supported_value_feature_has_a_structural_fixture() {
    let mut checked = Vec::new();
    for capability in capabilities()
        .iter()
        .filter(|c| c.status == CapabilityStatus::Supported && c.id.as_str().starts_with("GV"))
    {
        let source = match capability.id.as_str() {
            "GV01" => G::Uint8,
            "GV02" => G::Int8,
            "GV03" | "GV05" => G::Uint16,
            "GV04" | "GV18" => G::Int16,
            "GV06" | "GV08" => G::Uint32,
            "GV07" => G::Int32,
            "GV09" | "GV12" | "GV19" => G::Int64,
            "GV10" | "GV11" => G::Uint64,
            "GV13" => G::Uint128,
            "GV14" => G::Int128,
            "GV17" => G::DecimalExact(selene_core::DecimalType::new(12, 3).unwrap()),
            "GV21" | "GV22" => G::Float32,
            "GV23" => G::Double,
            "GV24" => G::Float64,
            "GV30" | "GV31" | "GV32" => G::CharacterString(
                selene_gql::ast::types::CharacterStringType::new(
                    2,
                    8,
                    selene_gql::ast::types::CharacterStringTypeForm::StringMinMax,
                )
                .unwrap(),
            ),
            "GV35" => G::Bytes,
            "GV36" | "GV37" | "GV38" => G::ByteString(
                selene_gql::ast::types::ByteStringType::new(
                    2,
                    8,
                    selene_gql::ast::types::ByteStringTypeForm::BytesMinMax,
                )
                .unwrap(),
            ),
            "GV39" => G::LocalDateTime,
            "GV40" => G::ZonedDateTime,
            "GV41" => G::DurationDayToSecond,
            "GV45" | "GV47" => G::Record(selene_gql::RecordType::Open),
            "GV46" => G::Record(selene_gql::RecordType::Closed(vec![(
                db_string("a").unwrap(),
                G::Int64,
            )])),
            "GV48" => G::Record(selene_gql::RecordType::Closed(vec![(
                db_string("a").unwrap(),
                G::Record(selene_gql::RecordType::Open),
            )])),
            "GV50" => G::BoundedList {
                element_type: Box::new(G::Integer),
                max_len: 8,
            },
            "GV55" => G::Path,
            "GV68" => G::AnyProperty,
            "GV90" => G::NotNull(Box::new(G::Integer)),
            other => panic!("selected family {other} lacks a structural fixture"),
        };
        let normalized = normalize_value_type(&source).unwrap();
        assert_eq!(
            normalize_value_type(&lower_value_type(&normalized)).unwrap(),
            normalized
        );
        checked.push(capability.id);
    }
    assert!(!checked.is_empty());
    // Inventory round trips above guard coverage, not independent correctness.
    for (source, expected) in [
        (G::Integer, T::INT64),
        (G::BigInt, T::INT64),
        (G::Real, T::from_scalar(S::Float32).unwrap()),
        (G::Double, T::FLOAT64),
    ] {
        assert_eq!(normalize_value_type(&source).unwrap(), expected);
    }
}

#[test]
fn admitted_union_descriptors_retain_components_without_any_widening() {
    for id in ["GV66", "GV67"] {
        assert!(
            selene_profile::DIRECT_SELECTED_FEATURES
                .iter()
                .any(|feature| feature.as_str() == id)
        );
        assert_ne!(
            selene_profile::capability_by_id(id).unwrap().status,
            CapabilityStatus::Supported,
            "bounded existing union behavior is not a complete-capability claim"
        );
    }
    let union = normalize_value_type(&G::ClosedDynamicUnion(vec![G::Integer, G::String])).unwrap();
    assert_eq!(
        union,
        normalize_value_type(&G::ClosedDynamicUnion(vec![
            G::String,
            G::Int64,
            G::Integer
        ]))
        .unwrap()
    );
    assert!(union.matches(&Value::Int(1)));
    assert!(union.matches(&Value::Null));
    assert!(!union.matches(&Value::Bool(true)));
    assert_ne!(union, T::DYNAMIC);
    const {
        assert!(!selene_profile::RELEASE_CLAIMABLE);
    }
    for source in ["RETURN 1 IS TYPED NULL", "RETURN 1 IS TYPED NOTHING"] {
        assert!(
            selene_gql::parse(source).is_err(),
            "unsupported immaterial type syntax must not bypass feature admission: {source}"
        );
    }
    assert!(
        selene_gql::parse("RETURN NULL").is_ok(),
        "the null value itself is not optional null-type syntax"
    );
}

#[test]
fn record_literal_inference_retains_nested_semantic_fields_without_rows() {
    let analyzed = selene_gql::analyze(
        selene_gql::parse("RETURN RECORD{items: [RECORD{value: 7}]} AS item").unwrap(),
        &selene_gql::EmptyProcedureRegistry,
        None,
    )
    .unwrap();
    let expected = T::record([(
        db_string("items").unwrap(),
        T::list(
            T::record([(db_string("value").unwrap(), T::INT64)]).unwrap(),
            None,
        )
        .unwrap(),
    )])
    .unwrap();
    assert!(
        analyzed
            .expr_types
            .iter()
            .any(|(id, _)| analyzed.expr_types.structural_type(id) == &expected)
    );
    assert_eq!(
        normalize_value_type(&G::NotNull(Box::new(G::NotNull(Box::new(G::Integer))))).unwrap(),
        T::INT64.with_nullability(false)
    );
    assert!(
        !T::list(T::INT64.with_nullability(false), None)
            .unwrap()
            .matches(&Value::List(vec![Value::Null]))
    );
    assert!(
        T::new(K::Record(None), true)
            .unwrap()
            .is_storable_descriptor()
    );
}
