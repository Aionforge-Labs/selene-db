//! String and adjacent scalar function evaluation.

use std::sync::Arc;

use rust_decimal::{Decimal, prelude::ToPrimitive};
use selene_core::Value;
use unicode_normalization::UnicodeNormalization;

use crate::{
    BinaryOp, NormalForm, SourceSpan, TrimSpec, ValueExpr,
    runtime::{Binding, BindingTableSchema, DataExceptionSubclass, EvalCtx, ExecutorError},
};

use super::{
    binary_ops::{
        data_exception, data_exception_value, data_exception_value_with, data_exception_with,
        eval_equality, string_slice, string_value,
    },
    evaluate,
};

#[derive(Clone, Copy)]
pub(super) enum TrimSide {
    Leading,
    Trailing,
    Both,
}

impl From<TrimSpec> for TrimSide {
    fn from(value: TrimSpec) -> Self {
        match value {
            TrimSpec::Leading => Self::Leading,
            TrimSpec::Trailing => Self::Trailing,
            TrimSpec::Both => Self::Both,
        }
    }
}

pub(super) fn eval_fixed_args(
    name: &str,
    args: &[ValueExpr],
    expected: usize,
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Value>, ExecutorError> {
    eval_range_args(name, args, expected..=expected, span, binding, schema, ctx)
}

pub(super) fn eval_range_args(
    name: &str,
    args: &[ValueExpr],
    arity: std::ops::RangeInclusive<usize>,
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Value>, ExecutorError> {
    let min = *arity.start();
    let max = *arity.end();
    if args.len() < min || args.len() > max {
        return Err(ExecutorError::FunctionArityMismatch {
            name: name.to_owned(),
            expected: arity_expected(min, max),
            actual: args.len(),
            span,
        });
    }
    args.iter()
        .map(|arg| evaluate(arg, binding, schema, ctx))
        .collect()
}

fn arity_expected(min: usize, max: usize) -> &'static str {
    if min == max {
        return match min {
            0 => "0",
            1 => "1",
            2 => "2",
            3 => "3",
            _ => "fixed",
        };
    }
    match (min, max) {
        (1, 2) => "1 or 2",
        (2, 3) => "2 or 3",
        (2, 65) => "2 to 65",
        _ => "variable",
    }
}

pub(super) fn eval_length(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    let value = args.into_iter().next().expect("arity checked");
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let Some(value) = string_slice(&value) else {
        return data_exception("character length argument is not a string", span);
    };
    Ok(Value::Int(value.chars().count() as i64))
}

pub(super) fn eval_byte_length(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    let value = args.into_iter().next().expect("arity checked");
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Bytes(value) = value else {
        return data_exception("byte length argument is not a byte string", span);
    };
    Ok(Value::Int(value.len() as i64))
}

pub(super) fn eval_left_right(
    args: Vec<Value>,
    span: SourceSpan,
    from_right: bool,
) -> Result<Value, ExecutorError> {
    let source = &args[0];
    let count = &args[1];
    if matches!(source, Value::Null) || matches!(count, Value::Null) {
        return Ok(Value::Null);
    }
    let count = string_length_count(count, span)?;
    if let Some(source) = string_slice(source) {
        return eval_left_right_string(source, count, from_right, span);
    }
    if let Value::Bytes(source) = source {
        return Ok(eval_left_right_bytes(source, count, from_right));
    }
    data_exception("LEFT/RIGHT source is not a string or byte string", span)
}

fn eval_left_right_string(
    source: &str,
    count: usize,
    from_right: bool,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let chars: Vec<char> = source.chars().collect();
    let value: String = if from_right {
        let start = chars.len().saturating_sub(count);
        chars[start..].iter().copied().collect()
    } else {
        chars.iter().take(count).copied().collect()
    };
    string_value(&value, span)
}

fn eval_left_right_bytes(source: &Arc<[u8]>, count: usize, from_right: bool) -> Value {
    let slice = if from_right {
        let start = source.len().saturating_sub(count);
        &source[start..]
    } else {
        &source[..source.len().min(count)]
    };
    Value::Bytes(Arc::<[u8]>::from(slice))
}

pub(super) fn eval_multi_char_trim(
    args: Vec<Value>,
    span: SourceSpan,
    side: TrimSide,
) -> Result<Value, ExecutorError> {
    let source = &args[0];
    if matches!(source, Value::Null) {
        return Ok(Value::Null);
    }
    if args
        .get(1)
        .is_some_and(|trim_chars| matches!(trim_chars, Value::Null))
    {
        return Ok(Value::Null);
    }
    let Some(source) = string_slice(source) else {
        return data_exception("trim source is not a string", span);
    };
    let trim_chars = if let Some(trim_chars) = args.get(1) {
        string_slice(trim_chars)
            .ok_or_else(|| data_exception_value("trim characters are not a string", span))?
    } else {
        " "
    };
    string_value(&trim_by_char_set(source, trim_chars, side), span)
}

pub(super) fn eval_explicit_trim(
    source: Value,
    character: Option<Value>,
    side: TrimSide,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    if matches!(source, Value::Null)
        || character
            .as_ref()
            .is_some_and(|value| matches!(value, Value::Null))
    {
        return Ok(Value::Null);
    }
    match source {
        Value::String(source) => eval_explicit_trim_string(source.as_str(), character, side, span),
        Value::Bytes(source) => eval_explicit_trim_bytes(&source, character, side, span),
        _ => data_exception("trim source is not a string or byte string", span),
    }
}

fn eval_explicit_trim_string(
    source: &str,
    character: Option<Value>,
    side: TrimSide,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let character = if let Some(character) = character {
        let Some(character) = string_slice(&character) else {
            return Err(data_exception_value_with(
                DataExceptionSubclass::ValuesNotComparable,
                "trim character is not comparable with source string",
                span,
            ));
        };
        let mut chars = character.chars();
        let Some(value) = chars.next() else {
            return Err(data_exception_value_with(
                DataExceptionSubclass::TrimError,
                "trim character must contain exactly one character",
                span,
            ));
        };
        if chars.next().is_some() {
            return Err(data_exception_value_with(
                DataExceptionSubclass::TrimError,
                "trim character must contain exactly one character",
                span,
            ));
        }
        value.to_string()
    } else {
        " ".to_owned()
    };
    string_value(&trim_by_char_set(source, &character, side), span)
}

fn eval_explicit_trim_bytes(
    source: &Arc<[u8]>,
    character: Option<Value>,
    side: TrimSide,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let trim_byte = if let Some(character) = character {
        let Value::Bytes(character) = character else {
            return Err(data_exception_value_with(
                DataExceptionSubclass::ValuesNotComparable,
                "trim character is not comparable with source byte string",
                span,
            ));
        };
        let [trim_byte] = character.as_ref() else {
            return Err(data_exception_value_with(
                DataExceptionSubclass::TrimError,
                "trim byte string must contain exactly one byte",
                span,
            ));
        };
        *trim_byte
    } else {
        b' '
    };

    let mut start = 0;
    let mut end = source.len();
    if matches!(side, TrimSide::Leading | TrimSide::Both) {
        start = source
            .iter()
            .position(|byte| *byte != trim_byte)
            .unwrap_or(source.len());
    }
    if start < end && matches!(side, TrimSide::Trailing | TrimSide::Both) {
        end = source
            .iter()
            .rposition(|byte| *byte != trim_byte)
            .map_or(start, |index| index + 1);
    }
    Ok(Value::Bytes(Arc::<[u8]>::from(&source[start..end])))
}

pub(super) fn eval_string_transform(
    args: Vec<Value>,
    span: SourceSpan,
    transform: fn(&str) -> String,
    max_string_length: u32,
) -> Result<Value, ExecutorError> {
    let value = args.into_iter().next().expect("arity checked");
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let Some(value) = string_slice(&value) else {
        return data_exception("string function argument is not a string", span);
    };
    let transformed = transform(value);
    let output = if is_normalized(value, NormalForm::Nfc) {
        normalize_string(&transformed, NormalForm::Nfc)
    } else {
        transformed
    };
    capped_string_function_value(&output, max_string_length, span, "string fold result")
}

pub(super) fn eval_normalize(
    value: Value,
    form: Option<NormalForm>,
    span: SourceSpan,
    max_string_length: u32,
) -> Result<Value, ExecutorError> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let Some(value) = string_slice(&value) else {
        return data_exception("normalize argument is not a string", span);
    };
    capped_string_function_value(
        &normalize_string(value, form.unwrap_or(NormalForm::Nfc)),
        max_string_length,
        span,
        "NORMALIZE result",
    )
}

