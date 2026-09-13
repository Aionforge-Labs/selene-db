//! Native-call argument, result, registration, and authority kernels.

pub(crate) mod context;
pub(crate) mod project;
mod validation;
pub(crate) use validation::{validate_arguments, validate_registration};

use crate::{
    BindingTableSchema, PlannedCall, ProjectExpr,
    runtime::{Binding, EvalCtx, ExecutorError, evaluator},
};
use selene_core::Value;

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
