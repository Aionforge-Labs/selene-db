//! Pull-based physical batch execution substrate (F04-PR01).
//!
//! This module introduces the physical operator contract and the typed
//! binding-batch representation alongside the existing row-oriented executor,
//! which remains the production path until F04-PR09. Batches are an internal
//! execution detail: nothing here is re-exported from the crate root, batch
//! positions are crate-private offsets, and results re-enter the stable
//! [`BindingTable`](super::BindingTable) API through the scan-to-result
//! tracer before any public surface observes them.
//!
//! Layout (each file owns one concern):
//!
//! - [`policy`] — configurable batch sizing without a release-promised size.
//! - [`binding_batch`] — typed column-major batches with null bitmaps, internal
//!   selection vectors, and explicit logical row counts.
//! - [`budget`] — reusable memory-budget and cancellation seams.
//! - [`operator`] — pull-based operator contract over one pinned snapshot.
//! - [`scan`] — the one working scan: node access lowered into batches.
//! - [`tracer`] — scan-to-result materialization for transition tests.
//! - [`reference`] — row-reference comparison helpers (test seam only).
//!
//! ISO/IEC 39075:2024 §4.3.6 binding-table semantics (duplicates preserved,
//! unit table versus empty table) hold independently of physical batch shape;
//! the regression tests assert identical logical tables across batch sizes.

pub(crate) mod binding_batch;
pub(crate) mod budget;
pub(crate) mod operator;
pub(crate) mod policy;
pub(crate) mod reference;
pub(crate) mod scan;
pub(crate) mod tracer;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;

pub(crate) use binding_batch::{BatchBuffer, BatchError, BindingBatch};
pub(crate) use budget::{BatchCancel, MemoryBudget, MemoryBudgetError};
pub(crate) use operator::{BatchExecutionContext, OperatorState, PhysicalOperator};
pub(crate) use policy::{BatchPolicy, BatchPolicyError};
pub(crate) use reference::{
    assert_same_rows, assert_same_schema, assert_tables_equivalent, collect_rows, descriptor_for,
};
pub(crate) use scan::{BatchNodeScan, BatchScanSpec};
pub(crate) use tracer::trace_scan_to_table;
