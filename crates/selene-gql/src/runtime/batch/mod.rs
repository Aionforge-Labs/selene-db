//! Pull-based physical batch execution substrate (F04-PR01, production since
//! F04-PR02).
//!
//! This module implements the physical operator contract and the typed
//! binding-batch representation. Primitive families (scan seed, one-hop
//! expansion, filter, project, page) execute through these operators when the
//! batch query driver ([`query`]) accepts the plan; every other operator
//! stays with the row executor until its owning F04 slice. Batches are an
//! internal execution detail: nothing here is re-exported from the crate
//! root, batch positions are crate-private offsets, and results re-enter the
//! stable [`BindingTable`](super::BindingTable) API through operator
//! materialization before any public surface observes them.
//!
//! Layout (each file owns one concern):
//!
//! - [`policy`] — configurable batch sizing without a release-promised size.
//! - [`binding_batch`] — typed column-major batches with null bitmaps, internal
//!   selection vectors, and explicit logical row counts.
//! - [`budget`] — reusable memory-budget and cancellation seams.
//! - [`operator`] — pull-based operator contract over one pinned snapshot.
//! - [`candidates`] — typed scan-candidate resolution bound to the snapshot.
//! - [`scan`] — node and edge access lowered into batches.
//! - [`expand`] — inner one-hop expansion lowered into batches.
//! - [`filter`] — predicate filtering over child batches.
//! - [`project`] — projection over child batches.
//! - [`page`] — offset/limit across batch boundaries.
//! - [`unit`] — single-row seed sources.
//! - [`tracer`] — operator-to-result materialization for tests and drivers.
//! - [`query`] — batch query driver: plan acceptance and operator assembly.
//! - [`reference`] — row-reference comparison helpers (test seam only).
//!
//! ISO/IEC 39075:2024 §4.3.6 binding-table semantics (duplicates preserved,
//! unit table versus empty table) hold independently of physical batch shape;
//! the regression tests assert identical logical tables across batch sizes.

pub(crate) mod binding_batch;
pub(crate) mod budget;
pub(crate) mod candidates;
pub(crate) mod expand;
pub(crate) mod filter;
pub(crate) mod operator;
pub(crate) mod page;
pub(crate) mod policy;
pub(crate) mod project;
pub(crate) mod query;
#[cfg(test)]
pub(crate) mod reference;
pub(crate) mod scan;
pub(crate) mod tracer;
pub(crate) mod unit;

#[cfg(test)]
mod differentials;
#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod scan_tests;
#[cfg(test)]
mod tests;

// Convenience re-exports for the transition differential tests. Production
// code addresses batch items through direct module paths so the public
// within-crate surface stays explicit about which family it consumes.
#[cfg(test)]
pub(crate) use binding_batch::{BatchBuffer, BatchError, BindingBatch};
#[cfg(test)]
pub(crate) use budget::{BatchCancel, MemoryBudget, MemoryBudgetError};
#[cfg(test)]
pub(crate) use operator::{BatchExecutionContext, OperatorState, PhysicalOperator};
#[cfg(test)]
pub(crate) use policy::{BatchPolicy, BatchPolicyError};
#[cfg(test)]
pub(crate) use reference::{
    assert_same_rows, assert_same_schema, assert_tables_equivalent, collect_rows, descriptor_for,
};
#[cfg(test)]
pub(crate) use tracer::trace_scan_to_table;
