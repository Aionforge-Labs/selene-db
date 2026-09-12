//! Batch projection: evaluate projection items per logical row.
//!
//! [`BatchProject`] wraps a child operator and evaluates every
//! [`ProjectExpr`](crate::ProjectExpr) against each logical row through the
//! shared [`evaluate`](crate::runtime::evaluator::evaluate) entry the row
//! projector uses, so computed values, property access, type errors, and
//! diagnostics agree with the row path by construction. The output schema is
//! the row path's `schema_for_items` for the same items (shared through the
//! pipeline projector), so declared column names, order, and types match
//! exactly.
//!
//! The child batch is recycled after its rows are projected; output columns
//! reuse buffer storage. Empty child windows cannot occur (every operator
//! skips them), but a zero-item projection over zero rows still yields an
//! empty batch with the declared schema rather than end of input while the
//! child is open.

use selene_core::Value;

use crate::{
    ProjectExpr,
    plan::BindingTableSchema,
    runtime::{Binding, EvalCtx, ExecutorError, evaluator},
};

use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
};

/// Pull-based projection over a child operator's batches.
pub(crate) struct BatchProject<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
    items: &'plan [ProjectExpr],
    output_schema: BindingTableSchema,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    state: OperatorState,
}

impl<'x, 'a, 'ctx, 'g, 'plan> BatchProject<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct a projection over `child` evaluating `items` in order.
    pub(crate) fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        items: &'plan [ProjectExpr],
        output_schema: BindingTableSchema,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    ) -> Self {
        Self {
            child,
            items,
            output_schema,
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
                detail: "batch project init is legal only once from Created",
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
        let Some(child) = self.child.next_batch(ctx, buffer)? else {
            self.state = OperatorState::Exhausted;
            return Ok(None);
        };
        let input_schema = self.child.output_schema().clone();
        let outcome = self.project_batch(&child, &input_schema, ctx, buffer);
        ctx.budget_mut().release(child.estimated_bytes());
        child.recycle(buffer);
        outcome
    }

    fn project_batch(
        &mut self,
        child: &BindingBatch,
        input_schema: &BindingTableSchema,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        let span = crate::SourceSpan::default();
        let mut columns: Vec<(Vec<Value>, Vec<bool>)> = Vec::with_capacity(self.items.len());
        for _ in self.items {
            let mut values = buffer.take_values();
            let mut nulls = buffer.take_nulls();
            values.clear();
            nulls.clear();
            columns.push((values, nulls));
        }
        for index in 0..child.logical_rows() {
            let row = Binding::new(child.logical_row(index));
            for (item, (column, nulls)) in self.items.iter().zip(columns.iter_mut()) {
                // Projection errors (wrong-type operands, missing-property
                // diagnostics with error severity, computed failures) abort
                // the pull exactly as the row projector aborts the pipeline.
                let value = evaluator::evaluate(&item.expr, &row, input_schema, &self.eval)?;
                nulls.push(value == Value::Null);
                column.push(value);
            }
        }
        let rows = child.logical_rows();
        let batch_columns = columns
            .into_iter()
            .map(|(values, nulls)| {
                BatchColumn::from_parts(values, nulls).map_err(|_| {
                    ExecutorError::ImplementationDefined {
                        detail: "batch project built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Zero-item projections still validate width: the output schema must
        // agree with the item list, so a mismatch fails here rather than
        // producing a silently misdescribed batch.
        let batch = BindingBatch::from_batch_columns(self.output_schema.clone(), batch_columns)
            .map_err(|_| ExecutorError::ImplementationDefined {
                detail: "batch project output disagrees with its schema",
            })?;
        ctx.finish_batch(rows);
        ctx.budget_mut()
            .reserve(batch.estimated_bytes())
            .map_err(|err| err.into_executor_error(span))?;
        Ok(Some(batch))
    }
}

impl PhysicalOperator for BatchProject<'_, '_, '_, '_, '_> {
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
                detail: "batch project pull is legal only while Open",
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
        &self.output_schema
    }
}
