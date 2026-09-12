//! Pull-based physical operator contract over one pinned snapshot.
//!
//! Every execution owns exactly one [`BatchExecutionContext`], which holds the
//! single request-owned graph snapshot for the whole run. Operators receive
//! the context on every call and must never substitute another snapshot: each
//! pull revalidates the pinned generation, so a batch can never silently
//! switch graph generation between pulls. (`Arc<SeleneGraph>` snapshots are
//! immutable, so the check is structural today and guards future refactors
//! that might thread a different snapshot through.)
//!
//! Operator lifecycle is explicit: [`PhysicalOperator::init`] once,
//! [`PhysicalOperator::next_batch`] until it returns `Ok(None)`, then
//! [`PhysicalOperator::close`]. `close` runs on every exit path including
//! cancellation and error, releasing the pinned snapshot and buffer claims.
//! [`OperatorState`] makes the lifecycle observable to tests.

#[cfg(test)]
use std::sync::Arc;

use selene_graph::SeleneGraph;

use crate::{SourceSpan, plan::BindingTableSchema, runtime::ExecutorError};

use super::{
    binding_batch::{BatchBuffer, BindingBatch},
    budget::{BatchCancel, MemoryBudget},
};

/// Observable lifecycle state of one physical operator instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperatorState {
    /// Constructed but not initialized.
    Created,
    /// Initialized; pulls may proceed.
    Open,
    /// The final batch was produced; further pulls return `Ok(None)`.
    Exhausted,
    /// A pull failed; only `close` is legal afterwards.
    Failed,
    /// Closed; the snapshot is released.
    Closed,
}

/// Request-owned context for one batch execution.
///
/// This bundles the pinned snapshot, the reusable cancellation seam, and the
/// reusable memory budget so later operators take one parameter instead of
/// three. The snapshot is `Option` only to model release: `close` takes it to
/// `None`, and any later access fails loudly instead of observing a replaced
/// graph.
///
/// Snapshots pin in two ways: [`Self::new`] retains an `Arc` (the F04-PR01
/// transition form, used by unit tests), while [`Self::borrowed`] pins the
/// statement snapshot by reference. Production drivers use the borrowed form
/// so batch operators observe the same working graph the row executor would
/// (including uncommitted writes in an explicit transaction); both forms
/// revalidate the pinned generation on every pull.
#[derive(Debug)]
pub(crate) struct BatchExecutionContext<'a> {
    snapshot: Option<SnapshotPin<'a>>,
    generation: u64,
    cancel: BatchCancel<'a>,
    budget: MemoryBudget,
    completed_batches: u64,
    completed_rows: usize,
}

/// How one batch execution pins its graph snapshot.
#[derive(Debug)]
enum SnapshotPin<'a> {
    /// Retained snapshot handle (test form since F04-PR02 production pins
    /// the statement snapshot by reference).
    #[cfg(test)]
    Owned(Arc<SeleneGraph>),
    /// Statement snapshot borrowed for the execution (production form).
    Borrowed(&'a SeleneGraph),
}

impl SnapshotPin<'_> {
    fn as_graph(&self) -> &SeleneGraph {
        match self {
            #[cfg(test)]
            Self::Owned(snapshot) => snapshot,
            Self::Borrowed(snapshot) => snapshot,
        }
    }
}

impl<'a> BatchExecutionContext<'a> {
    /// Pin `snapshot` for the whole execution and record its generation.
    ///
    /// Test form: production pins the statement snapshot by reference
    /// through [`Self::borrowed`].
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new(
        snapshot: Arc<SeleneGraph>,
        cancel: BatchCancel<'a>,
        budget: MemoryBudget,
    ) -> Self {
        let generation = snapshot.meta.generation;
        Self {
            snapshot: Some(SnapshotPin::Owned(snapshot)),
            generation,
            cancel,
            budget,
            completed_batches: 0,
            completed_rows: 0,
        }
    }

    /// Pin `snapshot` by reference for one batch execution.
    ///
    /// The caller guarantees the borrow covers the whole execution (the
    /// statement context outlives its batch driver call). Generation
    /// revalidation and release behave exactly as in the owned form.
    #[must_use]
    pub(crate) fn borrowed(
        snapshot: &'a SeleneGraph,
        cancel: BatchCancel<'a>,
        budget: MemoryBudget,
    ) -> Self {
        let generation = snapshot.meta.generation;
        Self {
            snapshot: Some(SnapshotPin::Borrowed(snapshot)),
            generation,
            cancel,
            budget,
            completed_batches: 0,
            completed_rows: 0,
        }
    }

    /// Borrow the pinned snapshot, failing loudly after `close`.
    ///
    /// # Errors
    ///
    /// Returns `ImplementationDefined` when the context is already closed.
    pub(crate) fn snapshot(&self) -> Result<&SeleneGraph, ExecutorError> {
        self.snapshot.as_ref().map(SnapshotPin::as_graph).ok_or(
            ExecutorError::ImplementationDefined {
                detail: "batch execution context is closed",
            },
        )
    }

