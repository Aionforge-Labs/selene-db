//! Implementation-defined JSON scalar functions.

use rust_decimal::{Decimal, prelude::ToPrimitive};
use selene_core::{JsonValue, Value, db_string};

use crate::{
    SourceSpan,
    runtime::{ExecutorError, evaluator::binary_ops::string_slice},
};

use super::{
    binary_ops::{data_exception, data_exception_value, string_value},
    cast::parse_json_value,
};

const MAX_JSON_PATH_SELECTORS: usize = 64;
const MAX_JSON_CONSTRUCTOR_ARGS: usize = 64;

pub(super) const JSON_PATH_MAX_ARGS: usize = MAX_JSON_PATH_SELECTORS + 1;
pub(super) const JSON_ARRAY_MAX_ARGS: usize = MAX_JSON_CONSTRUCTOR_ARGS;
pub(super) const JSON_OBJECT_MAX_ARGS: usize = MAX_JSON_CONSTRUCTOR_ARGS * 2;

pub(super) fn eval_json_parse(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    let value = args.into_iter().next().expect("arity checked");
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let Some(value) = string_slice(&value) else {
        return data_exception("json_parse argument is not a string", span);
    };
    parse_json_value(value, span)
}

pub(super) fn eval_json_stringify(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    match args.into_iter().next().expect("arity checked") {
        Value::Null => Ok(Value::Null),
        Value::Json(value) => string_value(&value.to_canonical_string(), span),
        _ => data_exception("json_stringify argument is not JSON", span),
    }
}

pub(super) fn eval_json_type(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    match args.into_iter().next().expect("arity checked") {
        Value::Null => Ok(Value::Null),
        Value::Json(value) => string_value(value.json_type_name(), span),
        _ => data_exception("json_type argument is not JSON", span),
    }
}

pub(super) fn eval_json_array(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    let mut values = Vec::with_capacity(args.len());
    for value in args {
        values.push(gql_value_to_json(value, "json_array", span)?);
    }
    json_value_from_serde(
        serde_json::Value::Array(values),
        "constructed JSON value",
        span,
    )
}

pub(super) fn eval_json_object(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    if !args.len().is_multiple_of(2) {
        return data_exception("json_object requires key/value argument pairs", span);
    }

    let mut object = serde_json::Map::with_capacity(args.len() / 2);
    let mut args = args.into_iter();
    while let Some(key) = args.next() {
        let value = args.next().expect("even argument count checked");
        let Value::String(key) = key else {
            return data_exception("json_object key is not a string", span);
        };
        if object.contains_key(key.as_str()) {
            return data_exception(
                format!("json_object duplicate key '{}'", key.as_str()),
                span,
            );
        }
        object.insert(
            key.as_str().to_owned(),
            gql_value_to_json(value, "json_object", span)?,
        );
    }
    json_value_from_serde(
        serde_json::Value::Object(object),
        "constructed JSON value",
        span,
    )
}

pub(super) fn eval_json_array_length(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    match args.into_iter().next().expect("arity checked") {
        Value::Null => Ok(Value::Null),
        Value::Json(value) => match value.as_serde() {
            serde_json::Value::Array(values) => Ok(Value::Int(values.len() as i64)),
            _ => data_exception("json_array_length argument is not a JSON array", span),
        },
        _ => data_exception("json_array_length argument is not JSON", span),
    }
}

pub(super) fn eval_json_object_keys(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    match args.into_iter().next().expect("arity checked") {
        Value::Null => Ok(Value::Null),
        Value::Json(value) => match value.as_serde() {
            serde_json::Value::Object(values) => {
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                let mut output = Vec::with_capacity(keys.len());
                for key in keys {
                    let key = db_string(key).map_err(|err| {
                        data_exception_value(format!("JSON object key is invalid: {err}"), span)
                    })?;
                    output.push(Value::String(key));
                }
                Ok(Value::List(output))
            }
            _ => data_exception("json_object_keys argument is not a JSON object", span),
        },
        _ => data_exception("json_object_keys argument is not JSON", span),
    }
}

