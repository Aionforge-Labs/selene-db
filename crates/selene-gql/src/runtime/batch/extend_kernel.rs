//! Per-binding expression kernels used only by the physical batch extension.

use super::policy::BatchPolicy;
use crate::runtime::pipeline::unwind::position_value;
use crate::{
    AnalyzedType, BindingTableColumn, BindingTableSchema, GqlType, PipelineOp,
    runtime::{Binding, DataExceptionSubclass, EvalCtx, ExecutorError, evaluator, parameter_type},
};
use selene_core::Value;

pub(super) fn output_schema(
    op: &PipelineOp,
    input: &BindingTableSchema,
) -> Result<BindingTableSchema, ExecutorError> {
    let mut schema = input.clone();
    match op {
        PipelineOp::Let(items) => {
            schema
                .columns
                .extend(items.iter().map(|item| BindingTableColumn {
                    name: item.alias.clone(),
                    hidden: None,
                    ty: item.ty.clone(),
                }))
        }
        PipelineOp::Unwind {
            source,
            alias,
            position,
            ..
        } => {
            let ty = match &source.ty {
                AnalyzedType::Resolved(GqlType::List(inner))
                | AnalyzedType::Resolved(GqlType::BoundedList {
                    element_type: inner,
                    ..
                }) => AnalyzedType::Resolved((**inner).clone()),
                _ => AnalyzedType::DYNAMIC,
            };
            schema.columns.push(BindingTableColumn {
                name: Some(alias.clone()),
                hidden: None,
                ty,
            });
            if let Some(position) = position {
                schema.columns.push(BindingTableColumn {
                    name: Some(position.alias.clone()),
                    hidden: None,
                    ty: AnalyzedType::Resolved(GqlType::Integer),
                });
            }
        }
        PipelineOp::CallSubquery(call) => schema.columns.extend(call.yield_schema.clone()),
        _ => return Err(invalid_op()),
    }
    Ok(schema)
}

pub(super) fn extend(
    op: &PipelineOp,
    row: &Binding,
    input: &BindingTableSchema,
    eval: EvalCtx<'_, '_, '_, '_>,
    policy: BatchPolicy,
    mut emit: impl FnMut(Binding) -> Result<(), ExecutorError>,
) -> Result<(), ExecutorError> {
    match op {
        PipelineOp::Let(items) => {
            let mut current = row.clone();
            let mut schema = input.clone();
            for item in items {
                let value = evaluator::evaluate(&item.expr, &current, &schema, &eval)?;
                if let (Some(declared), Some(alias)) = (&item.declared_type, &item.alias) {
                    parameter_type::validate_declared_type(
                        alias.clone(),
                        &value,
                        declared,
                        item.span,
                    )?;
                }
                current = current.with_appended_values([value]);
                schema.columns.push(BindingTableColumn {
                    name: item.alias.clone(),
                    hidden: None,
                    ty: item.ty.clone(),
                });
            }
            emit(Binding::new(current.values().to_vec()))
        }
        PipelineOp::Unwind {
            source,
            position,
            span,
            ..
        } => match evaluator::evaluate(&source.expr, row, input, &eval)? {
            Value::List(values) => {
                for (index, value) in values.into_iter().enumerate() {
                    eval.tx.check_cancellation()?;
                    let mut output = row.values().to_vec();
                    output.push(value);
                    if let Some(position) = position {
                        output.push(position_value(position.kind, index, *span)?);
                    }
                    emit(Binding::new(output))?;
                }
                Ok(())
            }
            Value::Null => Ok(()),
            _ => Err(ExecutorError::data_exception(
                DataExceptionSubclass::InvalidValueType,
                "row expansion requires a list value",
                *span,
            )),
        },
        PipelineOp::CallSubquery(call) => {
            super::table_call::extend(call, row, input, eval.tx, policy, emit)
        }
        _ => Err(invalid_op()),
    }
}

fn invalid_op() -> ExecutorError {
    ExecutorError::ImplementationDefined {
        detail: "non-extension operation in physical extension",
    }
}
