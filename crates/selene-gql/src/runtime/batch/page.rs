//! Batch page: OFFSET/LIMIT across batch boundaries.
//!
//! [`BatchPage`] applies a resolved `(offset, count)` window over a child
//! operator's logical rows with operator-level counters: skipping and taking
//! never reset per batch, so empty and intermediate batches cannot shift the
//! window. Amounts resolve through the row path's `resolve_amount` before
//! this operator is built, so parameter diagnostics (null, negative,
//! mistyped, out-of-range, unbound) are identical.
//!
//! The limit is applied exactly at this operator's pipeline position and is
//! never pushed below it: the child runs untruncated, and once `count` rows
//! are emitted the operator short-circuits (further pulls return end of
//! input without touching the child). A small `LIMIT` after a
//! multiplicity-producing expansion therefore returns the first rows of the
//! full expansion rather than limiting the seed.

use selene_core::Value;

use crate::{plan::BindingTableSchema, runtime::ExecutorError};

use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
};

/// Pull-based skip/take over a child operator's batches.
///
/// `offset` rows are dropped, then at most `count` rows are emitted.
/// A zero `count` emits nothing (the child is still initialized so binding
/// and candidate validation run, but never pulled).
pub(crate) struct BatchPage<'x> {
    child: Box<dyn PhysicalOperator + 'x>,
    offset: u64,
    count: u64,
    skipped: u64,
    emitted: u64,
    state: OperatorState,
}

impl<'x> BatchPage<'x> {
    /// Construct a page over `child` skipping `offset` and taking `count`.
    pub(crate) const fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        offset: u64,
        count: u64,
    ) -> Self {
        Self {
            child,
            offset,
            count,
            skipped: 0,
            emitted: 0,
            state: OperatorState::Created,
        }
    }

    /// Return rows skipped so far (operator-level, never per-batch).
    ///
    /// Test seam proving counters survive batch boundaries.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Return rows emitted so far (operator-level, never per-batch).
    ///
    /// Test seam proving counters survive batch boundaries.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Return the operator's lifecycle state.
    ///
    /// Test seam for lifecycle assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn state(&self) -> OperatorState {
        self.state
    }

    fn init_inner(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch page init is legal only once from Created",
            });
        }
        self.child.init(ctx)?;
        self.state = OperatorState::Open;
        Ok(())
    }

    fn pull_inner(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        ctx.ensure_generation()?;
        let span = crate::SourceSpan::default();
        ctx.check_cancel(span)?;
        // Short-circuit: the window is full, so end the input without
        // pulling the child again. No limit is ever pushed below this
        // operator; the child simply stops being consumed.
        if self.emitted >= self.count {
            self.state = OperatorState::Exhausted;
            return Ok(None);
        }
        let width = self.child.output_schema().columns.len();
        let mut columns: Vec<(Vec<Value>, Vec<bool>)> = Vec::with_capacity(width);
        for _ in 0..width {
            let mut values = buffer.take_values();
            let mut nulls = buffer.take_nulls();
            values.clear();
            nulls.clear();
            columns.push((values, nulls));
        }
        let mut taken = 0usize;
        while (self.emitted + taken as u64) < self.count {
            let Some(child) = self.child.next_batch(ctx, buffer)? else {
                break;
            };
            for index in 0..child.logical_rows() {
                if self.skipped < self.offset {
                    self.skipped += 1;
                    continue;
                }
                if self.emitted + taken as u64 >= self.count {
                    break;
                }
                let row = child.logical_row(index);
                for (slot, (column, nulls)) in columns.iter_mut().enumerate() {
                    let value = row.get(slot).cloned().unwrap_or(Value::Null);
                    nulls.push(value == Value::Null);
                    column.push(value);
                }
                taken += 1;
            }
            ctx.budget_mut().release(child.estimated_bytes());
            child.recycle(buffer);
            ctx.check_cancel(span)?;
        }
        if taken == 0 {
            self.state = OperatorState::Exhausted;
            // Return pooled storage before reporting end of input.
            for (values, nulls) in columns {
                buffer.recycle_column(values, nulls);
            }
            return Ok(None);
        }
        self.emitted += taken as u64;
        ctx.finish_batch(taken);
        let batch_columns = columns
            .into_iter()
            .map(|(values, nulls)| {
                BatchColumn::from_parts(values, nulls).map_err(|_| {
                    ExecutorError::ImplementationDefined {
                        detail: "batch page built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch =
            BindingBatch::from_batch_columns(self.child.output_schema().clone(), batch_columns)
                .map_err(|_| ExecutorError::ImplementationDefined {
                    detail: "batch page built a malformed batch",
                })?;
        ctx.budget_mut()
            .reserve(batch.estimated_bytes())
            .map_err(|err| err.into_executor_error(span))?;
        Ok(Some(batch))
    }
}

impl PhysicalOperator for BatchPage<'_> {
    fn init(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        let outcome = self.init_inner(ctx);
        if outcome.is_err() {
            self.state = OperatorState::Failed;
        }
        outcome
    }

    fn next_batch(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        if self.state == OperatorState::Exhausted {
            return Ok(None);
        }
        if self.state != OperatorState::Open {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch page pull is legal only while Open",
            });
        }
        let outcome = self.pull_inner(ctx, buffer);
        if outcome.is_err() {
            self.state = OperatorState::Failed;
        }
        outcome
    }

    fn close(&mut self, ctx: &mut BatchExecutionContext<'_>) {
        self.child.close(ctx);
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn output_schema(&self) -> &BindingTableSchema {
        self.child.output_schema()
    }
}
