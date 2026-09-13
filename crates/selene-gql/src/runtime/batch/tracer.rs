//! Operator-to-result tracer: execute one batch operator end to end.
//!
//! The tracer pulls an operator to completion and materializes the batches
//! into a [`BindingTable`], which re-enters the existing stable result API
//! (`StatementOutput::Rows`, `ExecutionOutcome::from_statement`). No new
//! public streaming API is introduced: batches never escape this function
//! except back into the recycled buffer.
//!
//! Materialize-then-recycle keeps at most one batch live: each consumed batch
//! returns its storage to the buffer before the next pull. On cancellation or
//! error the tracer closes the operator and returns `Err` without exposing
//! the partial table — the half-built rows stay local and are dropped. Batch
//! operators are read-only over the pinned snapshot, so a failed run cannot
//! leave a partial mutation behind either.

use crate::{
    plan::BindingTableSchema,
    runtime::{Binding, BindingTable, ExecutorError},
};

use super::{
    binding_batch::BatchBuffer,
    operator::{BatchExecutionContext, PhysicalOperator},
};

/// Pull `operator` to completion and materialize its batches as row storage.
///
/// The returned table carries the operator's declared schema, so an empty
/// result still yields full column types and order. The operator is closed on
/// every exit path, releasing its snapshot claim.
///
/// # Errors
///
/// Returns `init`/`next_batch` failures (cancellation, budget, generation,
/// or invariant errors) after closing the operator. No partial table is
/// returned on failure.
pub(crate) fn trace_operator_to_table<S: PhysicalOperator + ?Sized>(
    operator: &mut S,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<BindingTable, ExecutorError> {
    // A failed init still requires close: release the snapshot claim before
    // reporting the error.
    if let Err(err) = operator.init(ctx) {
        operator.close(ctx);
        return Err(err);
    }
    let schema: BindingTableSchema = operator.output_schema().clone();
    let mut buffer = BatchBuffer::new();
    let mut rows: Vec<Binding> = Vec::new();
    let pull = pull_all(operator, ctx, &mut buffer, &mut rows);
    operator.close(ctx);
    pull?;
    Ok(BindingTable::new(schema, rows))
}

/// Pull `operator` to completion and materialize its batches as row storage.
///
/// Scan-named historical entry over [`trace_operator_to_table`]; retained
/// for transition tests naming the scan path explicitly.
#[cfg(test)]
pub(crate) fn trace_scan_to_table<S: PhysicalOperator + ?Sized>(
    operator: &mut S,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<BindingTable, ExecutorError> {
    trace_operator_to_table(operator, ctx)
}

fn pull_all<S: PhysicalOperator + ?Sized>(
    operator: &mut S,
    ctx: &mut BatchExecutionContext<'_>,
    buffer: &mut BatchBuffer,
    rows: &mut Vec<Binding>,
) -> Result<(), ExecutorError> {
    while let Some(batch) = operator.next_batch(ctx, buffer)? {
        // Rows are cloned out before recycling, so returned storage
        // cannot alias materialized output. At most one batch is live
        // at any point in this loop.
        rows.extend((0..batch.logical_rows()).map(|index| batch.logical_binding(index)));
        ctx.budget_mut().release(batch.estimated_bytes());
        batch.recycle(buffer);
    }
    Ok(())
}
