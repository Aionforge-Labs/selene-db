//! Independent relation-model oracle for join/set differentials (F04-PR03)
//! and grouping/sorting differentials (F04-PR04).
//!
//! Comparing batch results only against the row executor is insufficient
//! where both engines could share a wrong rewrite, so this module implements
//! a small, separately written relation model: a nested-loop join and
//! multiset set operators over plain row vectors, plus grouping assignment
//! and ordering models. It shares no executor code
//! with either engine path — no [`RuntimeEqKey`], no join/domain validators,
//! no aggregate slots, no sort comparators, no row adapters. Its only shared
//! authority with production is
//! [`selene_core`] itself (exact numeric keys, duration order keys, canonical
//! JSON), which is the specification-level ground truth, not an optimizer
//! rewrite.
//!
//! Value semantics mirror the language equality relations directly:
//!
//! - Join keys follow the predicate-equality regime with the row path's
//!   validity rule: null keys never match, and composite keys containing
//!   null leaves or NaN never match (the probe self-check), while
//!   cross-type numerics collapse through exact numeric keys and records
//!   compare by field name.
//! - Set rows follow the distinctness regime where null equals null, so
//!   `UNION` deduplicates null rows and `EXCEPT` removes them.
//! - Group keys follow the grouping-equivalence regime: nulls belong
//!   together (one all-null group), cross-type numerics collapse, records
//!   compare by field name. Groups emit in first-emission order.
//! - Sort keys follow the ordering regime with per-key direction and null
//!   placement: nulls sort as a block before or after every non-null value,
//!   strings order by binary contents (the engine's selected collation),
//!   and ties keep input order (stable sort, no implicit total order).
//!
//! Order rules mirror the row path (probe-major joins, left-arm-first sets)
//! so differentials can compare exactly, but the primary assertions compare
//! multisets ([`assert_same_multiset`]) to keep order assumptions out of the
//! oracle.

use selene_core::{NumericKey, Record, Value};
use smallvec::SmallVec;

/// Test-local runtime equality over values.
///
/// Reimplements the runtime-equality relation from core primitives and
/// structural recursion: exact numeric collapse, order-independent open
/// records with recursive fields, positional lists, content strings, stable
/// reference identities, and deterministic fallbacks for the remaining
/// scalar families.
pub(crate) fn values_equal(lhs: &Value, rhs: &Value) -> bool {
    match (lhs, rhs) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => {
            if let (Some(lhs), Some(rhs)) = (NumericKey::of(lhs), NumericKey::of(rhs)) {
                return lhs == rhs;
            }
            match (lhs, rhs) {
                (Value::String(lhs), Value::String(rhs)) => lhs.as_str() == rhs.as_str(),
                (Value::Record(lhs), Value::Record(rhs)) => records_equal(lhs, rhs),
                (Value::List(lhs), Value::List(rhs)) => {
                    lhs.len() == rhs.len()
                        && lhs.iter().zip(rhs.iter()).all(|(a, b)| values_equal(a, b))
                }
                (Value::Vector(lhs), Value::Vector(rhs)) => {
                    lhs.dimension() == rhs.dimension()
                        && lhs.as_slice().iter().zip(rhs.as_slice()).all(|(a, b)| {
                            // NaN never equals, matching the engine's float
                            // identity; infinities and signed zeros compare
                            // by IEEE equality.
                            a == b
                        })
                }
                (Value::Json(lhs), Value::Json(rhs)) => {
                    lhs.to_canonical_string() == rhs.to_canonical_string()
                }
                (Value::Duration(lhs), Value::Duration(rhs)) => {
                    selene_core::duration_order_key(lhs) == selene_core::duration_order_key(rhs)
                }
                _ => lhs == rhs,
            }
        }
    }
}

/// Test-local open-record equality: same field-name set with recursively
/// equal values, independent of field order. Non-open records fall back to
/// structural equality, as on the engine path.
fn records_equal(lhs: &Record, rhs: &Record) -> bool {
    match (lhs, rhs) {
        (Record::Open(lhs), Record::Open(rhs)) => {
            lhs.len() == rhs.len()
                && lhs.iter().all(|(name, value)| {
                    rhs.iter()
                        .find(|(other, _)| name == other)
                        .is_some_and(|(_, other)| values_equal(value, other))
                })
        }
        _ => lhs == rhs,
    }
}

/// True when one join-key value may match against itself (probe validity).
///
/// Mirrors the engine probe self-check without sharing its code: nulls,
/// NaN components, null leaves inside composites, absent record-typed
/// fields, and closed records never match; every other value may.
pub(crate) fn key_self_valid(value: &Value) -> bool {
    key_element_valid(value)
}

/// True when one join-key element can participate in a match against itself.
///
/// Mirrors the row probe self-check: nulls, NaN components, null leaves
/// inside composites, and absent record-typed fields never match, while
/// every other value may.
fn key_element_valid(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Float(value) => !value.is_nan(),
        Value::Float32(value) => !value.is_nan(),
        Value::List(values) => values.iter().all(key_element_valid),
        Value::Record(record) => match &**record {
            Record::Open(fields) => fields.iter().all(|(_, value)| key_element_valid(value)),
            // Closed records never match themselves on the engine path.
            _ => false,
        },
        Value::RecordTyped(record) => record
            .values
            .iter()
            .all(|slot| slot.as_ref().is_some_and(key_element_valid)),
        Value::Vector(vector) => !vector.as_slice().iter().any(|c| c.is_nan()),
        _ => true,
    }
}