    /// Return the generation pinned at construction.
    ///
    /// Test seam for pin assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    /// Revalidate that the held snapshot is still the pinned generation.
    ///
    /// Operators call this on every pull before producing rows.
    ///
    /// # Errors
    ///
    /// Returns `ImplementationDefined` when closed or when the generation
    /// moved, which must never happen for an immutable snapshot.
    pub(crate) fn ensure_generation(&self) -> Result<(), ExecutorError> {
        let snapshot = self.snapshot()?;
        if snapshot.meta.generation != self.generation {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch snapshot generation changed between pulls",
            });
        }
        Ok(())
    }

    /// Run the cooperative cancellation checkpoint.
    ///
    /// # Errors
    ///
    /// Returns the mapped `Cancelled`, `Timeout`, or scan-budget error.
    pub(crate) fn check_cancel(&self, span: SourceSpan) -> Result<(), ExecutorError> {
        self.cancel.check(span)
    }

    /// Check cancellation and account for `nodes` scanned graph nodes.
    ///
    /// # Errors
    ///
    /// Returns the mapped executor error for the first tripped limit.
    pub(crate) fn note_nodes_scanned(
        &self,
        nodes: usize,
        span: SourceSpan,
    ) -> Result<(), ExecutorError> {
        self.cancel.note_nodes_scanned(nodes, span)
    }

    /// Borrow the memory budget mutably for reserve/release.
    #[must_use]
    pub(crate) const fn budget_mut(&mut self) -> &mut MemoryBudget {
        &mut self.budget
    }

    /// Return currently reserved estimated bytes.
    ///
    /// Test seam for budget-accounting assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn budget_used(&self) -> usize {
        self.budget.used_bytes()
    }

    /// Return the high-water mark of reserved estimated bytes.
    ///
    /// Test seam for the performance probe.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn budget_peak(&self) -> usize {
        self.budget.peak_bytes()
    }

    /// Spawn a nested execution context over the same pinned snapshot.
    ///
    /// Correlated operators (outer joins, non-leading matches, per-row `NEXT`
    /// blocks) evaluate one subtree per input row. Each evaluation runs in a
    /// nested context so operator `close` releases only the nested snapshot
    /// claim while the parent execution keeps its pin. Cancellation flows
    /// from the same token, deadline, and scan budget; memory accounting
    /// stays with the parent, which reserves every materialized row it keeps
    /// (nested scratch is transient and balanced before the nested context
    /// closes).
    ///
    /// # Errors
    ///
    /// Returns `ImplementationDefined` when the parent context is closed.
    pub(crate) fn nested(&self) -> Result<BatchExecutionContext<'_>, ExecutorError> {
        Ok(BatchExecutionContext::borrowed(
            self.snapshot()?,
            self.cancel,
            MemoryBudget::unlimited(),
        ))
    }

    /// Record one completed batch for execution telemetry.
    pub(crate) fn finish_batch(&mut self, rows: usize) {
        self.completed_batches += 1;
        self.completed_rows = self.completed_rows.saturating_add(rows);
    }

    /// Return completed batch and row counts.
    ///
    /// Test seam for completion assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn completed(&self) -> (u64, usize) {
        (self.completed_batches, self.completed_rows)
    }

    /// Release the pinned snapshot. Idempotent; safe on every exit path.
    pub(crate) fn close(&mut self) {
        self.snapshot = None;
    }

    /// Return true after `close` released the snapshot.
    ///
    /// Test seam for release assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn is_closed(&self) -> bool {
        self.snapshot.is_none()
    }
}

/// Pull-based physical operator over binding batches.
///
/// Implementations resolve their input (typed candidates, child batches)
/// once in `init` from the context's pinned snapshot, then serve bounded
/// batches from that captured state. They must call
/// [`BatchExecutionContext::ensure_generation`] on every pull and check
/// cancellation at batch boundaries.
pub(crate) trait PhysicalOperator {
    /// Resolve inputs against the pinned snapshot and open the operator.
    ///
    /// # Errors
    ///
    /// Returns the executor error for resolution, cancellation, or budget
    /// failures. A failed `init` still requires `close`.
    fn init(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError>;

    /// Produce the next batch, reusing `buffer` scratch for column storage.
    ///
    /// Returns `Ok(None)` at end of input. After an error the operator is
    /// failed and only `close` is legal.
    ///
    /// # Errors
    ///
    /// Returns cancellation, budget, generation, or invariant errors without
    /// exposing partial operator state to the caller.
    fn next_batch(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError>;

    /// Release operator-held state and close the execution context.
    ///
    /// Must be safe to call after `init` failure, after a pull error, and
    /// more than once.
    fn close(&mut self, ctx: &mut BatchExecutionContext<'_>);

    /// Borrow the declared output schema.
    ///
    /// The tracer clones this up front so empty results keep full column
    /// types and order.
    fn output_schema(&self) -> &BindingTableSchema;
}

impl<P: PhysicalOperator + ?Sized> PhysicalOperator for Box<P> {
    fn init(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        (**self).init(ctx)
    }

    fn next_batch(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        (**self).next_batch(ctx, buffer)
    }

    fn close(&mut self, ctx: &mut BatchExecutionContext<'_>) {
        (**self).close(ctx);
    }

    fn output_schema(&self) -> &BindingTableSchema {
        (**self).output_schema()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_pins_generation_and_releases_on_close() {
        let graph = Arc::new(SeleneGraph::new(selene_core::GraphId::new(4_101)));
        let generation = graph.meta.generation;
        let mut ctx = BatchExecutionContext::new(
            graph.clone(),
            BatchCancel::disabled(),
            MemoryBudget::unlimited(),
        );
        // `generation` takes self by value in this test scope copy.
        assert_eq!(ctx.generation(), generation);
        ctx.ensure_generation().unwrap();
        assert_eq!(Arc::strong_count(&graph), 2);
        ctx.close();
        assert!(ctx.is_closed());
        assert_eq!(Arc::strong_count(&graph), 1);
        assert!(ctx.snapshot().is_err());
        // Closing twice is safe.
        ctx.close();
    }
}
