//! Canonical key wrappers for runtime DISTINCT, aggregate DISTINCT, and joins.
//!
//! `DistinctRowKey` is variant-strict and follows `Value::PartialEq`.
//! `RuntimeEqKey` follows the runtime row-key comparator used by pattern joins:
//! cross-type numbers use the core's exact numeric grouping keys, and strings
//! compare by contents. Both hash
//! paths normalize signed zero and NaN payloads so any values that compare equal
//! under their key regime hash identically.

use std::{
    hash::{Hash, Hasher},
    mem,
};

use selene_core::{PathSegment, Record, Value};

use super::value_compare;

/// Variant-strict key for structural hash invariant tests.
#[derive(Clone, Debug)]
#[cfg(test)]
pub(crate) struct DistinctRowKey(pub(crate) Vec<Value>);

#[cfg(test)]
impl PartialEq for DistinctRowKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

#[cfg(test)]
impl Eq for DistinctRowKey {}

#[cfg(test)]
impl Hash for DistinctRowKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.len().hash(state);
        for value in &self.0 {
            hash_value_variant_strict(value, state);
        }
    }
}

/// Runtime-equality key for aggregate `DISTINCT` and hash-join key probes.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeEqKey(pub(crate) Vec<Value>);

impl RuntimeEqKey {
    pub(crate) fn from_row(row: Vec<Value>) -> Self {
        Self(row)
    }
}

impl PartialEq for RuntimeEqKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self
                .0
                .iter()
                .zip(&other.0)
                .all(|(lhs, rhs)| runtime_values_equal(lhs, rhs))
    }
}

impl Eq for RuntimeEqKey {}

impl Hash for RuntimeEqKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.len().hash(state);
        for value in &self.0 {
            hash_value_runtime_eq(value, state);
        }
    }
}

fn runtime_values_equal(lhs: &Value, rhs: &Value) -> bool {
    match (lhs, rhs) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => value_compare::equal_non_null(lhs, rhs),
    }
}

fn hash_value_runtime_eq<H: Hasher>(value: &Value, state: &mut H) {
    if hash_runtime_numeric(value, state) {
        return;
    }
    match value {
        Value::String(value) => {
            "runtime-string".hash(state);
            value.as_str().hash(state);
        }
        // Records / lists must recurse through the runtime-eq regime so the
        // field/element values (including cross-type numerics and permuted
        // record fields) hash exactly as `equal_non_null` compares them
        // (GQLRT-14 parity).
        Value::Record(record) => hash_record_runtime_eq(record, state),
        Value::List(values) => {
            mem::discriminant(value).hash(state);
            values.len().hash(state);
            for value in values {
                hash_value_runtime_eq(value, state);
            }
        }
        _ => hash_value_variant_strict(value, state),
    }
}

/// Runtime-eq record hash: field-name-sorted (order-independent) with field
/// values hashed under the runtime-eq regime, matching `record_key_equal`.
fn hash_record_runtime_eq<H: Hasher>(record: &Record, state: &mut H) {
    mem::discriminant(record).hash(state);
    match record {
        Record::Open(fields) => {
            fields.len().hash(state);
            let mut sorted: Vec<&(selene_core::DbString, Value)> = fields.iter().collect();
            sorted.sort_by(|lhs, rhs| lhs.0.as_str().cmp(rhs.0.as_str()));
            for (name, value) in sorted {
                name.as_str().hash(state);
                hash_value_runtime_eq(value, state);
            }
        }
        _ => format!("{record:?}").hash(state),
    }
}