/// True when every element of a probe key may match.
fn probe_key_valid(keys: &[Value]) -> bool {
    keys.iter().all(key_element_valid)
}

/// Extract non-null key values at `indexes`, or `None` for a null key.
///
/// Mirrors the engine's null-key skip without sharing its code.
fn extract_key(row: &[Value], indexes: &[usize]) -> Option<SmallVec<[Value; 4]>> {
    let mut keys = SmallVec::new();
    for index in indexes {
        let value = row.get(*index).cloned().unwrap_or(Value::Null);
        if matches!(value, Value::Null) {
            return None;
        }
        keys.push(value);
    }
    Some(keys)
}

/// Independent nested-loop inner join over plain row vectors.
///
/// `build`/`probe` are row slices in walk order; `key_indexes` address the
/// shared key columns; `build_is_left` selects the merge orientation.
/// Output is probe-major with build order inside each probe, matching the
/// row engine's emission order.
pub(crate) fn nested_join(
    build: &[Vec<Value>],
    probe: &[Vec<Value>],
    key_indexes: &[usize],
    build_is_left: bool,
    width: usize,
) -> Vec<Vec<Value>> {
    let mut output = Vec::new();
    for probe_row in probe {
        let Some(probe_keys) = extract_key(probe_row, key_indexes) else {
            continue;
        };
        if !probe_key_valid(&probe_keys) {
            continue;
        }
        for build_row in build {
            let Some(build_keys) = extract_key(build_row, key_indexes) else {
                continue;
            };
            if probe_keys.len() != build_keys.len()
                || !probe_keys
                    .iter()
                    .zip(build_keys.iter())
                    .all(|(a, b)| values_equal(a, b))
            {
                continue;
            }
            output.push(merge_values(build_row, probe_row, build_is_left, width));
        }
    }
    output
}

/// Merge one joined row preferring the join-left side's bound values.
///
/// Null is the unbound sentinel: bound values win regardless of side.
fn merge_values(
    build_row: &[Value],
    probe_row: &[Value],
    build_is_left: bool,
    width: usize,
) -> Vec<Value> {
    let (left, right) = if build_is_left {
        (build_row, probe_row)
    } else {
        (probe_row, build_row)
    };
    (0..width)
        .map(|index| {
            let left_value = left.get(index).cloned().unwrap_or(Value::Null);
            if matches!(left_value, Value::Null) {
                right.get(index).cloned().unwrap_or(Value::Null)
            } else {
                left_value
            }
        })
        .collect()
}

/// Independent multiset set operation over plain row vectors.
///
/// `op` selects union/intersect/except in set (`distinct: true`) or
/// multiset form. Rows compare with [`values_equal`] (null equals null);
/// matching consumes right-arm rows for the multiset variants. Output
/// preserves left-arm order with first occurrences, matching the row
/// engine's emission order.
pub(crate) fn multiset_op(
    op: crate::SetOp,
    lhs: &[Vec<Value>],
    rhs: &[Vec<Value>],
    distinct: bool,
) -> Vec<Vec<Value>> {
    use crate::SetOp;
    let mut output = Vec::new();
    let mut consumed = vec![false; rhs.len()];
    let mut emitted: Vec<Vec<Value>> = Vec::new();
    match op {
        SetOp::UnionAll | SetOp::Union => {
            for row in lhs.iter().chain(rhs.iter()) {
                if distinct && emitted.iter().any(|seen| rows_equal(seen, row)) {
                    continue;
                }
                emitted.push(row.clone());
                output.push(row.clone());
            }
        }
        SetOp::IntersectAll | SetOp::Intersect => {
            for row in lhs {
                if distinct && emitted.iter().any(|seen| rows_equal(seen, row)) {
                    continue;
                }
                if distinct {
                    if rhs.iter().any(|candidate| rows_equal(row, candidate)) {
                        emitted.push(row.clone());
                        output.push(row.clone());
                    }
                } else if let Some((index, _)) = rhs
                    .iter()
                    .enumerate()
                    .find(|(index, candidate)| !consumed[*index] && rows_equal(row, candidate))
                {
                    consumed[index] = true;
                    output.push(row.clone());
                }
            }
        }
        SetOp::ExceptAll | SetOp::Except => {
            for row in lhs {
                if distinct && emitted.iter().any(|seen| rows_equal(seen, row)) {
                    continue;
                }
                let removed = rhs
                    .iter()
                    .enumerate()
                    .find(|(index, candidate)| {
                        (distinct || !consumed[*index]) && rows_equal(row, candidate)
                    })
                    .map(|(index, _)| index);
                if distinct {
                    if removed.is_none() {
                        emitted.push(row.clone());
                        output.push(row.clone());
                    }
                } else if let Some(index) = removed {
                    consumed[index] = true;
                } else {
                    output.push(row.clone());
                }
            }
        }
        SetOp::Otherwise => {
            if lhs.is_empty() {
                output.extend(rhs.iter().cloned());
            } else {
                output.extend(lhs.iter().cloned());
            }
        }
    }
    output
}

