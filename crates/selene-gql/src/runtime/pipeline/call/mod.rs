//! Procedure-call pipeline operator.

pub(crate) mod context;
pub(crate) mod project;
mod validation;

pub(crate) use validation::{validate_arguments, validate_registration};

use selene_core::Value;

use crate::{
    BindingTableSchema, PlannedCall, ProjectExpr, SubqueryRegistry,
    analyze::ExprIdLookup,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, TxContext, evaluator},
};

pub(super) fn execute(
    call: &PlannedCall,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    validate_registration(call, ctx)?;
    let registry = ctx.registry();
    let (input_schema, rows) = table.into_parts();
    let output_schema = output_schema(&input_schema, call);
    let mut output = Vec::new();
    let mut rows_since_check = 0;

    for row in rows {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let args = {
            let eval_ctx = EvalCtx {
                tx: ctx,
                expr_ids,
                subqueries,
            };
            evaluate_args(&call.args, &row, &input_schema, &eval_ctx)?
        };
        validate_arguments(call, &args)?;
        let result = {
            let deadline = ctx.deadline();
            let mut procedure_ctx = context::build(call, ctx)?;
            registry
                .execute(call.handle, &args, &mut procedure_ctx)
                .map_err(|source| context::procedure_error(source, call.span, deadline))?
        };
        if call.optional && result.rows.is_empty() {
            output.push(optional_output_row(call, &row));
            continue;
        }
        for output_row in result.rows {
            ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
            let projected = project::project_yield_row(call, output_row)?;
            output.push(row.with_appended_values(projected));
        }
    }

    Ok(BindingTable::new(output_schema, output))
}

pub(super) fn execute_read_only(
    call: &PlannedCall,
    table: BindingTable,
    ctx: &TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    validate_registration(call, ctx)?;
    let registry = ctx.registry();
    let (input_schema, rows) = table.into_parts();
    let output_schema = output_schema(&input_schema, call);
    let mut output = Vec::new();
    let mut rows_since_check = 0;

    for row in rows {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let args = {
            let eval_ctx = EvalCtx {
                tx: ctx,
                expr_ids,
                subqueries,
            };
            evaluate_args(&call.args, &row, &input_schema, &eval_ctx)?
        };
        validate_arguments(call, &args)?;
        let result = {
            let deadline = ctx.deadline();
            let mut procedure_ctx = context::build_read_only(call, ctx)?;
            registry
                .execute(call.handle, &args, &mut procedure_ctx)
                .map_err(|source| context::procedure_error(source, call.span, deadline))?
        };
        if call.optional && result.rows.is_empty() {
            output.push(optional_output_row(call, &row));
            continue;
        }
        for output_row in result.rows {
            ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
            let projected = project::project_yield_row(call, output_row)?;
            output.push(row.with_appended_values(projected));
        }
    }

    Ok(BindingTable::new(output_schema, output))
}

pub(crate) fn evaluate_args(
    args: &[ProjectExpr],
    row: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Value>, ExecutorError> {
    args.iter()
        .map(|arg| evaluator::evaluate(&arg.expr, row, schema, ctx))
        .collect()
}

pub(crate) fn output_schema(input: &BindingTableSchema, call: &PlannedCall) -> BindingTableSchema {
    let mut schema = input.clone();
    schema.columns.extend(call.yield_schema.clone());
    schema
}

pub(crate) fn optional_output_row(call: &PlannedCall, input: &Binding) -> Binding {
    input.with_appended_values(std::iter::repeat_n(Value::Null, call.yield_schema.len()))
}