fn hash_value_variant_strict<H: Hasher>(value: &Value, state: &mut H) {
    mem::discriminant(value).hash(state);
    match value {
        Value::Bool(value) => value.hash(state),
        Value::Int(value) => value.hash(state),
        Value::Uint(value) => value.hash(state),
        Value::Int128(value) => value.hash(state),
        Value::Uint128(value) => value.hash(state),
        Value::Float(value) => hash_f64_canonical(*value, state),
        Value::Float32(value) => hash_f32_canonical(*value, state),
        Value::Decimal(value) => value.hash(state),
        Value::String(value) => value.hash(state),
        Value::Bytes(value) => value.as_ref().hash(state),
        Value::List(values) => {
            values.len().hash(state);
            for value in values {
                hash_value_variant_strict(value, state);
            }
        }
        Value::Record(record) => hash_record(record, state),
        Value::RecordTyped(record) => {
            record.type_id.hash(state);
            record.values.len().hash(state);
            for value in &record.values {
                value.is_some().hash(state);
                if let Some(value) = value {
                    hash_value_variant_strict(value, state);
                }
            }
        }
        Value::Path(path) => {
            path.graph.hash(state);
            path.start.hash(state);
            path.segments.len().hash(state);
            for segment in &path.segments {
                hash_path_segment(segment, state);
            }
        }
        Value::NodeRef(value) => value.hash(state),
        Value::EdgeRef(value) => value.hash(state),
        Value::GraphRef(value) => value.hash(state),
        Value::TableRef(value) => value.hash(state),
        Value::ZonedDateTime(value) => value.hash(state),
        Value::LocalDateTime(value) => value.hash(state),
        Value::Date(value) => value.hash(state),
        Value::ZonedTime(value) => value.hash(state),
        Value::LocalTime(value) => value.hash(state),
        Value::Duration(value) => selene_core::duration_order_key(value).hash(state),
        Value::Extended { type_id, payload } => {
            type_id.hash(state);
            payload.as_ref().hash(state);
        }
        Value::Null => {}
        Value::Uuid(value) => value.hash(state),
        Value::Vector(value) => {
            value.dimension().hash(state);
            for component in value.as_slice() {
                hash_f32_canonical(*component, state);
            }
        }
        Value::Json(value) => value.to_canonical_string().hash(state),
        _ => format!("{value:?}").hash(state),
    }
}

fn hash_record<H: Hasher>(record: &Record, state: &mut H) {
    // Variant-strict regime ([`DistinctRowKey`]): mirrors positional
    // `Value::PartialEq`, so this stays positional. The order-independent,
    // field-name-keyed record hash lives in [`hash_record_runtime_eq`], the
    // parity partner of the runtime-eq `record_key_equal`.
    mem::discriminant(record).hash(state);
    match record {
        Record::Open(fields) => {
            fields.len().hash(state);
            for (name, value) in fields {
                name.hash(state);
                hash_value_variant_strict(value, state);
            }
        }
        _ => format!("{record:?}").hash(state),
    }
}

fn hash_path_segment<H: Hasher>(segment: &PathSegment, state: &mut H) {
    segment.edge.hash(state);
    segment.node.hash(state);
}

fn hash_f64_canonical<H: Hasher>(value: f64, state: &mut H) {
    if value == 0.0 {
        0_u64.hash(state);
    } else if value.is_nan() {
        u64::MAX.hash(state);
    } else {
        value.to_bits().hash(state);
    }
}

fn hash_f32_canonical<H: Hasher>(value: f32, state: &mut H) {
    if value == 0.0 {
        0_u32.hash(state);
    } else if value.is_nan() {
        u32::MAX.hash(state);
    } else {
        value.to_bits().hash(state);
    }
}

