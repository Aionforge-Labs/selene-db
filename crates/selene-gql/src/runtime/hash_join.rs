//! Hash-join join-tree operator.

use rustc_hash::FxHashMap;
use selene_core::{DbString, Value};

use crate::{
    BuildSide, JoinTree,
    runtime::{Binding, ExecutorError, value_key::RuntimeEqKey},
};

use super::{join_domain::JoinDomain, pattern};

/// Execute a binary pattern hash join.
///
/// Join keys are extracted by binding name via `pattern::key_values` and then
/// bucketed with `RuntimeEqKey`, so build/probe matching mirrors
/// `pattern::key_values_equal`: null keys are skipped, lossless cross-type
/// numeric values match, and strings compare by contents.
pub(crate) fn execute(
    left: &JoinTree,
    right: &JoinTree,
    key: &[DbString],
    build_side: BuildSide,
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Vec<Binding>, ExecutorError> {
    match build_side {
        BuildSide::Left => execute_ordered(left, right, key, env, true),
        BuildSide::Right => execute_ordered(right, left, key, env, false),
    }
}

fn execute_ordered(
    build_tree: &JoinTree,
    probe_tree: &JoinTree,
    key: &[DbString],
    env: pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
    build_is_left: bool,
) -> Result<Vec<Binding>, ExecutorError> {
    let key_indexes = pattern::resolve_key(env.schema, key)?;
    let build_rows = pattern::walk_join_tree(build_tree, env)?;
    let mut domains = JoinDomain::default();
    let mut build_entries = FxHashMap::default();
    for row in build_rows {
        if let Some(key_values) = pattern::key_values_at(&row, &key_indexes) {
            domains.observe(&key_values)?;
            insert_build_row(&mut build_entries, key_values, row);
        }
    }

    let probe_rows = pattern::walk_join_tree(probe_tree, env)?;
    let mut rows = Vec::new();
    for probe in probe_rows {
        let Some(probe_key) = pattern::key_values_at(&probe, &key_indexes) else {
            continue;
        };
        let mut probe_domain = JoinDomain::default();
        probe_domain.observe(&probe_key)?;
        domains.compare(&probe_domain)?;
        if !pattern::key_values_equal(&probe_key, &probe_key)? {
            continue;
        }
        if let Some(matching_builds) = matching_builds(&build_entries, probe_key) {
            for build in matching_builds {
                let row = if build_is_left {
                    pattern::merge_rows(build, &probe, env.schema)
                } else {
                    pattern::merge_rows(&probe, build, env.schema)
                };
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

fn insert_build_row(
    build_entries: &mut FxHashMap<RuntimeEqKey, Vec<Binding>>,
    key_values: Vec<Value>,
    row: Binding,
) {
    build_entries
        .entry(RuntimeEqKey::from_row(key_values))
        .or_default()
        .push(row);
}

fn matching_builds(
    build_entries: &FxHashMap<RuntimeEqKey, Vec<Binding>>,
    probe_key: Vec<Value>,
) -> Option<&[Binding]> {
    build_entries
        .get(&RuntimeEqKey::from_row(probe_key))
        .map(Vec::as_slice)
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashMap;
    use selene_core::Value;

    use super::{insert_build_row, matching_builds};
    use crate::runtime::Binding;

    #[test]
    fn hash_join_uses_runtime_eq_keys() {
        let mut build_entries = FxHashMap::default();
        insert_build_row(
            &mut build_entries,
            vec![Value::Int(1)],
            Binding::new([Value::Int(10)]),
        );

        let matches =
            matching_builds(&build_entries, vec![Value::Float(1.0)]).expect("probe matches");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].values(), &[Value::Int(10)]);
    }

    #[test]
    fn join_domain_rejects_incomparable_keys_and_predicates_skip_unknown() {
        let mut domain = crate::runtime::comparison_domain::ComparisonDomain::default();
        domain
            .observe(
                &[Value::Int(1)],
                selene_core::ComparisonMode::PredicateEquality,
            )
            .unwrap();
        assert!(
            domain
                .observe(
                    &[Value::String(selene_core::db_string("1").unwrap())],
                    selene_core::ComparisonMode::PredicateEquality
                )
                .is_err()
        );
        for value in [Value::Float(f64::NAN), Value::List(vec![Value::Null])] {
            assert!(
                !crate::runtime::pattern::key_values_equal(
                    std::slice::from_ref(&value),
                    std::slice::from_ref(&value)
                )
                .unwrap()
            );
        }
        assert!(
            crate::runtime::pattern::key_values_equal(&[Value::Int(1)], &[Value::Float(1.0)])
                .unwrap()
        );
    }
}
