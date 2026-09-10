//! Independent bounded rational fixtures and selected numeric family coverage.

use std::{
    cmp::Ordering,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use proptest::prelude::*;
use rust_decimal::Decimal;

use crate::{NumericKey, Value};

fn key(value: Value) -> NumericKey {
    NumericKey::of(&value).unwrap()
}
fn hash(value: NumericKey) -> u64 {
    let mut state = DefaultHasher::new();
    value.hash(&mut state);
    state.finish()
}

#[test]
fn selected_numeric_inventory_has_exactly_seven_representations() {
    let numbers: Vec<_> = Value::ALL
        .iter()
        .map(|make| make())
        .filter(Value::is_number)
        .collect();
    assert_eq!(numbers.len(), 7);
    let zero = key(Value::Int(0));
    for value in numbers {
        let other = key(value);
        assert_eq!(zero, other);
        assert_eq!(hash(zero), hash(other));
    }
    for value in Value::ALL.iter().map(|make| make()) {
        assert_eq!(NumericKey::of(&value).is_some(), value.is_number());
    }
}

#[test]
fn zero_nan_and_infinity_policies_are_operation_specific() {
    for zero in [
        Value::Float(-0.0),
        Value::Float32(-0.0),
        Value::Decimal(Decimal::NEGATIVE_ONE * Decimal::ZERO),
    ] {
        assert_eq!(key(zero).sort_cmp(key(Value::Int(0))), Ordering::Equal);
    }
    let nans = [f64::NAN, f64::from_bits(0xfff8_0000_0000_0001)];
    for nan in nans {
        let nan = key(Value::Float(nan));
        let other = key(Value::Float32(f32::NAN));
        assert_eq!(nan, other);
        assert_eq!(hash(nan), hash(other));
        assert_eq!(nan.predicate_cmp(other), None);
        assert_eq!(
            nan.sort_cmp(key(Value::Float(f64::INFINITY))),
            Ordering::Greater
        );
    }
    assert_eq!(
        key(Value::Float(f64::NEG_INFINITY)).predicate_cmp(key(Value::Int128(i128::MIN))),
        Some(Ordering::Less)
    );
    assert_eq!(
        key(Value::Float32(f32::INFINITY)),
        key(Value::Float(f64::INFINITY))
    );
}

#[test]
fn independently_bracketed_decimal_binary_boundaries() {
    let cases = [
        (Decimal::ZERO, f64::from_bits(1), Ordering::Less),
        (Decimal::ZERO, -f64::from_bits(1), Ordering::Greater),
        (Decimal::MAX, 2_f64.powi(96), Ordering::Less),
        (Decimal::MIN, -2_f64.powi(96), Ordering::Greater),
        (Decimal::new(1, 28), f64::MIN_POSITIVE, Ordering::Greater),
        (
            "0.1000000000000000055511151231".parse().unwrap(),
            0.1,
            Ordering::Less,
        ),
        (
            "0.1000000000000000055511151232".parse().unwrap(),
            0.1,
            Ordering::Greater,
        ),
    ];
    for (decimal, binary, expected) in cases {
        let a = key(Value::Decimal(decimal));
        let b = key(Value::Float(binary));
        assert_eq!(a.predicate_cmp(b), Some(expected), "{decimal} vs {binary}");
        assert_eq!(b.predicate_cmp(a), Some(expected.reverse()));
        assert_ne!(a, b);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn decimal_vs_binary_matches_independent_integer_cross_products(
        decimal in -1_000_000_000_i64..1_000_000_000,
        binary in -1_000_000_000_i64..1_000_000_000,
        decimal_scale in 0_u32..10,
        binary_scale in 0_i32..30,
    ) {
        // Both cross-products fit i128. The oracle does not decompose IEEE
        // bits or use any canonical-key/reduction helper from the implementation.
        let left = i128::from(decimal) * (1_i128 << binary_scale);
        let right = i128::from(binary) * 10_i128.pow(decimal_scale);
        let a = key(Value::Decimal(Decimal::new(decimal, decimal_scale)));
        let b = key(Value::Float(binary as f64 / 2_f64.powi(binary_scale)));
        prop_assert_eq!(a.predicate_cmp(b), Some(left.cmp(&right)));
        prop_assert_eq!(a == b, left == right);
        if a == b { prop_assert_eq!(hash(a), hash(b)); }
    }

    #[test]
    fn arbitrary_u128_values_order_exactly(a in any::<u128>(), b in any::<u128>()) {
        prop_assert_eq!(key(Value::Uint128(a)).predicate_cmp(key(Value::Uint128(b))), Some(a.cmp(&b)));
    }

    #[test]
    fn arbitrary_binary_values_match_ieee_order(a in any::<f64>(), b in any::<f64>()) {
        prop_assert_eq!(key(Value::Float(a)).predicate_cmp(key(Value::Float(b))), a.partial_cmp(&b));
    }

    #[test]
    fn decimal_order_matches_decimal_arithmetic(a in any::<i64>(), b in any::<i64>(), sa in 0_u32..29, sb in 0_u32..29) {
        let a = Decimal::new(a, sa);
        let b = Decimal::new(b, sb);
        prop_assert_eq!(key(Value::Decimal(a)).predicate_cmp(key(Value::Decimal(b))), Some(a.cmp(&b)));
    }

    #[test]
    fn equivalent_dyadic_representations_share_hash(mantissa in -1_000_000_i64..1_000_000, scale in 0_u32..20) {
        let decimal = Decimal::from_i128_with_scale(i128::from(mantissa) * 5_i128.pow(scale), scale);
        let binary = mantissa as f64 / 2_f64.powi(scale as i32);
        let a = key(Value::Decimal(decimal));
        let b = key(Value::Float(binary));
        prop_assert_eq!(a, b);
        prop_assert_eq!(hash(a), hash(b));
    }
}
