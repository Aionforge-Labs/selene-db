//! Expression, schema, and diagnostic kernels used by physical batches.
//!
//! This module contains no statement dispatch or row executor. The public
//! binding-table entry point delegates unconditionally to the batch driver.

pub(crate) mod aggregate;
pub(crate) mod call;
pub(crate) mod explain;
pub(crate) mod group_by;
mod limit;
mod match_op;
pub(crate) mod order_by;
mod project;
pub(crate) mod union;
pub(crate) mod unwind;

pub(crate) use limit::{resolve_amount, u64_to_bounded_usize};
pub(crate) use match_op::{seed_row, target_schema};
pub(crate) use project::schema_for_items;
pub(crate) use union::{assert_compatible_schemas, set_op_key_cap_exceeded};

use crate::{
    PipelineOp, SubqueryRegistry,
    analyze::ExprIdLookup,
    runtime::{BindingTable, ExecutorError, TxContext},
};

/// Execute a physical pipeline against an input binding table.
pub fn execute_pipeline(
    pipeline: &[PipelineOp],
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let expr_ids = ExprIdLookup::default();
    let subqueries = SubqueryRegistry::default();
    let (expr_ids, subqueries) = ctx.plan_metadata().unwrap_or((&expr_ids, &subqueries));
    super::batch::query::execute_pipeline(
        pipeline,
        table,
        ctx,
        expr_ids,
        subqueries,
        super::batch::policy::BatchPolicy::default_policy(),
    )
}

pub(crate) fn read_only_write_op_error() -> ExecutorError {
    ExecutorError::InvalidTransactionState {
        detail: "write pipeline op invoked from read-only subquery",
        span: crate::SourceSpan::default(),
    }
}
