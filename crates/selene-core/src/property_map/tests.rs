use super::*;
use crate::db_string;
use proptest::prelude::*;

fn key(name: &str) -> DbString {
    db_string(name).unwrap()
}

fn int(value: i64) -> Value {
    Value::Int(value)
}

#[test]
fn standard_get_set_remove_round_trip() {
    let mut map = PropertyMap::new();
    let name = key("pm.name");
    map.set(name.clone(), int(1)).unwrap();
    assert_eq!(map.get(&name), Some(&int(1)));
    assert_eq!(map.remove(&name), Some(int(1)));
    assert!(map.get(&name).is_none());
}

#[test]
fn standard_keys_remain_sorted_after_inserts() {
    let mut map = PropertyMap::new();
    for name in ["pm.c", "pm.a", "pm.b"] {
        map.set(key(name), Value::String(key(name))).unwrap();
    }
    assert!(map.sorted_invariant_holds());
}

#[test]
fn from_pairs_sorts_and_keeps_last_duplicate_value() {
    let a = key("pm.from_pairs.a");
    let b = key("pm.from_pairs.b");
    let map = PropertyMap::from_pairs([
        (b.clone(), int(10)),
        (a.clone(), int(1)),
        (b.clone(), int(20)),
        (a.clone(), int(2)),
    ])
    .unwrap();

    assert_eq!(map.get(&a), Some(&int(2)));
    assert_eq!(map.get(&b), Some(&int(20)));
    assert_eq!(
        map.keys().cloned().collect::<Vec<_>>(),
        vec![a.clone(), b.clone()]
    );
    assert!(map.sorted_invariant_holds());
}

#[test]
fn from_pairs_accepts_empty_and_singleton_maps() {
    let empty = PropertyMap::from_pairs([]).unwrap();
    assert!(empty.is_empty());
    assert!(empty.sorted_invariant_holds());

    let score = key("pm.from_pairs.score");
    let singleton = PropertyMap::from_pairs([(score.clone(), int(42))]).unwrap();
    assert_eq!(singleton.len(), 1);
    assert_eq!(singleton.get(&score), Some(&int(42)));
    assert!(singleton.sorted_invariant_holds());
}

#[test]
fn from_pairs_accepts_already_canonical_pairs() {
    let a = key("pm.from_pairs.canonical.a");
    let b = key("pm.from_pairs.canonical.b");
    let map = PropertyMap::from_pairs([(a.clone(), int(1)), (b.clone(), int(2))]).unwrap();

    assert_eq!(map.get(&a), Some(&int(1)));
    assert_eq!(map.get(&b), Some(&int(2)));
    assert!(map.sorted_invariant_holds());
}

#[test]
fn compact_get_by_known_key() {
    let a = key("pm.compact.a");
    let b = key("pm.compact.b");
    let map = PropertyMap::compact([a.clone(), b.clone()], [Some(int(1)), None]).unwrap();
    assert_eq!(map.get(&a), Some(&int(1)));
    assert_eq!(map.get(&b), None);
}

#[test]
fn compact_accepts_empty_and_singleton_maps() {
    let empty = PropertyMap::compact([], []).unwrap();
    assert!(empty.is_empty());
    assert!(empty.sorted_invariant_holds());

    let score = key("pm.compact.score");
    let singleton = PropertyMap::compact([score.clone()], [Some(int(42))]).unwrap();
    assert_eq!(singleton.len(), 1);
    assert_eq!(singleton.get(&score), Some(&int(42)));
    assert!(singleton.sorted_invariant_holds());
}

#[test]
fn compact_accepts_already_canonical_keys() {
    let a = key("pm.compact.canonical.a");
    let b = key("pm.compact.canonical.b");
    let map = PropertyMap::compact([a.clone(), b.clone()], [Some(int(1)), None]).unwrap();

    assert_eq!(map.get(&a), Some(&int(1)));
    assert_eq!(map.get(&b), None);
    assert!(map.sorted_invariant_holds());
}

#[test]
fn compact_widens_on_unknown_key_insert() {
    let a = key("pm.widen.a");
    let b = key("pm.widen.b");
    let c = key("pm.widen.c");
    let mut map =
        PropertyMap::compact([a.clone(), b.clone()], [Some(int(1)), Some(int(2))]).unwrap();
    map.set(c.clone(), int(3)).unwrap();
    assert!(matches!(map, PropertyMap::Standard(_)));
    assert_eq!(map.get(&a), Some(&int(1)));
    assert_eq!(map.get(&b), Some(&int(2)));
    assert_eq!(map.get(&c), Some(&int(3)));
}

#[test]
fn widening_preserves_optional_values() {
    let a = key("pm.none.a");
    let b = key("pm.none.b");
    let c = key("pm.none.c");
    let mut map = PropertyMap::compact([a.clone(), b.clone()], [None, Some(int(2))]).unwrap();
    map.set(c, int(3)).unwrap();
    assert_eq!(map.get(&a), None);
    assert_eq!(map.get(&b), Some(&int(2)));
    assert_eq!(map.len(), 2);
}

