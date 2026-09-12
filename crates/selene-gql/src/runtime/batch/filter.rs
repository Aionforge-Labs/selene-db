//! Batch filter: retain rows satisfying one semantic predicate.
//!
//! [`BatchFilter`] wraps a child operator and keeps only the logical rows
//! whose predicate evaluates to `True`, using the shared
//! [`evaluate`](crate::runtime::evaluator::evaluate) entry the row filter
//! uses. `False`, `Null`, and every other value are dropped; evaluation
//! errors propagate unchanged (never suppressed into drops). An
//! index-consumed predicate is a planner bug in both paths and reports the
//! same `ImplementationDefined` diagnostic as the row filter.
//!
//! Filtering reuses the child batch storage in place through the selection
//! vector ([`BindingBatch::select`](super::binding_batch::BindingBatch::select)):
//! physical columns are never rewritten, null bitmaps stay aligned by
//! construction, and empty windows are skipped inside the operator so parents
//! only observe non-empty batches or end of input.

use selene_core::Value;

use crate::{
    FilterPredicate,
    plan::BindingTableSchema,
    runtime::{Binding, EvalCtx, ExecutorError, evaluator},
};

use super::{
    binding_batch::{BatchBuffer, BindingBatch},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
};

/// Pull-based filter over a child operator's batches.
///
/// The output schema is the child's schema (filtering never reshapes rows).
pub(crate) struct BatchFilter<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
    predicate: &'plan FilterPredicate,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    state: OperatorState,
}

impl<'x, 'a, 'ctx, 'g, 'plan> BatchFilter<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct a filter over `child` retaining `predicate` rows.
    pub(crate) const fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        predicate: &'plan FilterPredicate,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    ) -> Self {
        Self {
            child,
            predicate,
            eval,
            state: OperatorState::Created,
        }
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
                detail: "batch filter init is legal only once from Created",
            });
        }
        if self.predicate.index_consumed {
            return Err(ExecutorError::ImplementationDefined {
                detail: "index-consumed predicate emitted into pipeline",
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
        loop {
            let Some(mut batch) = self.child.next_batch(ctx, buffer)? else {
                self.state = OperatorState::Exhausted;
                return Ok(None);
            };
            let schema = self.child.output_schema().clone();
            let keep = keep_mask(&batch, &schema, self.predicate, &self.eval)?;
            batch
                .select(&keep, buffer)
                .map_err(|_| ExecutorError::ImplementationDefined {
                    detail: "batch filter built a malformed selection",
                })?;
            if batch.logical_rows() > 0 {
                ctx.finish_batch(batch.logical_rows());
                return Ok(Some(batch));
            }
            batch.recycle(buffer);
            ctx.check_cancel(span)?;
        }
    }
}

impl PhysicalOperator for BatchFilter<'_, '_, '_, '_, '_> {
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
                detail: "batch filter pull is legal only while Open",
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

/// Evaluate the predicate once per logical row: only `True` is kept.
///
/// Any other value (`False`, `Null`, or a non-boolean the analyzer left
/// unrejected) drops the row, exactly as the row filter does. Evaluation
/// errors return immediately and are never converted into drops.
fn keep_mask(
    batch: &BindingBatch,
    schema: &BindingTableSchema,
    predicate: &FilterPredicate,
    eval: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<bool>, ExecutorError> {
    let mut keep = Vec::with_capacity(batch.logical_rows());
    for index in 0..batch.logical_rows() {
        let row = Binding::new(batch.logical_row(index));
        let retained = match evaluator::evaluate(&predicate.expr, &row, schema, eval)? {
            Value::Bool(true) => true,
            Value::Bool(false) | Value::Null => false,
            _ => false,
        };
        keep.push(retained);
    }
    Ok(keep)
}
