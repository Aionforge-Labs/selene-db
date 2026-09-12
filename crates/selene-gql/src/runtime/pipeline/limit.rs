use crate::{
    LimitAmount, SourceSpan,
    runtime::{BindingTable, DataExceptionSubclass, ExecutorError, TxContext, parameter_type},
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use selene_core::Value;

pub(super) fn execute(
    offset: &LimitAmount,
    count: &LimitAmount,
    table: BindingTable,
    ctx: &TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let offset = resolve_amount(offset, ctx)?;
    let count = resolve_amount(count, ctx)?;
    let (schema, mut rows) = table.into_parts();
    ctx.check_cancellation()?;
    let start = u64_to_bounded_usize(offset, rows.len());
    let end = start.saturating_add(u64_to_bounded_usize(count, rows.len() - start));
    rows.truncate(end);
    if start == 0 {
        return Ok(BindingTable::new(schema, rows));
    }
    Ok(BindingTable::new(schema, rows.split_off(start)))
}

/// Resolve a pipeline limit/offset amount against bound parameters.
///
/// Shared with the batch page operator so both paths report identical
/// diagnostics for null, negative, mistyped, and out-of-range parameters.
pub(crate) fn resolve_amount(
    amount: &LimitAmount,
    ctx: &TxContext<'_, '_>,
) -> Result<u64, ExecutorError> {
    match amount {
        LimitAmount::Literal(value) => Ok(*value),
        LimitAmount::Parameter {
            name,
            declared_type,
            span,
        } => match ctx.parameters().get(name) {
            Some(value) => {
                reject_null_limit_value(value, *span)?;
                if let Some(declared_type) = declared_type {
                    parameter_type::validate_declared_type(
                        name.clone(),
                        value,
                        declared_type,
                        *span,
                    )?;
                }
                parameter_amount(*span, value)
            }
            None => Err(ExecutorError::UnboundParameter {
                name: name.clone(),
                span: *span,
            }),
        },
    }
}

fn parameter_amount(span: SourceSpan, value: &Value) -> Result<u64, ExecutorError> {
    match value {
        Value::Int(value) if *value >= 0 => Ok(*value as u64),
        Value::Int(_) => Err(negative_limit_value(span)),
        Value::Int128(value) if *value >= 0 => {
            u64::try_from(*value).map_err(|_| numeric_out_of_range(span))
        }
        Value::Int128(_) => Err(negative_limit_value(span)),
        Value::Uint(value) => Ok(*value),
        Value::Uint128(value) => u64::try_from(*value).map_err(|_| numeric_out_of_range(span)),
        Value::Decimal(value) => decimal_amount(value, span),
        _ => Err(invalid_limit_value_type(span)),
    }
}

fn reject_null_limit_value(value: &Value, span: SourceSpan) -> Result<(), ExecutorError> {
    if matches!(value, Value::Null) {
        return Err(ExecutorError::data_exception(
            DataExceptionSubclass::NullValueNotAllowed,
            "LIMIT/OFFSET parameter cannot be NULL",
            span,
        ));
    }
    Ok(())
}

fn decimal_amount(value: &Decimal, span: SourceSpan) -> Result<u64, ExecutorError> {
    if value.trunc() != *value {
        return Err(invalid_limit_value_type(span));
    }
    if *value < Decimal::ZERO {
        return Err(negative_limit_value(span));
    }
    value.to_u64().ok_or_else(|| numeric_out_of_range(span))
}

fn negative_limit_value(span: SourceSpan) -> ExecutorError {
    ExecutorError::data_exception(
        DataExceptionSubclass::NegativeLimitValue,
        "LIMIT/OFFSET parameter must be non-negative",
        span,
    )
}

fn invalid_limit_value_type(span: SourceSpan) -> ExecutorError {
    ExecutorError::data_exception(
        DataExceptionSubclass::InvalidValueType,
        "LIMIT/OFFSET parameter is not an exact number with scale 0",
        span,
    )
}

fn numeric_out_of_range(span: SourceSpan) -> ExecutorError {
    ExecutorError::data_exception(
        DataExceptionSubclass::NumericValueOutOfRange,
        "LIMIT/OFFSET parameter exceeds the supported range",
        span,
    )
}

/// Bound a `u64` amount to a live row count for skip/take slicing.
///
/// Shared with the batch sort operator so bounded top-K windows agree with
/// the row path exactly.
pub(crate) fn u64_to_bounded_usize(value: u64, upper_bound: usize) -> usize {
    usize::try_from(value)
        .unwrap_or(usize::MAX)
        .min(upper_bound)
}
