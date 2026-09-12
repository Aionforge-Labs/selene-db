//! Scan-to-result tracer: execute one batch operator end to end.
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
//! the partial table — the half-built rows stay local and are dropped. The
//! scan operator is read-only over an immutable snapshot, so a failed run
//! cannot leave a partial mutation behind either.

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
/// scan still yields full column types and order. The operator is closed on
/// every exit path, releasing its snapshot claim.
///
/// # Errors
///
/// Returns `init`/`next_batch` failures (cancellation, budget, generation,
/// or invariant errors) after closing the operator. No partial table is
/// returned on failure.
pub(crate) fn trace_scan_to_table<S: PhysicalOperator>(
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

fn pull_all<S: PhysicalOperator>(
    operator: &mut S,
    ctx: &mut BatchExecutionContext<'_>,
    buffer: &mut BatchBuffer,
    rows: &mut Vec<Binding>,
) -> Result<(), ExecutorError> {
    while let Some(batch) = operator.next_batch(ctx, buffer)? {
        // Rows are cloned out before recycling, so returned storage
        // cannot alias materialized output. At most one batch is live
        // at any point in this loop.
        rows.extend(batch.logical_rows_vec().into_iter().map(Binding::new));
        ctx.budget_mut().release(batch.estimated_bytes());
        batch.recycle(buffer);
    }
    Ok(())
}