pub(super) fn eval_json_contains(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let mut args = args.into_iter();
    let target = args.next().expect("arity checked");
    let candidate = args.next().expect("arity checked");
    if matches!(target, Value::Null) || matches!(candidate, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Json(target) = target else {
        return data_exception("json_contains target is not JSON", span);
    };
    let Value::Json(candidate) = candidate else {
        return data_exception("json_contains candidate is not JSON", span);
    };
    Ok(Value::Bool(target.contains(&candidate)))
}

pub(super) fn eval_json_merge_patch(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let mut args = args.into_iter();
    let target = args.next().expect("arity checked");
    let patch = args.next().expect("arity checked");
    if matches!(target, Value::Null) || matches!(patch, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Json(target) = target else {
        return data_exception("json_merge_patch target is not JSON", span);
    };
    let Value::Json(patch) = patch else {
        return data_exception("json_merge_patch patch is not JSON", span);
    };
    target.merge_patch(&patch).map(Value::Json).map_err(|err| {
        data_exception_value(format!("JSON merge patch result is invalid: {err}"), span)
    })
}

pub(super) fn eval_json_patch(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    let mut args = args.into_iter();
    let target = args.next().expect("arity checked");
    let patch = args.next().expect("arity checked");
    if matches!(target, Value::Null) || matches!(patch, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Json(target) = target else {
        return data_exception("json_patch target is not JSON", span);
    };
    let Value::Json(patch) = patch else {
        return data_exception("json_patch patch is not JSON", span);
    };
    target
        .apply_patch(&patch)
        .map(Value::Json)
        .map_err(|err| data_exception_value(format!("JSON patch operation failed: {err}"), span))
}

pub(super) fn eval_json_get(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    let Some(value) = select_json_path(&args, "json_get", PathArgs::VariadicOnly, span)? else {
        return Ok(Value::Null);
    };
    selected_json_value(value, span)
}

pub(super) fn eval_json_get_text(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let Some(value) = select_json_path(&args, "json_get_text", PathArgs::VariadicOnly, span)?
    else {
        return Ok(Value::Null);
    };
    selected_json_text(value, span)
}

pub(super) fn eval_json_get_scalar(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let Some(value) = select_json_path(&args, "json_get_scalar", PathArgs::VariadicOnly, span)?
    else {
        return Ok(Value::Null);
    };
    selected_json_scalar(value, "json_get_scalar", span)
}

pub(super) fn eval_json_get_path(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let Some(value) = select_json_path(&args, "json_get_path", PathArgs::DocumentOrVariadic, span)?
    else {
        return Ok(Value::Null);
    };
    selected_json_value(value, span)
}

pub(super) fn eval_json_get_path_text(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let Some(value) = select_json_path(
        &args,
        "json_get_path_text",
        PathArgs::DocumentOrVariadic,
        span,
    )?
    else {
        return Ok(Value::Null);
    };
    selected_json_text(value, span)
}

pub(super) fn eval_json_get_path_scalar(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let Some(value) = select_json_path(
        &args,
        "json_get_path_scalar",
        PathArgs::DocumentOrVariadic,
        span,
    )?
    else {
        return Ok(Value::Null);
    };
    selected_json_scalar(value, "json_get_path_scalar", span)
}

pub(super) fn eval_json_has_path(
    args: Vec<Value>,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    match json_path_exists(&args, "json_has_path", PathArgs::DocumentOrVariadic, span)? {
        Some(exists) => Ok(Value::Bool(exists)),
        None => Ok(Value::Null),
    }
}

fn selected_json_value(
    value: &serde_json::Value,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    json_value_from_serde(value.clone(), "selected JSON value", span)
}

fn selected_json_text(value: &serde_json::Value, span: SourceSpan) -> Result<Value, ExecutorError> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::String(value) => string_value(value, span),
        other => {
            let json = JsonValue::new(other.clone()).map_err(|err| {
                data_exception_value(format!("selected JSON value is invalid: {err}"), span)
            })?;
            string_value(&json.to_canonical_string(), span)
        }
    }
}

fn selected_json_scalar(
    value: &serde_json::Value,
    function: &'static str,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(value) => Ok(Value::Bool(*value)),
        serde_json::Value::Number(value) => json_scalar_number(value, function, span),
        serde_json::Value::String(value) => string_value(value, span),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => data_exception(
            format!("{function} selected value is not a JSON scalar"),
            span,
        ),
    }
}

fn json_scalar_number(
    value: &serde_json::Number,
    function: &'static str,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    if let Some(value) = value.as_i64() {
        return Ok(Value::Int(value));
    }
    if let Some(value) = value.as_u64() {
        return Ok(Value::Uint(value));
    }
    if let Some(value) = value.as_f64()
        && value.is_finite()
    {
        return Ok(Value::Float(value));
    }
    data_exception(
        format!("{function} selected JSON number is outside supported scalar ranges"),
        span,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathArgs {
    VariadicOnly,
    DocumentOrVariadic,
}

fn select_json_path<'a>(
    args: &'a [Value],
    function: &'static str,
    path_args: PathArgs,
    span: SourceSpan,
) -> Result<Option<&'a serde_json::Value>, ExecutorError> {
    debug_assert!(args.len() >= 2);
    debug_assert!(args.len() <= JSON_PATH_MAX_ARGS);
    if matches!(args[0], Value::Null) {
        return Ok(None);
    }
    let Value::Json(value) = &args[0] else {
        return Err(data_exception_value(
            format!("{function} target is not JSON"),
            span,
        ));
    };
    let mut current = value.as_serde();
    if path_args == PathArgs::DocumentOrVariadic
        && args.len() == 2
        && let Some(document) = optional_path_document(&args[1])?
    {
        return select_json_document_path(current, document, function, span);
    }
    for selector in &args[1..] {
        if matches!(selector, Value::Null) {
            return Ok(None);
        }
        let Some(next) = select_json_child(current, selector, function, span)? else {
            return Ok(None);
        };
        current = next;
    }
    Ok(Some(current))
}