fn capped_string_function_value(
    text: &str,
    max_string_length: u32,
    span: SourceSpan,
    subject: &'static str,
) -> Result<Value, ExecutorError> {
    let max_chars = usize::try_from(max_string_length).unwrap_or(usize::MAX);
    if text.chars().count() > max_chars {
        return data_exception_with(
            DataExceptionSubclass::StringDataRightTruncation,
            format!("{subject} exceeds the configured maximum character length"),
            span,
        );
    }
    selene_core::db_string(text)
        .map(Value::String)
        .map_err(|_err| {
            data_exception_value_with(
                DataExceptionSubclass::StringDataRightTruncation,
                format!("{subject} exceeds the maximum byte length"),
                span,
            )
        })
}

pub(super) fn is_normalized(value: &str, form: NormalForm) -> bool {
    // Allocation-free per-form quick check — avoids materializing a normalized
    // copy per row when the input is already normalized (the common case).
    match form {
        NormalForm::Nfc => unicode_normalization::is_nfc(value),
        NormalForm::Nfd => unicode_normalization::is_nfd(value),
        NormalForm::Nfkc => unicode_normalization::is_nfkc(value),
        NormalForm::Nfkd => unicode_normalization::is_nfkd(value),
    }
}

pub(super) fn eval_trim_function(
    name: &str,
    args: &[ValueExpr],
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    match args.len() {
        1 => {
            let value = evaluate(&args[0], binding, schema, ctx)?;
            eval_string_trim(value, span)
        }
        2 => eval_list_trim(args, span, binding, schema, ctx),
        actual => Err(ExecutorError::FunctionArityMismatch {
            name: name.to_owned(),
            expected: "1 or 2",
            actual,
            span,
        }),
    }
}

