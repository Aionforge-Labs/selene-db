//! Pure descriptor fixtures, independent of source syntax and engine catalogs.

use super::*;
use crate::{Record, Value, db_string};

#[test]
fn assignment_is_distinct_from_membership_and_handles_structural_containers() {
    let open = StructuralType::new(TypeKind::Record(None), true).unwrap();
    let closed =
        StructuralType::record([(db_string("a").unwrap(), StructuralType::INT64)]).unwrap();
    assert!(open.assignment_compatible(&closed));
    assert!(!closed.assignment_compatible(&open));
    assert!(StructuralType::FLOAT64.assignment_compatible(&StructuralType::INT64));
    assert!(
        !StructuralType::FLOAT64.matches(&Value::Int(1)),
        "membership does not silently perform assignment conversion"
    );
    assert!(
        !StructuralType::INT64
            .with_nullability(false)
            .assignment_compatible(&StructuralType::NULL)
    );
    assert!(
        StructuralType::list(open, None)
            .unwrap()
            .assignment_compatible(&StructuralType::list(closed, Some(3)).unwrap())
    );
}
use proptest::prelude::*;

#[test]
fn descriptors_release_shared_fields_without_an_immortal_pool() {
    let fields = Arc::from([(db_string("field").unwrap(), StructuralType::INT64)]);
    let ty = StructuralType::new(TypeKind::Record(Some(fields)), true).unwrap();
    let TypeKind::Record(Some(fields)) = ty.kind() else {
        unreachable!();
    };
    let weak = Arc::downgrade(fields);
    let clone = ty.clone();
    drop(ty);
    assert!(weak.upgrade().is_some());
    drop(clone);
    assert!(weak.upgrade().is_none());
}

#[test]
fn query_membership_inventory_never_admits_opaque_or_positional_legacy_values() {
    let open = StructuralType::new(TypeKind::Record(None), true).unwrap();
    for make in Value::ALL {
        let value = make();
        let supported = !matches!(value, Value::RecordTyped(_) | Value::Extended { .. });
        assert_eq!(
            StructuralType::DYNAMIC.matches(&value),
            supported,
            "{}",
            value.variant_name()
        );
        let nested = Value::Record(Box::new(Record::Open(
            [(db_string("nested").unwrap(), Value::List(vec![value]))]
                .into_iter()
                .collect(),
        )));
        assert_eq!(open.matches(&nested), supported);
    }
}

#[test]
fn query_membership_rejects_excessive_runtime_depth_before_recursing() {
    std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(|| {
            let mut value = Value::Int(1);
            for _ in 0..4096 {
                value = Value::List(vec![value]);
            }
            assert!(!StructuralType::DYNAMIC.matches(&value));
            // Tear down the adversarial fixture iteratively as well.
            while let Value::List(mut values) = value {
                value = values.pop().unwrap();
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn duplicate_fields_bad_bounds_and_excessive_depth_are_rejected() {
    assert!(StructuralType::list(StructuralType::INT64, Some(0)).is_err());
    let name = db_string("a").unwrap();
    assert!(matches!(
        StructuralType::record([
            (name.clone(), StructuralType::INT64),
            (name, StructuralType::STRING)
        ]),
        Err(StructuralTypeError::DuplicateField(_))
    ));
    assert!(
        StructuralType::from_scalar(ScalarType::String(Some(CharacterStringType {
            min_len: 9,
            max_len: 2
        })))
        .is_err()
    );
    let mut ty = StructuralType::INT64;
    for _ in 1..MAX_STRUCTURAL_TYPE_DEPTH {
        ty = StructuralType::list(ty, None).unwrap();
    }
    assert_eq!(ty.depth(), MAX_STRUCTURAL_TYPE_DEPTH);
    assert!(matches!(
        StructuralType::list(ty, None),
        Err(StructuralTypeError::DepthLimit)
    ));
}

proptest! {
    #[test]
    fn field_permutations_have_one_type_and_membership_model(a in any::<i64>(), b in any::<bool>(), reversed in any::<bool>(), nullable in any::<bool>()) {
        let an = db_string("a").unwrap();
        let bn = db_string("b").unwrap();
        let av = StructuralType::INT64.with_nullability(nullable);
        let expected = StructuralType::record([(an.clone(), av.clone()), (bn.clone(), StructuralType::BOOLEAN)]).unwrap();
        let fields = if reversed { vec![(bn.clone(), StructuralType::BOOLEAN), (an.clone(), av)] } else { vec![(an.clone(), av), (bn.clone(), StructuralType::BOOLEAN)] };
        let actual = StructuralType::record(fields).unwrap();
        prop_assert_eq!(&actual, &expected);
        let record = |number| Value::Record(Box::new(Record::Open([(bn.clone(), Value::Bool(b)), (an.clone(), number)].into_iter().collect())));
        prop_assert!(actual.matches(&record(Value::Int(a))));
        prop_assert_eq!(actual.matches(&record(Value::Null)), nullable);
        prop_assert!(!actual.matches(&record(Value::Bool(false))));
    }
}