fn gql_value_to_json(
    value: Value,
    function: &'static str,
    span: SourceSpan,
) -> Result<serde_json::Value, ExecutorError> {
    Ok(match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(value) => serde_json::Value::Bool(value),
        Value::Int(value) => serde_json::Value::Number(value.into()),
        Value::Uint(value) => serde_json::Value::Number(value.into()),
        Value::Float(value) => serde_json::Value::Number(json_number(value, function, span)?),
        Value::Float32(value) => {
            serde_json::Value::Number(json_number(f64::from(value), function, span)?)
        }
        Value::String(value) => serde_json::Value::String(value.as_str().to_owned()),
        Value::Json(value) => value.as_serde().clone(),
        Value::List(values) => {
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                output.push(gql_value_to_json(value, function, span)?);
            }
            serde_json::Value::Array(output)
        }
        Value::Int128(_) | Value::Uint128(_) | Value::Decimal(_) => {
            return data_exception(
                format!(
                    "{function} exact numeric value cannot be represented as JSON without precision loss"
                ),
                span,
            );
        }
        other => {
            return data_exception(
                format!("{function} cannot convert {} to JSON", other.variant_name()),
                span,
            );
        }
    })
}

fn json_number(
    value: f64,
    function: &'static str,
    span: SourceSpan,
) -> Result<serde_json::Number, ExecutorError> {
    serde_json::Number::from_f64(value).ok_or_else(|| {
        data_exception_value(
            format!("{function} numeric value cannot be represented as a JSON number"),
            span,
        )
    })
}

fn json_value_from_serde(
    value: serde_json::Value,
    context: &'static str,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    JsonValue::new(value)
        .map(Value::Json)
        .map_err(|err| data_exception_value(format!("{context} is invalid: {err}"), span))
}

fn json_path_exists(
    args: &[Value],
    function: &'static str,
    path_args: PathArgs,
    span: SourceSpan,
) -> Result<Option<bool>, ExecutorError> {
    debug_assert!(args.len() >= 2);
    debug_assert!(args.len() <= JSON_PATH_MAX_ARGS);
    if matches!(args[0], Value::Null) {
        return Ok(None);
    }
    let Value::Json(value) = &args[0] else {
        return Err(data_exception_value(
            format!("{function} target is not JSON"),
            span,
        ));
    };
    let mut current = value.as_serde();
    if path_args == PathArgs::DocumentOrVariadic
        && args.len() == 2
        && let Some(document) = optional_path_document(&args[1])?
    {
        return json_document_path_exists(current, document, function, span);
    }
    for selector in &args[1..] {
        if matches!(selector, Value::Null) {
            return Ok(None);
        }
        let Some(next) = select_json_child(current, selector, function, span)? else {
            return Ok(Some(false));
        };
        current = next;
    }
    Ok(Some(true))
}

fn optional_path_document(selector: &Value) -> Result<Option<&JsonValue>, ExecutorError> {
    match selector {
        Value::Null => Ok(None),
        Value::Json(value) => Ok(Some(value)),
        _ => Ok(None),
    }
}

fn select_json_document_path<'a>(
    mut current: &'a serde_json::Value,
    document: &JsonValue,
    function: &'static str,
    span: SourceSpan,
) -> Result<Option<&'a serde_json::Value>, ExecutorError> {
    for selector in path_document_selectors(document, function, span)? {
        let Some(next) = select_json_document_child(current, selector, function, span)? else {
            return Ok(None);
        };
        current = next;
    }
    Ok(Some(current))
}

fn json_document_path_exists(
    mut current: &serde_json::Value,
    document: &JsonValue,
    function: &'static str,
    span: SourceSpan,
) -> Result<Option<bool>, ExecutorError> {
    for selector in path_document_selectors(document, function, span)? {
        let Some(next) = select_json_document_child(current, selector, function, span)? else {
            return Ok(Some(false));
        };
        current = next;
    }
    Ok(Some(true))
}

