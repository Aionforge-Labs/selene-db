//! Result comparison helpers; these do not implement reference semantics.
//!
//! The transition comparisons ran before F04-PR09 deleted the row executor.
//! Batch-shape comparisons now prove partition invariance, not independent
//! correctness. Independent relation/path/type models and clause-derived
//! expectations remain able to disagree with the engine.

use selene_core::Value;

use crate::runtime::{BindingTable, BindingTableDescriptor};

/// Collect a table's rows as owned value vectors in storage order.
#[must_use]
pub(crate) fn collect_rows(table: &BindingTable) -> Vec<Vec<Value>> {
    table
        .rows()
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

/// Assert two tables carry the same declared schema (names, types, order).
///
/// # Panics
///
/// Panics with `what` context when schemas differ.
pub(crate) fn assert_same_schema(expected: &BindingTable, actual: &BindingTable, what: &str) {
    assert_eq!(
        expected.schema(),
        actual.schema(),
        "{what}: batch output schema diverged from the row reference"
    );
}

/// Assert two row vectors match exactly, in order.
///
/// Scans over a pinned snapshot are deterministic, so order-sensitive
/// comparison is the stronger check: it pins both cardinality and order.
///
/// # Panics
///
/// Panics with `what` context and a bounded diff on mismatch.
pub(crate) fn assert_same_rows(expected: &[Vec<Value>], actual: &[Vec<Value>], what: &str) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "{what}: row count diverged (expected {}, actual {})",
        expected.len(),
        actual.len()
    );
    for (index, (want, got)) in expected.iter().zip(actual.iter()).enumerate() {
        assert_eq!(
            want, got,
            "{what}: row {index} diverged (expected {want:?}, actual {got:?})"
        );
    }
}

/// Assert two materialized tables match exactly:
/// declared schema plus every row in order.
///
/// # Panics
///
/// Panics with `what` context on any divergence.
pub(crate) fn assert_tables_equivalent(expected: &BindingTable, actual: &BindingTable, what: &str) {
    assert_same_schema(expected, actual, what);
    assert_same_rows(&collect_rows(expected), &collect_rows(actual), what);
}

/// Assert the declared descriptor preserves the table's schema types and
/// preferred column order, including for an empty table.
#[must_use]
pub(crate) fn descriptor_for(table: &BindingTable) -> BindingTableDescriptor {
    BindingTableDescriptor::from_schema(table.schema())
}