/// Row equality under the set regime.
fn rows_equal(lhs: &[Value], rhs: &[Value]) -> bool {
    lhs.len() == rhs.len() && lhs.iter().zip(rhs.iter()).all(|(a, b)| values_equal(a, b))
}

/// Assign row indexes to groups under the grouping-equivalence regime.
///
/// `key_width` leading columns of each row form the group key. Rows join
/// the first group whose key tuple is [`values_equal`]-equal (nulls belong
/// together, so an all-null key forms one group); otherwise they open a new
/// group. Groups emit in first-emission order, matching the engine's group
/// emission order. Returns one index list per group.
pub(crate) fn groups_of(rows: &[Vec<Value>], key_width: usize) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let key = &row[..key_width.min(row.len())];
        let hit = groups.iter().position(|members| {
            let first = &rows[members[0]];
            let first_key = &first[..key_width.min(first.len())];
            first_key.len() == key.len()
                && first_key
                    .iter()
                    .zip(key.iter())
                    .all(|(a, b)| values_equal(a, b))
        });
        match hit {
            Some(group) => groups[group].push(index),
            None => groups.push(vec![index]),
        }
    }
    groups
}

/// Deduplicate rows keeping first occurrences under the set regime.
///
/// Independent first-seen retention without sharing the engine's key or
/// domain code: a row is kept exactly when no earlier kept row is
/// [`rows_equal`]-equal to it.
pub(crate) fn model_distinct(rows: &[Vec<Value>]) -> Vec<Vec<Value>> {
    let mut kept: Vec<Vec<Value>> = Vec::new();
    for row in rows {
        if !kept.iter().any(|seen| rows_equal(seen, row)) {
            kept.push(row.clone());
        }
    }
    kept
}

/// One independently specified sort key: a column with a direction and a
/// null placement.
pub(crate) struct ModelSortKey {
    /// Sorted column index.
    pub(crate) column: usize,
    /// True for ascending, false for descending.
    pub(crate) ascending: bool,
    /// True when nulls sort before every non-null value.
    pub(crate) nulls_first: bool,
}

/// Stable sort of row indexes under the ordering regime.
///
/// Returns the input indexes in sorted order; ties keep input order (a
/// stable sort, so no implicit total order is invented). Values compare
/// through [`model_values_cmp`]; rows carrying a value outside the
/// model-ordered families panic rather than inventing an order.
pub(crate) fn model_sort(rows: &[Vec<Value>], keys: &[ModelSortKey]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|lhs, rhs| model_rows_cmp(&rows[*lhs], &rows[*rhs], keys));
    order
}

/// Compare two rows key by key under the ordering regime.
fn model_rows_cmp(lhs: &[Value], rhs: &[Value], keys: &[ModelSortKey]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for key in keys {
        let lhs = lhs.get(key.column).cloned().unwrap_or(Value::Null);
        let rhs = rhs.get(key.column).cloned().unwrap_or(Value::Null);
        let ordering = match (&lhs, &rhs) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (_, Value::Null) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            _ => {
                let ordering = model_values_cmp(&lhs, &rhs);
                if key.ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Compare two non-null values under the ordering regime.
///
/// Cross-type numerics order through exact numeric keys, strings order by
/// binary contents (the engine's selected collation), and booleans order
/// `false < true`. Anything else panics: the model covers the fixture
/// families and refuses to invent an order beyond them.
fn model_values_cmp(lhs: &Value, rhs: &Value) -> std::cmp::Ordering {
    if let (Some(lhs), Some(rhs)) = (NumericKey::of(lhs), NumericKey::of(rhs)) {
        return lhs.sort_cmp(rhs);
    }
    match (lhs, rhs) {
        (Value::Bool(lhs), Value::Bool(rhs)) => lhs.cmp(rhs),
        (Value::String(lhs), Value::String(rhs)) => lhs.as_str().cmp(rhs.as_str()),
        _ => panic!("relation model does not order {lhs:?} against {rhs:?}"),
    }
}

/// Assert two row multisets carry the same equality classes with the same
/// counts, independent of order.
///
/// # Panics
///
/// Panics with `what` context on any count divergence.
pub(crate) fn assert_same_multiset(expected: &[Vec<Value>], actual: &[Vec<Value>], what: &str) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "{what}: row count diverged (expected {}, actual {})",
        expected.len(),
        actual.len()
    );
    let mut consumed = vec![false; actual.len()];
    for (index, want) in expected.iter().enumerate() {
        let hit = actual
            .iter()
            .enumerate()
            .find(|(slot, got)| !consumed[*slot] && rows_equal(want, got))
            .map(|(slot, _)| slot);
        assert!(
            hit.is_some(),
            "{what}: row {index} {want:?} has no unmatched peer in actual {actual:?}"
        );
        consumed[hit.expect("matched peer")] = true;
    }
}
