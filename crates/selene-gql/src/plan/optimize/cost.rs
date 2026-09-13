//! Cost / cardinality model for index-vs-Linear plan decisions (OPT-5).
//!
//! The estimators here translate predicate shapes into estimated *output row
//! counts* (lower = cheaper) by reading live statistics from the pinned
//! [`IndexCatalog`] snapshot. The index-selection rules call them immediately
//! before assigning a non-`Linear` [`crate::plan::ScanAccess`]: an index path is
//! taken only when its estimated cost is strictly below the residual baseline
//! (the cost of the path that would otherwise run). Each estimator returns
//! `Option<u64>`; `None` means "no statistic available" and the caller keeps its
//! pre-OPT-5 structural decision verbatim.
//!
//! # Why results are unchanged
//!
//! Every selectable `ScanAccess` variant (`LabelIndex`, `TypedIndexRange`,
//! `BitmapUnion`, `CompositeLookup`, `DisjunctiveScan`) is row-equivalent to a
//! Linear scan: the runtime always re-applies the label predicate and residual
//! property filters, so the produced row multiset is identical regardless of
//! access path. Cost can therefore only (a) choose between row-equivalent paths
//! or (b) decline a path, in which case the scan stays `Linear` — the trivially
//! correct full evaluation. The statistics are read from the same pinned
//! snapshot the executor runs against, so a plan can never be costed against a
//! graph that differs from the one executed.

use selene_core::{DbString, Value};

use crate::Literal;
use crate::plan::{IndexKey, IndexTarget, TypedIndexBounds, optimize::IndexCatalog};

/// Parameter-range fallback fraction: a single-ended or double-ended range over
/// a parameter bound is estimated to return `total_distinct / RANGE_DIVISOR` of
/// the index, mirroring the verbatim `HEURISTIC_RANGE = 0.33` (≈ 1/3) used by
/// the selectivity heuristics so the no-exact-stat path is unchanged.
const RANGE_DIVISOR: u64 = 3;

/// The Linear-scan baseline for a single-label scan: the label bitmap size when
/// known, else the total row count. This is the cost the index path must beat.
///
/// Returns `None` only when neither statistic is available (no-stats catalog),
/// signalling the caller to keep its structural decision.
pub fn linear_baseline(
    catalog: &dyn IndexCatalog,
    target: IndexTarget,
    label: DbString,
) -> Option<u64> {
    match catalog.label_cardinality(target, label) {
        Some(rows) => Some(rows),
        None => catalog.total_rows(target),
    }
}

/// Estimated rows produced by promoting a bare single-label scan to a label
/// index. The label index enumerates exactly the rows carrying the label.
pub fn label_scan_cost(
    catalog: &dyn IndexCatalog,
    target: IndexTarget,
    label: DbString,
) -> Option<u64> {
    catalog.label_cardinality(target, label)
}

/// Estimated rows for a typed equality / range probe under
/// `(target, label, property)`.
///
/// Literal bounds resolve to exact cardinalities; parameter bounds fall back to
/// the average bucket size (equality) or a `1/RANGE_DIVISOR` fraction of the
/// distinct-key estimate (range), matching the retained heuristics.
pub fn typed_index_cost(
    catalog: &dyn IndexCatalog,
    target: IndexTarget,
    label: DbString,
    property: DbString,
    bounds: &TypedIndexBounds,
) -> Option<u64> {
    match bounds {
        TypedIndexBounds::Equality(key) => equality_cost(catalog, target, label, property, key),
        TypedIndexBounds::GreaterThan(key)
        | TypedIndexBounds::GreaterEqual(key)
        | TypedIndexBounds::LessThan(key)
        | TypedIndexBounds::LessEqual(key) => {
            single_ended_range_cost(catalog, target, label, property, bounds, key)
        }
        TypedIndexBounds::Range {
            lo,
            lo_inclusive,
            hi,
            hi_inclusive,
        } => {
            // Exact only when both bounds are literals; otherwise fall back to
            // the parameter range fraction.
            if let (Some(lo_val), Some(hi_val)) = (literal_value(lo), literal_value(hi)) {
                let range = (
                    bound(lo_val, *lo_inclusive, BoundEnd::Lower),
                    bound(hi_val, *hi_inclusive, BoundEnd::Upper),
                );
                catalog.range_cardinality(target, label, property, range)
            } else {
                parameter_range_cost(catalog, target, label)
            }
        }
    }
}