fn eval_string_trim(value: Value, span: SourceSpan) -> Result<Value, ExecutorError> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    if let Some(value) = string_slice(&value) {
        return string_value(&trim_by_char_set(value, " ", TrimSide::Both), span);
    }
    if let Value::Bytes(value) = value {
        return eval_explicit_trim_bytes(&value, None, TrimSide::Both, span);
    }
    data_exception("trim argument is not a string or byte string", span)
}

fn eval_list_trim(
    args: &[ValueExpr],
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    let count = evaluate(&args[1], binding, schema, ctx)?;
    let Some(count) = list_trim_count(count, span)? else {
        return Ok(Value::Null);
    };
    let source = evaluate(&args[0], binding, schema, ctx)?;
    let Value::List(values) = source else {
        return if matches!(source, Value::Null) {
            Ok(Value::Null)
        } else {
            data_exception("list trim source is not a list", span)
        };
    };
    let len = u128::try_from(values.len()).expect("usize fits in u128");
    if count > len {
        return data_exception_with(
            DataExceptionSubclass::ListElementError,
            "list trim count exceeds list cardinality",
            span,
        );
    }
    let retained = usize::try_from(len - count).expect("retained length is bounded by list length");
    Ok(Value::List(values.into_iter().take(retained).collect()))
}

fn list_trim_count(value: Value, span: SourceSpan) -> Result<Option<u128>, ExecutorError> {
    match value {
        Value::Null => Ok(None),
        Value::Int(value) if value >= 0 => Ok(Some(
            u128::try_from(value).expect("non-negative i64 fits in u128"),
        )),
        Value::Int128(value) if value >= 0 => Ok(Some(
            u128::try_from(value).expect("non-negative i128 fits in u128"),
        )),
        Value::Int(_) | Value::Int128(_) => data_exception_with(
            DataExceptionSubclass::ListElementError,
            "list trim count is negative",
            span,
        ),
        Value::Uint(value) => Ok(Some(u128::from(value))),
        Value::Uint128(value) => Ok(Some(value)),
        Value::Decimal(value) if value.trunc() != value => {
            data_exception("list trim count is not an exact integer", span)
        }
        Value::Decimal(value) if value >= Decimal::ZERO => Ok(Some(
            value.to_u128().expect("Decimal magnitude fits in u128"),
        )),
        Value::Decimal(_) => Err(data_exception_value_with(
            DataExceptionSubclass::ListElementError,
            "list trim count is negative",
            span,
        )),
        _ => data_exception("list trim count is not an exact integer", span),
    }
}

