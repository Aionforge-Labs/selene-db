//! Physical mutation stage over bounded binding inputs (F04-PR05).
//!
//! A mutation is an eager barrier, not a lazy read operator: later LIMITs may
//! not stop its writes, and the next operation observes all preceding writes.
//! The input comes from the query driver or a remaining row-family adapter.
//! Bounded inputs retain `Binding` insert-site metadata (including anonymous
//! edge endpoints), which read-only column batches intentionally do not carry.
//! No snapshot borrow survives a mutation: each kernel reads the working graph
//! through the same borrowed `TxContext`, then uses its existing mutator.
//!
//! Batch completion is NOT transaction completion. Only the statement/facade
//! owner prepares or commits, validates the final graph and indexes, and handles
//! rollback. An error exposes no output table and never triggers a row retry.

use crate::{
    MutationOp, SubqueryRegistry,
    analyze::ExprIdLookup,
    runtime::{Binding, BindingTable, ExecutorError, TxContext},
};

use super::policy::BatchPolicy;

mod kernel;

/// One physical mutation, borrowing intent and expression metadata only.
/// Consuming execution prevents an operator instance being retried after error.
pub(crate) struct PhysicalMutation<'p> {
    op: &'p MutationOp,
    expr_ids: &'p ExprIdLookup,
    subqueries: &'p SubqueryRegistry,
    policy: BatchPolicy,
}

impl<'p> PhysicalMutation<'p> {
    pub(crate) fn new(
        op: &'p MutationOp,
        expr_ids: &'p ExprIdLookup,
        subqueries: &'p SubqueryRegistry,
        policy: BatchPolicy,
    ) -> Self {
        Self {
            op,
            expr_ids,
            subqueries,
            policy,
        }
    }

    pub(crate) fn execute(
        self,
        table: BindingTable,
        ctx: &mut TxContext<'_, '_>,
    ) -> Result<BindingTable, ExecutorError> {
        self.execute_observing(table, ctx, |_, _| {})
    }

    // The observer is a deterministic test seam for batch shape and cancellation.
    // Production monomorphizes the no-op callback above; it owns no request state.
    pub(super) fn execute_observing(
        self,
        table: BindingTable,
        ctx: &mut TxContext<'_, '_>,
        mut after_batch: impl FnMut(usize, &TxContext<'_, '_>),
    ) -> Result<BindingTable, ExecutorError> {
        ctx.check_cancellation()?;
        // DELETE resolves one deduplicated target set before touching the graph.
        // A later batch may name an incident edge needed by an earlier node;
        // checking detachment per batch would change the statement's semantics.
        if matches!(self.op, MutationOp::DeleteTargets { .. }) || table.row_count() == 0 {
            let output = self.stage(table, ctx)?;
            ctx.check_cancellation()?;
            return Ok(output);
        }
        let (input_schema, rows) = table.into_parts();
        let batch_rows = self.policy.rows_per_batch(
            std::mem::size_of::<Binding>().saturating_add(
                input_schema
                    .columns
                    .len()
                    .saturating_mul(std::mem::size_of::<selene_core::Value>()),
            ),
        );
        let mut output = Vec::with_capacity(rows.len());
        let mut output_schema = input_schema.clone();
        let mut rows = rows.into_iter();
        loop {
            let batch: Vec<_> = rows.by_ref().take(batch_rows).collect();
            if batch.is_empty() {
                break;
            }
            ctx.check_cancellation()?;
            let staged = self.stage(BindingTable::new(input_schema.clone(), batch), ctx)?;
            let (schema, batch) = staged.into_parts();
            output_schema = schema;
            after_batch(batch.len(), ctx);
            output.extend(batch);
            ctx.check_cancellation()?;
        }
        Ok(BindingTable::new(output_schema, output))
    }

    fn stage(
        &self,
        table: BindingTable,
        ctx: &mut TxContext<'_, '_>,
    ) -> Result<BindingTable, ExecutorError> {
        kernel::execute(self.op, table, ctx, self.expr_ids, self.subqueries)
    }
}