/// Equality cost: exact match count for a literal, average bucket for a
/// parameter.
fn equality_cost(
    catalog: &dyn IndexCatalog,
    target: IndexTarget,
    label: DbString,
    property: DbString,
    key: &IndexKey,
) -> Option<u64> {
    match literal_value(key) {
        Some(value) => catalog.equality_cardinality(target, label, property, &value),
        None => catalog.typed_avg_bucket(target, label, property),
    }
}

/// Single-ended range cost: exact when the bound is a literal, else the
/// parameter range fraction.
fn single_ended_range_cost(
    catalog: &dyn IndexCatalog,
    target: IndexTarget,
    label: DbString,
    property: DbString,
    bounds: &TypedIndexBounds,
    key: &IndexKey,
) -> Option<u64> {
    let Some(value) = literal_value(key) else {
        return parameter_range_cost(catalog, target, label);
    };
    let range = match bounds {
        TypedIndexBounds::GreaterThan(_) => {
            (std::ops::Bound::Excluded(value), std::ops::Bound::Unbounded)
        }
        TypedIndexBounds::GreaterEqual(_) => {
            (std::ops::Bound::Included(value), std::ops::Bound::Unbounded)
        }
        TypedIndexBounds::LessThan(_) => {
            (std::ops::Bound::Unbounded, std::ops::Bound::Excluded(value))
        }
        TypedIndexBounds::LessEqual(_) => {
            (std::ops::Bound::Unbounded, std::ops::Bound::Included(value))
        }
        // The two non-single-ended variants are routed by the caller.
        TypedIndexBounds::Equality(_) | TypedIndexBounds::Range { .. } => return None,
    };
    catalog.range_cardinality(target, label, property, range)
}

/// Parameter range fallback (value unknown at plan time): a range keeps roughly
/// `1/RANGE_DIVISOR` of the population (the label-scoped row count, else total
/// rows), mirroring the retained `HEURISTIC_RANGE` ≈ 1/3 fraction. Declines when
/// no population statistic is available.
fn parameter_range_cost(
    catalog: &dyn IndexCatalog,
    target: IndexTarget,
    label: DbString,
) -> Option<u64> {
    let population = catalog
        .label_cardinality(target, label)
        .or_else(|| catalog.total_rows(target))?;
    Some((population / RANGE_DIVISOR).max(1).min(population))
}

/// In-list cost: Σ per-element equality cardinality (literals) or
/// `k * avg_bucket` (parameters). Returns `None` if any element lacks a stat.
pub fn in_list_cost(
    catalog: &dyn IndexCatalog,
    target: IndexTarget,
    label: DbString,
    property: DbString,
    keys: &[IndexKey],
) -> Option<u64> {
    let mut total: u64 = 0;
    for key in keys {
        let element = match literal_value(key) {
            Some(value) => {
                catalog.equality_cardinality(target, label.clone(), property.clone(), &value)?
            }
            None => catalog.typed_avg_bucket(target, label.clone(), property.clone())?,
        };
        total = total.saturating_add(element);
    }
    Some(total)
}

/// Composite lookup cost: exact match count for all-literal keys, average
/// bucket for any parameter key.
pub fn composite_cost(
    catalog: &dyn IndexCatalog,
    target: IndexTarget,
    label: DbString,
    properties: &[DbString],
    keys: &[IndexKey],
) -> Option<u64> {
    let mut literal_keys: Vec<Value> = Vec::with_capacity(keys.len());
    let mut all_literal = true;
    for key in keys {
        match literal_value(key) {
            Some(value) => literal_keys.push(value),
            None => {
                all_literal = false;
                break;
            }
        }
    }
    if all_literal {
        catalog.composite_cardinality(target, label, properties, &literal_keys)
    } else {
        catalog.composite_avg_bucket(target, label, properties)
    }
}

