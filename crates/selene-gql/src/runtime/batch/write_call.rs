//! Eager effectful native call barrier with bounded inputs and one transaction.

use super::policy::BatchPolicy;
use crate::{
    PlannedCall, SubqueryRegistry,
    analyze::ExprIdLookup,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, TxContext, pipeline::call},
};

pub(super) fn execute(
    call: &PlannedCall,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
    policy: BatchPolicy,
) -> Result<BindingTable, ExecutorError> {
    call::validate_registration(call, ctx)?;
    ctx.check_cancellation()?;
    let registry = ctx.registry();
    let (input_schema, rows) = table.into_parts();
    let output_schema = call::output_schema(&input_schema, call);
    let width = std::mem::size_of::<Binding>().saturating_add(
        input_schema
            .columns
            .len()
            .saturating_mul(std::mem::size_of::<selene_core::Value>()),
    );
    let mut output = Vec::new();
    for batch in rows.chunks(policy.rows_per_batch(width)) {
        for row in batch {
            ctx.check_cancellation()?;
            let args = call::evaluate_args(
                &call.args,
                row,
                &input_schema,
                &EvalCtx {
                    tx: ctx,
                    expr_ids,
                    subqueries,
                },
            )?;
            call::validate_arguments(call, &args)?;
            let deadline = ctx.deadline();
            let result = {
                let mut authority = call::context::build(call, ctx)?;
                registry
                    .execute(call.handle, &args, &mut authority)
                    .map_err(|error| call::context::procedure_error(error, call.span, deadline))?
            };
            if call.optional && result.rows.is_empty() {
                output.push(call::optional_output_row(call, row));
            } else {
                for values in result.rows {
                    ctx.check_cancellation()?;
                    output.push(
                        row.with_appended_values(call::project::project_yield_row(call, values)?),
                    );
                }
            }
        }
    }
    ctx.check_cancellation()?;
    Ok(BindingTable::new(output_schema, output))
}