#[test]
fn iter_yields_keys_in_sorted_order() {
    let a = key("pm.iter.a");
    let b = key("pm.iter.b");
    let standard = PropertyMap::from_pairs([(b.clone(), int(2)), (a.clone(), int(1))]).unwrap();
    assert_eq!(
        standard.keys().cloned().collect::<Vec<_>>(),
        vec![a.clone(), b.clone()]
    );

    let compact =
        PropertyMap::compact([b.clone(), a.clone()], [Some(int(2)), Some(int(1))]).unwrap();
    assert_eq!(compact.keys().cloned().collect::<Vec<_>>(), vec![a, b]);
}

#[test]
fn len_is_empty_consistency() {
    let mut map = PropertyMap::new();
    assert!(map.is_empty());
    map.set(key("pm.len"), int(1)).unwrap();
    assert_eq!(map.len(), 1);
    assert!(!map.is_empty());
}

#[test]
fn compact_rejects_mismatched_key_value_lengths() {
    let a = key("pm.mismatch.a");
    let b = key("pm.mismatch.b");
    let err = PropertyMap::compact([a.clone(), b], [Some(int(1))]).unwrap_err();
    assert!(matches!(
        err,
        CoreError::CompactKeyValueLengthMismatch { keys: 2, values: 1 }
    ));
    let err = PropertyMap::compact([a], [Some(int(1)), Some(int(2))]).unwrap_err();
    assert!(matches!(
        err,
        CoreError::CompactKeyValueLengthMismatch { keys: 1, values: 2 }
    ));
}

#[test]
fn format2_independent_of_insertion_order() {
    // Two maps built from different insertion orders of the same pairs
    // serialize to byte-identical wire.
    let pairs = [
        (key("pm.order.gamma"), int(3)),
        (key("pm.order.alpha"), int(1)),
        (key("pm.order.beta"), int(2)),
    ];
    let forward = PropertyMap::from_pairs(pairs.iter().cloned()).unwrap();
    let reverse = PropertyMap::from_pairs(pairs.iter().rev().cloned()).unwrap();
    assert_eq!(
        crate::serde_tests::encode_map(&forward).unwrap(),
        crate::serde_tests::encode_map(&reverse).unwrap(),
    );
}

#[test]
fn edge_cases_for_missing_and_overwrite() {
    let a = key("pm.edge.a");
    let mut map = PropertyMap::new();
    assert_eq!(map.get(&a), None);
    assert_eq!(map.remove(&a), None);
    map.set(a.clone(), int(1)).unwrap();
    map.set(a.clone(), int(2)).unwrap();
    assert_eq!(map.len(), 1);
    assert_eq!(map.remove(&a), Some(int(2)));
    assert!(map.is_empty());
}

proptest! {
    #[test]
    fn standard_insert_remove_preserves_sorted_invariant(ops in proptest::collection::vec((0_u8..32, any::<bool>()), 1..128)) {
        let mut map = PropertyMap::new();
        let mut expected = std::collections::BTreeSet::new();
        for (raw, insert) in ops {
            let name = format!("pm.prop.{raw}");
            let key = db_string(&name).unwrap();
            if insert {
                map.set(key.clone(), Value::Uint(u64::from(raw))).unwrap();
                expected.insert(key);
            } else {
                map.remove(&key);
                expected.remove(&key);
            }
            prop_assert!(map.sorted_invariant_holds());
            prop_assert_eq!(map.len(), expected.len());
        }
    }

    #[test]
    fn compact_widening_matches_reference_map(
        // Slot presence for the four schema keys at construction.
        seed in proptest::collection::vec(any::<bool>(), 4),
        // Unknown keys exercise the widening path.
        ops in proptest::collection::vec((0_usize..6, any::<bool>(), 0_u64..16), 0..64),
    ) {
        let pid = std::process::id();
        let schema_keys: Vec<DbString> = (0..4)
            .map(|i| db_string(&format!("pm.core16.{pid}.k{i}")).unwrap())
            .collect();
        let all_keys: Vec<DbString> = (0..6)
            .map(|i| db_string(&format!("pm.core16.{pid}.k{i}")).unwrap())
            .collect();

        let mut oracle = std::collections::BTreeMap::<DbString, Value>::new();
        let seed_values: Vec<Option<Value>> = schema_keys
            .iter()
            .zip(&seed)
            .map(|(k, present)| {
                if *present {
                    oracle.insert(k.clone(), Value::Uint(0));
                    Some(Value::Uint(0))
                } else {
                    None
                }
            })
            .collect();

        let mut map = PropertyMap::compact(schema_keys.iter().cloned(), seed_values).unwrap();
        let is_compact = matches!(map, PropertyMap::Compact { .. });
        prop_assert!(is_compact);

        for (idx, present, raw) in ops {
            let key = all_keys[idx].clone();
            if present {
                let value = Value::Uint(raw);
                map.set(key.clone(), value.clone()).unwrap();
                oracle.insert(key, value);
            } else {
                let removed = map.remove(&key);
                let expected = oracle.remove(&key);
                prop_assert_eq!(removed, expected);
            }

            // Observational equality: get() agrees, including None == absent.
            for k in &all_keys {
                prop_assert_eq!(map.get(k), oracle.get(k));
            }
            prop_assert_eq!(map.len(), oracle.len());
            prop_assert!(map.sorted_invariant_holds());

            // iter() yields exactly the oracle's present pairs in key order.
            let observed: Vec<(DbString, Value)> =
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let expected: Vec<(DbString, Value)> =
                oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            prop_assert_eq!(observed, expected);
        }
    }
}