/// Disjunctive-label expansion cost: Σ per-label cardinality across branches.
/// Returns `None` if any branch's label cardinality is unavailable.
pub fn disjunctive_cost(
    catalog: &dyn IndexCatalog,
    target: IndexTarget,
    labels: &[DbString],
) -> Option<u64> {
    let mut total: u64 = 0;
    for label in labels {
        total = total.saturating_add(catalog.label_cardinality(target, label.clone())?);
    }
    Some(total)
}

/// Whether the cost gate should DECLINE an index path (keeping the scan
/// `Linear`).
///
/// Declines only when the index is provably *no better* than the baseline over
/// a **non-empty** population: `baseline > 0 && index_cost >= baseline`. When
/// `baseline == 0` (degenerate / empty graph) or `index_cost < baseline` the
/// gate does NOT decline — it keeps the pre-OPT-5 structural behavior of taking
/// the index. This preserves the long-standing "a registered index is used"
/// contract on empty graphs (where index and Linear both cost ~0 and declining
/// would needlessly drop the access path) while still rejecting a
/// non-selective index whose cost equals or exceeds a full residual scan.
pub fn should_decline_index(index_cost: u64, baseline: u64) -> bool {
    baseline > 0 && index_cost >= baseline
}

/// Bound-end discriminator used when building a `(Bound, Bound)` range.
enum BoundEnd {
    Lower,
    Upper,
}

fn bound(value: Value, inclusive: bool, _end: BoundEnd) -> std::ops::Bound<Value> {
    if inclusive {
        std::ops::Bound::Included(value)
    } else {
        std::ops::Bound::Excluded(value)
    }
}

/// Resolve an [`IndexKey`] to a concrete [`Value`] when it is a non-null
/// literal; parameters and nulls return `None` (the caller routes those to the
/// average-bucket / parameter-fraction estimators).
fn literal_value(key: &IndexKey) -> Option<Value> {
    match key {
        IndexKey::Literal(literal) => literal_to_value(literal),
        IndexKey::Parameter { .. } | IndexKey::ParameterList { .. } => None,
    }
}

/// Local planner-`Literal` → `Value` lowering, mirroring
/// `runtime::scan_resolve::literal_to_value` but kept crate-internal to the
/// optimizer so the cost module has no runtime dependency. `Null` yields `None`.
pub(super) fn literal_to_value(literal: &Literal) -> Option<Value> {
    Some(match literal {
        Literal::Bool(value, _) => Value::Bool(*value),
        Literal::Integer(value, _) | Literal::RadixInteger(value, _, _) => Value::Int(*value),
        Literal::Decimal(value, _, _) => Value::Decimal(*value),
        Literal::Float(value, _, _) => Value::Float(*value),
        Literal::String(value, _, _) => Value::String(value.clone()),
        Literal::Bytes(value, _) => Value::Bytes(value.clone()),
        Literal::Uuid(value, _, _) => Value::Uuid(*value),
        Literal::ZonedDateTime(value, _, _) => Value::ZonedDateTime(value.clone()),
        Literal::LocalDateTime(value, _, _) => Value::LocalDateTime(*value),
        Literal::Date(value, _, _) => Value::Date(*value),
        Literal::ZonedTime(value, _, _) => Value::ZonedTime(value.clone()),
        Literal::LocalTime(value, _, _) => Value::LocalTime(*value),
        Literal::Duration(value, _, _) => Value::Duration(value.clone()),
        Literal::Null(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: estimator unit tests that need synthetic `DbString` labels/properties
    // live in `tests/cost_estimators.rs` (test-harness gated). Only the pure
    // numeric gate logic is tested inline here.

    #[test]
    fn should_decline_index_semantics() {
        // Cheaper index → never decline (take it).
        assert!(!should_decline_index(5, 10));
        // Equal or worse on a non-empty population → decline (stay Linear).
        assert!(should_decline_index(10, 10), "equal cost is not better");
        assert!(should_decline_index(11, 10), "more expensive");
        // Empty population (baseline 0) → never decline, even at equal 0 cost:
        // preserves the structural "registered index is used" contract.
        assert!(!should_decline_index(0, 0), "empty graph keeps the index");
    }
}