fn path_document_selectors<'a>(
    document: &'a JsonValue,
    function: &'static str,
    span: SourceSpan,
) -> Result<&'a [serde_json::Value], ExecutorError> {
    let serde_json::Value::Array(selectors) = document.as_serde() else {
        return Err(data_exception_value(
            format!("{function} path document is not a JSON array"),
            span,
        ));
    };
    if selectors.is_empty() {
        return Err(data_exception_value(
            format!("{function} path document must contain at least one selector"),
            span,
        ));
    }
    if selectors.len() > MAX_JSON_PATH_SELECTORS {
        return Err(data_exception_value(
            format!("{function} supports at most {MAX_JSON_PATH_SELECTORS} path selectors"),
            span,
        ));
    }
    Ok(selectors.as_slice())
}

fn select_json_document_child<'a>(
    current: &'a serde_json::Value,
    selector: &serde_json::Value,
    function: &'static str,
    span: SourceSpan,
) -> Result<Option<&'a serde_json::Value>, ExecutorError> {
    match (current, selector) {
        (serde_json::Value::Object(values), serde_json::Value::String(key)) => Ok(values.get(key)),
        (serde_json::Value::Array(values), serde_json::Value::Number(index)) => {
            let Some(index) = json_document_array_index(index, values.len(), function, span)?
            else {
                return Ok(None);
            };
            Ok(values.get(index))
        }
        (serde_json::Value::Object(_), _) => Err(data_exception_value(
            format!("{function} path document object key is not a string"),
            span,
        )),
        (serde_json::Value::Array(_), _) => Err(data_exception_value(
            format!("{function} path document array index is not an integer"),
            span,
        )),
        _ => Ok(None),
    }
}

fn json_document_array_index(
    value: &serde_json::Number,
    len: usize,
    function: &'static str,
    span: SourceSpan,
) -> Result<Option<usize>, ExecutorError> {
    if let Some(value) = value.as_i64() {
        return Ok(json_signed_array_index(value, len));
    }
    if let Some(value) = value.as_u64() {
        return Ok(usize::try_from(value).ok().filter(|idx| *idx < len));
    }
    Err(data_exception_value(
        format!("{function} path document array index is not an integer"),
        span,
    ))
}

fn select_json_child<'a>(
    current: &'a serde_json::Value,
    selector: &Value,
    function: &'static str,
    span: SourceSpan,
) -> Result<Option<&'a serde_json::Value>, ExecutorError> {
    match (current, selector) {
        (serde_json::Value::Object(values), Value::String(key)) => Ok(values.get(key.as_str())),
        (serde_json::Value::Array(values), index) => {
            let Some(index) = json_array_index(index, values.len(), function, span)? else {
                return Ok(None);
            };
            Ok(values.get(index))
        }
        (serde_json::Value::Object(_), _) => Err(data_exception_value(
            format!("{function} object key is not a string"),
            span,
        )),
        _ => Ok(None),
    }
}

fn json_array_index(
    value: &Value,
    len: usize,
    function: &'static str,
    span: SourceSpan,
) -> Result<Option<usize>, ExecutorError> {
    match value {
        Value::Int(value) => Ok(json_signed_array_index(*value, len)),
        Value::Int128(value) => Ok(json_signed_wide_array_index(*value, len)),
        Value::Uint(value) => Ok(usize::try_from(*value).ok().filter(|idx| *idx < len)),
        Value::Uint128(value) => Ok(usize::try_from(*value).ok().filter(|idx| *idx < len)),
        Value::Decimal(value) => json_decimal_array_index(*value, len, function, span),
        _ => Err(json_array_index_error(function, span)),
    }
}

fn json_signed_array_index(value: i64, len: usize) -> Option<usize> {
    if value >= 0 {
        return usize::try_from(value).ok().filter(|idx| *idx < len);
    }
    let offset = usize::try_from(value.unsigned_abs()).ok()?;
    len.checked_sub(offset)
}

fn json_signed_wide_array_index(value: i128, len: usize) -> Option<usize> {
    if value >= 0 {
        return usize::try_from(value).ok().filter(|idx| *idx < len);
    }
    let offset = usize::try_from(value.unsigned_abs()).ok()?;
    len.checked_sub(offset)
}

fn json_decimal_array_index(
    value: Decimal,
    len: usize,
    function: &'static str,
    span: SourceSpan,
) -> Result<Option<usize>, ExecutorError> {
    if value.trunc() != value {
        return Err(json_array_index_error(function, span));
    }
    if value >= Decimal::ZERO {
        return Ok(value
            .to_u128()
            .and_then(|index| usize::try_from(index).ok())
            .filter(|idx| *idx < len));
    }
    Ok(value
        .to_i128()
        .and_then(|index| json_signed_wide_array_index(index, len)))
}

fn json_array_index_error(function: &'static str, span: SourceSpan) -> ExecutorError {
    data_exception_value(format!("{function} array index is not an integer"), span)
}