pub(super) fn eval_coalesce(
    name: &str,
    args: &[ValueExpr],
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    if args.len() < 2 {
        return Err(ExecutorError::FunctionArityMismatch {
            name: name.to_owned(),
            expected: "at least 2",
            actual: args.len(),
            span,
        });
    }
    for arg in args {
        let value = evaluate(arg, binding, schema, ctx)?;
        if !matches!(value, Value::Null) {
            return Ok(value);
        }
    }
    Ok(Value::Null)
}

pub(super) fn eval_nullif(mut args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    // Borrow both operands for the equality test; only the lhs is ever returned
    // (by value, via swap_remove), so neither clone is needed.
    let equal = eval_equality(BinaryOp::Eq, &args[0], &args[1], span)?;
    match equal {
        Value::Bool(true) => Ok(Value::Null),
        Value::Bool(false) | Value::Null => Ok(args.swap_remove(0)),
        _ => data_exception("nullif comparison did not produce boolean", span),
    }
}

pub(super) fn eval_size(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    match args.into_iter().next().expect("arity checked") {
        Value::Null => Ok(Value::Null),
        Value::List(values) => Ok(Value::Int(values.len() as i64)),
        _ => data_exception("size argument is not a list", span),
    }
}

fn normalize_string(value: &str, form: NormalForm) -> String {
    match form {
        NormalForm::Nfc => value.nfc().collect(),
        NormalForm::Nfd => value.nfd().collect(),
        NormalForm::Nfkc => value.nfkc().collect(),
        NormalForm::Nfkd => value.nfkd().collect(),
    }
}

fn string_length_count(value: &Value, span: SourceSpan) -> Result<usize, ExecutorError> {
    match value {
        Value::Int(value) if *value >= 0 => usize::try_from(*value).map_err(|_| {
            data_exception_value_with(
                DataExceptionSubclass::NumericValueOutOfRange,
                "integer argument is too large",
                span,
            )
        }),
        Value::Int128(value) if *value >= 0 => usize::try_from(*value).map_err(|_| {
            data_exception_value_with(
                DataExceptionSubclass::NumericValueOutOfRange,
                "integer argument is too large",
                span,
            )
        }),
        Value::Int(_) | Value::Int128(_) => Err(data_exception_value_with(
            DataExceptionSubclass::SubstringError,
            "string length is negative",
            span,
        )),
        Value::Uint(value) => usize::try_from(*value).map_err(|_| {
            data_exception_value_with(
                DataExceptionSubclass::NumericValueOutOfRange,
                "integer argument is too large",
                span,
            )
        }),
        Value::Uint128(value) => usize::try_from(*value).map_err(|_| {
            data_exception_value_with(
                DataExceptionSubclass::NumericValueOutOfRange,
                "integer argument is too large",
                span,
            )
        }),
        Value::Decimal(value) if value.trunc() == *value && *value >= Decimal::ZERO => {
            value.to_usize().ok_or_else(|| {
                data_exception_value_with(
                    DataExceptionSubclass::NumericValueOutOfRange,
                    "integer argument is too large",
                    span,
                )
            })
        }
        Value::Decimal(value) if *value < Decimal::ZERO => Err(data_exception_value_with(
            DataExceptionSubclass::SubstringError,
            "string length is negative",
            span,
        )),
        _ => data_exception("string length is not an integer", span),
    }
}

fn trim_by_char_set(source: &str, trim_chars: &str, side: TrimSide) -> String {
    let chars: Vec<char> = source.chars().collect();
    let trims = |candidate: char| trim_chars.chars().any(|trim| trim == candidate);
    let start = if matches!(side, TrimSide::Leading | TrimSide::Both) {
        chars
            .iter()
            .position(|candidate| !trims(*candidate))
            .unwrap_or(chars.len())
    } else {
        0
    };
    let end = if matches!(side, TrimSide::Trailing | TrimSide::Both) {
        chars
            .iter()
            .rposition(|candidate| !trims(*candidate))
            .map_or(start, |index| index.saturating_add(1))
    } else {
        chars.len()
    };
    chars[start..end].iter().copied().collect()
}