fn hash_runtime_numeric<H: Hasher>(value: &Value, state: &mut H) -> bool {
    if let Some(key) = selene_core::NumericKey::of(value) {
        "runtime-number".hash(state);
        key.hash(state);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, hash_map::DefaultHasher},
        hash::{Hash, Hasher},
    };

    use proptest::{prelude::*, test_runner::Config};
    use selene_core::{Record, Value, VectorValue, db_string};
    use smallvec::smallvec;

    use super::{DistinctRowKey, RuntimeEqKey, runtime_values_equal};

    fn key_hash(value: &impl Hash) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn value_key_hash_eq_invariant_signed_zero() {
        let lhs = DistinctRowKey(vec![Value::Float(0.0)]);
        let rhs = DistinctRowKey(vec![Value::Float(-0.0)]);

        assert_eq!(lhs, rhs);
        assert_eq!(key_hash(&lhs), key_hash(&rhs));
    }

    #[test]
    fn value_key_hash_eq_invariant_nan() {
        let lhs = DistinctRowKey(vec![Value::Float(f64::from_bits(0x7ff8_0000_0000_0001))]);
        let rhs = DistinctRowKey(vec![Value::Float(f64::from_bits(0x7ff8_0000_0000_0002))]);

        assert_eq!(lhs, rhs);
        assert_eq!(key_hash(&lhs), key_hash(&rhs));
    }

    #[test]
    fn value_key_hash_eq_invariant_vector_signed_zero() {
        let lhs = DistinctRowKey(vec![Value::Vector(
            VectorValue::new(vec![0.0, -0.0]).unwrap(),
        )]);
        let rhs = DistinctRowKey(vec![Value::Vector(
            VectorValue::new(vec![-0.0, 0.0]).unwrap(),
        )]);

        assert_eq!(lhs, rhs);
        assert_eq!(key_hash(&lhs), key_hash(&rhs));
    }

    #[test]
    fn distinct_row_key_keeps_int_and_float_apart() {
        let int = DistinctRowKey(vec![Value::Int(1)]);
        let float = DistinctRowKey(vec![Value::Float(1.0)]);

        assert_ne!(int, float);
    }

    #[test]
    fn runtime_eq_key_collapses_cross_type_numerics() {
        let int = RuntimeEqKey::from_row(vec![Value::Int(1)]);
        let uint = RuntimeEqKey::from_row(vec![Value::Uint(1)]);
        let float = RuntimeEqKey::from_row(vec![Value::Float(1.0)]);
        let float32 = RuntimeEqKey::from_row(vec![Value::Float32(1.0)]);

        assert_eq!(int, uint);
        assert_eq!(int, float);
        assert_eq!(int, float32);
        assert_eq!(key_hash(&int), key_hash(&uint));
        assert_eq!(key_hash(&int), key_hash(&float));
        assert_eq!(key_hash(&int), key_hash(&float32));
    }

    #[test]
    fn runtime_eq_key_collapses_wide_and_decimal_numerics() {
        // GQLRT-26: Int128 / Uint128 / Decimal that are runtime-equal to the
        // 64-bit / float numerics must share the hash key, or DISTINCT / GROUP
        // BY / set-ops silently keep equal values apart.
        let int = RuntimeEqKey::from_row(vec![Value::Int(1)]);
        let int128 = RuntimeEqKey::from_row(vec![Value::Int128(1)]);
        let uint128 = RuntimeEqKey::from_row(vec![Value::Uint128(1)]);
        let decimal_int = RuntimeEqKey::from_row(vec![Value::Decimal("1".parse().unwrap())]);

        assert_eq!(int, int128);
        assert_eq!(int, uint128);
        assert_eq!(int, decimal_int);
        assert_eq!(key_hash(&int), key_hash(&int128));
        assert_eq!(key_hash(&int), key_hash(&uint128));
        assert_eq!(key_hash(&int), key_hash(&decimal_int));

        // A dyadic Decimal equals its binary-float twin and must hash with it.
        let half_float = RuntimeEqKey::from_row(vec![Value::Float(0.5)]);
        let half_decimal = RuntimeEqKey::from_row(vec![Value::Decimal("0.5".parse().unwrap())]);
        assert_eq!(half_float, half_decimal);
        assert_eq!(key_hash(&half_float), key_hash(&half_decimal));

        // A non-dyadic Decimal (0.1) is not equal to any binary float, so the
        // keys must stay distinct (no false collapse).
        let tenth_decimal = RuntimeEqKey::from_row(vec![Value::Decimal("0.1".parse().unwrap())]);
        let tenth_float = RuntimeEqKey::from_row(vec![Value::Float(0.1)]);
        assert_ne!(tenth_decimal, tenth_float);
    }

    #[test]
    fn runtime_eq_key_collapses_permuted_records() {
        // GQLRT-14 parity: `{a:1,b:2}` and `{b:2,a:1}` are field-name-equal, so
        // the RuntimeEqKey must treat them equal AND hash them identically, or
        // DISTINCT / GROUP BY / set-ops keep them apart.
        let a = db_string("a").expect("key fits DB string cap");
        let b = db_string("b").expect("key fits DB string cap");
        let lhs = Value::Record(Box::new(Record::Open(smallvec![
            (a.clone(), Value::Int(1)),
            (b.clone(), Value::Int(2)),
        ])));
        let rhs = Value::Record(Box::new(Record::Open(smallvec![
            (b, Value::Int(2)),
            (a, Value::Int(1)),
        ])));

        let lhs_key = RuntimeEqKey::from_row(vec![lhs.clone()]);
        let rhs_key = RuntimeEqKey::from_row(vec![rhs.clone()]);
        assert_eq!(lhs_key, rhs_key);
        assert_eq!(key_hash(&lhs_key), key_hash(&rhs_key));

        let mut map = HashMap::new();
        map.insert(RuntimeEqKey::from_row(vec![lhs]), 1);
        map.insert(RuntimeEqKey::from_row(vec![rhs]), 2);
        assert_eq!(
            map.len(),
            1,
            "permuted records collapse to one DISTINCT key"
        );
    }

    #[test]
    fn runtime_eq_key_record_cross_type_numeric_field_parity() {
        // A record field comparing equal under runtime numeric collapse
        // (`{a:1}` vs `{a:1.0}`) must also hash equal.
        let a = db_string("a").expect("key fits DB string cap");
        let int_rec = RuntimeEqKey::from_row(vec![Value::Record(Box::new(Record::Open(
            smallvec![(a.clone(), Value::Int(1))],
        )))]);
        let float_rec = RuntimeEqKey::from_row(vec![Value::Record(Box::new(Record::Open(
            smallvec![(a, Value::Float(1.0))],
        )))]);

        assert_eq!(int_rec, float_rec);
        assert_eq!(key_hash(&int_rec), key_hash(&float_rec));
    }

    #[test]
    fn runtime_eq_key_hashes_strings_by_content() {
        let a = RuntimeEqKey::from_row(vec![Value::String(
            db_string("same").expect("test string fits DB string cap"),
        )]);
        let b = RuntimeEqKey::from_row(vec![Value::String(
            db_string("same").expect("test string fits DB string cap"),
        )]);

        assert_eq!(a, b);
        assert_eq!(key_hash(&a), key_hash(&b));
    }

    #[test]
    fn runtime_eq_key_dedups_record_with_null_by_rust_equality() {
        let key = db_string("x").expect("test key fits DB string cap");
        let record = Value::Record(Box::new(Record::Open(smallvec![(key, Value::Null)])));
        let mut map = HashMap::new();

        assert_eq!(record, record.clone());
        map.insert(RuntimeEqKey::from_row(vec![record.clone()]), 1);
        map.insert(RuntimeEqKey::from_row(vec![record]), 2);

        assert_eq!(map.len(), 1);
        assert_eq!(map.values().copied().collect::<Vec<_>>(), vec![2]);
    }

    proptest! {
        #![proptest_config(Config::with_cases(256))]

        #[test]
        fn runtime_eq_key_parity_with_runtime_equal(
            lhs in runtime_value_strategy(),
            rhs in runtime_value_strategy(),
        ) {
            let expected = runtime_values_equal(&lhs, &rhs);
            let lhs_key = RuntimeEqKey::from_row(vec![lhs]);
            let rhs_key = RuntimeEqKey::from_row(vec![rhs]);

            prop_assert_eq!(lhs_key == rhs_key, expected);
        }
    }

    fn runtime_value_strategy() -> BoxedStrategy<Value> {
        prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            (-1000_i64..1000).prop_map(Value::Int),
            (0_u64..1000).prop_map(Value::Uint),
            (-1000_i64..1000).prop_map(|value| Value::Float(value as f64)),
            (-1000_i32..1000).prop_map(|value| Value::Float(value as f64 + 0.5)),
            Just(Value::Float(0.0)),
            Just(Value::Float(-0.0)),
            Just(Value::Float(f64::NAN)),
            Just(Value::Float(f64::INFINITY)),
            (-1000_i16..1000).prop_map(|value| Value::Float32(value as f32)),
            Just(Value::Float32(-0.0)),
            Just(Value::Float32(f32::NAN)),
            (-1000_i64..1000).prop_map(|value| Value::Int128(i128::from(value))),
            (0_u64..1000).prop_map(|value| Value::Uint128(u128::from(value))),
            (-1000_i64..1000)
                .prop_map(|value| { Value::Decimal(rust_decimal::Decimal::from(value)) }),
            (-1000_i64..1000)
                .prop_map(|value| { Value::Decimal(rust_decimal::Decimal::new(value, 1)) }),
            prop::sample::select(vec!["a", "b", "same"]).prop_map(|value| {
                Value::String(db_string(value).expect("test string fits DB string cap"))
            }),
            proptest::collection::vec(-1000_i16..1000, 1..8).prop_map(|components| {
                let components = components.into_iter().map(f32::from).collect::<Vec<_>>();
                Value::Vector(VectorValue::new(components).expect("test vector is finite"))
            }),
            permuted_record_strategy(),
        ]
        .boxed()
    }

    /// Two-field records over names {a,b} in either order with small values,
    /// exercising the GQLRT-14 permutation/cross-type-field parity invariant.
    fn permuted_record_strategy() -> impl Strategy<Value = Value> {
        let field_value = prop_oneof![
            (-3_i64..3).prop_map(Value::Int),
            (-3_i64..3).prop_map(|value| Value::Float(value as f64)),
            Just(Value::Null),
        ];
        (field_value.clone(), field_value, any::<bool>()).prop_map(|(a, b, reversed)| {
            let a_key = db_string("a").expect("string fits DB string cap");
            let b_key = db_string("b").expect("string fits DB string cap");
            let fields = if reversed {
                smallvec![(b_key, b), (a_key, a)]
            } else {
                smallvec![(a_key, a), (b_key, b)]
            };
            Value::Record(Box::new(Record::Open(fields)))
        })
    }
}
