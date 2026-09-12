//! Property default lowering, coercion, validation, and rendering.

mod descriptor;
mod lists;
mod records;

use std::fmt::Write as _;

use rust_decimal::Decimal;
use selene_core::{
    ByteStringType, CharacterStringType, DbString, DecimalType, JsonValue, PropertyValueType,
    Value, byte_string_fits_type, character_string_fits_type, db_string, decimal_fits_type,
};
use selene_graph::{PropertyDefaultValue, PropertyElementType, RecordFieldTypes};

use crate::{DataExceptionSubclass, ExecutorError, Literal, ProjectExpr, ValueExpr};

use lists::{list_default_value, render_list_literal, render_vector_literal, vector_default_value};
use records::{record_default_value, render_record_literal};

pub(super) use descriptor::coerce_property_descriptor_default;

pub(super) fn property_default_value(
    project: &ProjectExpr,
    value_type: PropertyValueType,
    list_element_type: Option<&PropertyElementType>,
    record_field_types: Option<&RecordFieldTypes>,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let ValueExpr::Literal(literal) = &project.expr else {
        return match (&project.expr, value_type) {
            (ValueExpr::ListLiteral { items, .. }, PropertyValueType::Vector) => {
                vector_default_value(items, span)
            }
            (ValueExpr::ListLiteral { items, .. }, PropertyValueType::List) => {
                let element_type =
                    list_element_type.ok_or(ExecutorError::ImplementationDefined {
                        detail: "LIST DEFAULT requires a declared LIST element type",
                    })?;
                list_default_value(items, element_type, span)
            }
            (ValueExpr::ListLiteral { .. }, _) => Err(ExecutorError::ImplementationDefined {
                detail: "LIST DEFAULT is only supported for LIST and VECTOR properties",
            }),
            (ValueExpr::RecordLiteral { fields, .. }, PropertyValueType::RecordTyped) => {
                record_default_value(fields, record_field_types, span)
            }
            (ValueExpr::RecordLiteral { .. }, _) => Err(ExecutorError::ImplementationDefined {
                detail: "RECORD DEFAULT is only supported for RECORD properties",
            }),
            _ => Err(ExecutorError::ImplementationDefined {
                detail: "DEFAULT constraint must lower to a literal expression",
            }),
        };
    };
    literal_property_default_value(literal, span)
}

pub(super) fn literal_property_default_value(
    literal: &Literal,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    match literal {
        Literal::Null(_) => Ok(PropertyDefaultValue::Null),
        Literal::Bool(value, _) => Ok(PropertyDefaultValue::Boolean(*value)),
        Literal::Integer(value, _) | Literal::RadixInteger(value, _, _) => {
            Ok(PropertyDefaultValue::Integer(*value))
        }
        Literal::Decimal(value, _, _) => decimal_default_value(*value, span),
        Literal::String(value, _, _) => Ok(PropertyDefaultValue::String(value.clone())),
        Literal::Bytes(value, _) => Ok(PropertyDefaultValue::Bytes(value.to_vec())),
        Literal::Uuid(value, _, _) => db_string(&value.to_string())
            .map(PropertyDefaultValue::Uuid)
            .map_err(|err| {
                ExecutorError::data_exception(
                    DataExceptionSubclass::DataException,
                    format!("UUID DEFAULT value is invalid: {err}"),
                    span,
                )
            }),
        Literal::Float(value, _, _) => float_default_value(*value, span),
        Literal::ZonedDateTime(value, _, _) => {
            temporal_default_value("ZONED DATETIME", zoned_datetime_image(value), span)
                .map(PropertyDefaultValue::ZonedDateTime)
        }
        Literal::LocalDateTime(value, _, _) => {
            temporal_default_value("LOCAL DATETIME", value.to_string(), span)
                .map(PropertyDefaultValue::LocalDateTime)
        }
        Literal::Date(value, _, _) => {
            temporal_default_value("DATE", value.to_string(), span).map(PropertyDefaultValue::Date)
        }
        Literal::ZonedTime(value, _, _) => {
            temporal_default_value("ZONED TIME", zoned_time_image(value), span)
                .map(PropertyDefaultValue::ZonedTime)
        }
        Literal::LocalTime(value, _, _) => {
            temporal_default_value("LOCAL TIME", value.to_string(), span)
                .map(PropertyDefaultValue::LocalTime)
        }
        Literal::Duration(value, _, _) => {
            temporal_default_value("DURATION", value.to_string(), span)
                .map(PropertyDefaultValue::Duration)
        }
    }
}

pub(super) fn coerce_property_default_value(
    value_type: PropertyValueType,
    default: PropertyDefaultValue,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    match (value_type, default) {
        (PropertyValueType::Uint, PropertyDefaultValue::Integer(value)) => {
            coerce_integer_to_uint(value, span)
        }
        (PropertyValueType::Uint, PropertyDefaultValue::String(value)) => {
            coerce_string_to_uint(value.as_str(), span)
        }
        (PropertyValueType::Int128, PropertyDefaultValue::Integer(value)) => {
            Ok(PropertyDefaultValue::Int128(i128::from(value)))
        }
        (PropertyValueType::Int128, PropertyDefaultValue::String(value)) => {
            coerce_string_to_int128(value.as_str(), span)
        }
        (PropertyValueType::Uint128, PropertyDefaultValue::Integer(value)) => {
            coerce_integer_to_uint128(value, span)
        }
        (PropertyValueType::Uint128, PropertyDefaultValue::String(value)) => {
            coerce_string_to_uint128(value.as_str(), span)
        }
        (PropertyValueType::Decimal, PropertyDefaultValue::Integer(value)) => {
            decimal_default_value(Decimal::from(value), span)
        }
        (PropertyValueType::Decimal, PropertyDefaultValue::Float(bits)) => {
            coerce_float_to_decimal(f64::from_bits(bits), span)
        }
        (PropertyValueType::Decimal, PropertyDefaultValue::Float32(bits)) => {
            coerce_float_to_decimal(f64::from(f32::from_bits(bits)), span)
        }
        (PropertyValueType::Decimal, PropertyDefaultValue::String(value)) => {
            coerce_string_to_decimal(value.as_str(), span)
        }
        (PropertyValueType::Float32, PropertyDefaultValue::Float(bits)) => {
            coerce_float_to_float32(bits, span)
        }
        (PropertyValueType::Json, PropertyDefaultValue::String(value)) => {
            coerce_string_to_json(value.as_str(), span)
        }
        (_, default) => Ok(default),
    }
}

pub(super) struct DefaultValidationContext<'a> {
    pub(super) property: DbString,
    pub(super) value_type: PropertyValueType,
    pub(super) decimal_type: Option<DecimalType>,
    pub(super) character_string_type: Option<CharacterStringType>,
    pub(super) byte_string_type: Option<ByteStringType>,
    pub(super) list_element_type: Option<&'a PropertyElementType>,
    pub(super) record_field_types: Option<&'a RecordFieldTypes>,
    pub(super) required: bool,
    pub(super) span: crate::SourceSpan,
}

pub(super) fn validate_default_value(
    context: &DefaultValidationContext<'_>,
    default: &PropertyDefaultValue,
) -> Result<(), ExecutorError> {
    let value = default.to_value().map_err(|err| {
        ExecutorError::data_exception(
            DataExceptionSubclass::DataException,
            format!("DEFAULT value is invalid: {err}"),
            context.span,
        )
    })?;
    if matches!(value, selene_core::Value::Null) {
        if context.required {
            return Err(default_type_error(
                context.property.clone(),
                context.value_type,
                "Null",
                "NOT NULL property cannot default to NULL",
                context.span,
            ));
        }
        return Ok(());
    }
    if default_matches_value(
        context.value_type,
        context.decimal_type,
        context.character_string_type,
        context.byte_string_type,
        context.list_element_type,
        context.record_field_types,
        &value,
    ) {
        return Ok(());
    }
    Err(default_type_error(
        context.property.clone(),
        context.value_type,
        PropertyValueType::observed_name(&value),
        "DEFAULT literal is not assignable to property type",
        context.span,
    ))
}

pub(in crate::runtime::batch::catalog) fn render_property_default_value(
    default: &PropertyDefaultValue,
) -> Result<String, ExecutorError> {
    match default {
        PropertyDefaultValue::Null => Ok("NULL".to_owned()),
        PropertyDefaultValue::Boolean(value) => Ok(value.to_string().to_uppercase()),
        PropertyDefaultValue::Integer(value) => Ok(value.to_string()),
        PropertyDefaultValue::Uint(value) => Ok(render_string_literal(&value.to_string())),
        PropertyDefaultValue::Int128(value) => Ok(render_string_literal(&value.to_string())),
        PropertyDefaultValue::Uint128(value) => Ok(render_string_literal(&value.to_string())),
        PropertyDefaultValue::Decimal(value) => Ok(render_string_literal(value.as_str())),
        PropertyDefaultValue::String(value) | PropertyDefaultValue::Json(value) => {
            Ok(render_string_literal(value.as_str()))
        }
        PropertyDefaultValue::Bytes(value) => Ok(render_byte_string_literal(value)),
        PropertyDefaultValue::List(values) => render_list_literal(values),
        PropertyDefaultValue::Record(fields) => render_record_literal(fields),
        PropertyDefaultValue::Uuid(value) => {
            Ok(format!("UUID {}", render_string_literal(value.as_str())))
        }
        PropertyDefaultValue::Float(bits) => render_float_literal(f64::from_bits(*bits)),
        PropertyDefaultValue::Float32(bits) => {
            render_float_literal(f64::from(f32::from_bits(*bits)))
        }
        PropertyDefaultValue::ZonedDateTime(value) => Ok(render_keyword_string_literal(
            "ZONED DATETIME",
            value.as_str(),
        )),
        PropertyDefaultValue::LocalDateTime(value) => Ok(render_keyword_string_literal(
            "LOCAL DATETIME",
            value.as_str(),
        )),
        PropertyDefaultValue::Date(value) => {
            Ok(render_keyword_string_literal("DATE", value.as_str()))
        }
        PropertyDefaultValue::ZonedTime(value) => {
            Ok(render_keyword_string_literal("ZONED TIME", value.as_str()))
        }
        PropertyDefaultValue::LocalTime(value) => {
            Ok(render_keyword_string_literal("LOCAL TIME", value.as_str()))
        }
        PropertyDefaultValue::Duration(value) => {
            Ok(render_keyword_string_literal("DURATION", value.as_str()))
        }
        PropertyDefaultValue::Vector(bits) => render_vector_literal(bits),
        _ => Err(ExecutorError::ImplementationDefined {
            detail: "unsupported property default value in catalog DDL rendering",
        }),
    }
}

fn default_matches_value(
    value_type: PropertyValueType,
    decimal_type: Option<DecimalType>,
    character_string_type: Option<CharacterStringType>,
    byte_string_type: Option<ByteStringType>,
    list_element_type: Option<&PropertyElementType>,
    record_field_types: Option<&RecordFieldTypes>,
    value: &Value,
) -> bool {
    match value_type {
        PropertyValueType::List => {
            let Some(element_type) = list_element_type else {
                return matches!(value, Value::List(_));
            };
            match value {
                Value::List(values) => values.iter().all(|value| element_type.matches(value)),
                _ => false,
            }
        }
        PropertyValueType::Record | PropertyValueType::RecordTyped => {
            if !matches!(value, Value::Record(_) | Value::RecordTyped(_)) {
                return false;
            }
            match record_field_types {
                Some(fields) => fields.matches(value),
                None => true,
            }
        }
        PropertyValueType::Decimal => match decimal_type {
            Some(decimal_type) => {
                matches!(value, Value::Decimal(value) if decimal_fits_type(*value, decimal_type))
            }
            None => value_type.matches(value),
        },
        PropertyValueType::String => match character_string_type {
            Some(character_string_type) => {
                matches!(value, Value::String(value) if character_string_fits_type(value, character_string_type))
            }
            None => value_type.matches(value),
        },
        PropertyValueType::Bytes => match byte_string_type {
            Some(byte_string_type) => {
                matches!(value, Value::Bytes(value) if byte_string_fits_type(value, byte_string_type))
            }
            None => value_type.matches(value),
        },
        _ => value_type.matches(value),
    }
}

fn coerce_integer_to_uint(
    value: i64,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    u64::try_from(value)
        .map(PropertyDefaultValue::Uint)
        .map_err(|_| {
            numeric_default_out_of_range("INTEGER DEFAULT literal is negative for UINT64", span)
        })
}

fn coerce_integer_to_uint128(
    value: i64,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    u128::try_from(value)
        .map(PropertyDefaultValue::Uint128)
        .map_err(|_| {
            numeric_default_out_of_range("INTEGER DEFAULT literal is negative for UINT128", span)
        })
}

fn coerce_string_to_uint(
    text: &str,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    parse_u128_default(text, u128::from(u64::MAX), "UINT64", span)
        .map(|value| PropertyDefaultValue::Uint(value as u64))
}

fn coerce_string_to_int128(
    text: &str,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let trimmed = text.trim();
    match trimmed.parse::<i128>() {
        Ok(value) => Ok(PropertyDefaultValue::Int128(value)),
        Err(_) if integer_literal_overflows_i128(trimmed) => Err(numeric_default_out_of_range(
            "STRING DEFAULT value exceeds INT128 range",
            span,
        )),
        Err(_) => Err(invalid_numeric_default_text("INT128", text, span)),
    }
}

fn coerce_string_to_uint128(
    text: &str,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    parse_u128_default(text, u128::MAX, "UINT128", span).map(PropertyDefaultValue::Uint128)
}

fn parse_u128_default(
    text: &str,
    max: u128,
    kind: &'static str,
    span: crate::SourceSpan,
) -> Result<u128, ExecutorError> {
    let trimmed = text.trim();
    match trimmed.parse::<u128>() {
        Ok(value) if value <= max => Ok(value),
        Ok(_) => Err(numeric_default_out_of_range(
            "STRING DEFAULT value exceeds unsigned integer range",
            span,
        )),
        Err(_) if unsigned_literal_overflows(trimmed, max) => Err(numeric_default_out_of_range(
            "STRING DEFAULT value exceeds unsigned integer range",
            span,
        )),
        Err(_) => Err(invalid_numeric_default_text(kind, text, span)),
    }
}

fn coerce_float_to_decimal(
    value: f64,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    if value.is_nan() {
        return Err(ExecutorError::data_exception(
            DataExceptionSubclass::InvalidCharacterValueForCast,
            "FLOAT DEFAULT NaN has no DECIMAL representation",
            span,
        ));
    }
    let decimal = Decimal::from_f64_retain(value).ok_or_else(|| {
        numeric_default_out_of_range("FLOAT DEFAULT value exceeds DECIMAL range", span)
    })?;
    decimal_default_value(decimal, span)
}

fn coerce_string_to_decimal(
    text: &str,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    text.trim()
        .parse::<Decimal>()
        .map_err(|_| invalid_numeric_default_text("DECIMAL", text, span))
        .and_then(|value| decimal_default_value(value, span))
}

fn decimal_default_value(
    value: Decimal,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    db_string(&value.to_string())
        .map(PropertyDefaultValue::Decimal)
        .map_err(|err| {
            ExecutorError::data_exception(
                DataExceptionSubclass::DataException,
                format!("DECIMAL DEFAULT value is invalid: {err}"),
                span,
            )
        })
}

fn coerce_float_to_float32(
    bits: u64,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let value = f64::from_bits(bits);
    if !value.is_finite() {
        return Err(float_default_out_of_range(
            "FLOAT DEFAULT literal must be finite",
            span,
        ));
    }
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = value as f32;
    if !narrowed.is_finite() {
        return Err(float_default_out_of_range(
            "FLOAT DEFAULT literal exceeds FLOAT32 range",
            span,
        ));
    }
    Ok(PropertyDefaultValue::Float32(canonical_f32_bits(narrowed)))
}

fn coerce_string_to_json(
    value: &str,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let json = JsonValue::parse_str(value).map_err(|err| {
        if err.gqlstatus() == "22018" {
            ExecutorError::data_exception(
                DataExceptionSubclass::InvalidCharacterValueForCast,
                format!("JSON DEFAULT string is not valid JSON: {err}"),
                span,
            )
        } else {
            ExecutorError::data_exception(
                DataExceptionSubclass::DataException,
                format!("JSON DEFAULT value is invalid: {err}"),
                span,
            )
        }
    })?;
    let canonical = DbString::from_string(json.to_canonical_string()).map_err(|err| {
        ExecutorError::data_exception(
            DataExceptionSubclass::DataException,
            format!("JSON DEFAULT value is invalid: {err}"),
            span,
        )
    })?;
    Ok(PropertyDefaultValue::Json(canonical))
}

fn float_default_value(
    value: f64,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    if !value.is_finite() {
        return Err(float_default_out_of_range(
            "FLOAT DEFAULT literal must be finite",
            span,
        ));
    }
    Ok(PropertyDefaultValue::Float(canonical_f64_bits(value)))
}

fn float_default_out_of_range(message: &'static str, span: crate::SourceSpan) -> ExecutorError {
    ExecutorError::data_exception(DataExceptionSubclass::NumericValueOutOfRange, message, span)
}

fn canonical_f64_bits(value: f64) -> u64 {
    if value == 0.0 {
        0.0_f64.to_bits()
    } else {
        value.to_bits()
    }
}

fn canonical_f32_bits(value: f32) -> u32 {
    if value == 0.0 {
        0.0_f32.to_bits()
    } else {
        value.to_bits()
    }
}

fn temporal_default_value(
    kind: &'static str,
    text: String,
    span: crate::SourceSpan,
) -> Result<selene_core::DbString, ExecutorError> {
    DbString::from_string(text).map_err(|err| {
        ExecutorError::data_exception(
            DataExceptionSubclass::DataException,
            format!("{kind} DEFAULT value is invalid: {err}"),
            span,
        )
    })
}

fn zoned_datetime_image(value: &jiff::Zoned) -> String {
    format!("{}{}", value.datetime(), value.offset())
}

fn zoned_time_image(value: &jiff::Zoned) -> String {
    format!("{}{}", value.time(), value.offset())
}

fn default_type_error(
    property: selene_core::DbString,
    expected: PropertyValueType,
    observed: &'static str,
    reason: &'static str,
    span: crate::SourceSpan,
) -> ExecutorError {
    ExecutorError::data_exception(
        DataExceptionSubclass::InvalidValueType,
        format!(
            "{reason}: property {property} expects {}, default is {observed}",
            expected.name()
        ),
        span,
    )
}

fn invalid_numeric_default_text(
    kind: &'static str,
    text: &str,
    span: crate::SourceSpan,
) -> ExecutorError {
    ExecutorError::data_exception(
        DataExceptionSubclass::InvalidCharacterValueForCast,
        format!("STRING DEFAULT value `{text}` is not a valid {kind}"),
        span,
    )
}

fn numeric_default_out_of_range(message: &'static str, span: crate::SourceSpan) -> ExecutorError {
    ExecutorError::data_exception(DataExceptionSubclass::NumericValueOutOfRange, message, span)
}

fn unsigned_literal_overflows(text: &str, max: u128) -> bool {
    !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && digits_exceed_limit(text, max)
}

fn integer_literal_overflows_i128(text: &str) -> bool {
    let (negative, digits) = match text.as_bytes().first().copied() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        Some(_) => (false, text),
        None => return false,
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let limit = if negative {
        1_u128 << 127
    } else {
        i128::MAX as u128
    };
    digits_exceed_limit(digits, limit)
}

fn digits_exceed_limit(digits: &str, limit: u128) -> bool {
    let mut value = 0_u128;
    for byte in digits.bytes() {
        let digit = u128::from(byte - b'0');
        if value > (limit - digit) / 10 {
            return true;
        }
        value = value * 10 + digit;
    }
    false
}

fn render_float_literal(value: f64) -> Result<String, ExecutorError> {
    if !value.is_finite() {
        return Err(ExecutorError::ImplementationDefined {
            detail: "non-finite property default value in catalog DDL rendering",
        });
    }
    let mut rendered = value.to_string();
    if !rendered.contains(|ch| ['.', 'e', 'E'].contains(&ch)) {
        rendered.push_str(".0");
    }
    rendered.push('D');
    Ok(rendered)
}

fn render_keyword_string_literal(keyword: &'static str, value: &str) -> String {
    format!("{keyword} {}", render_string_literal(value))
}

fn render_string_literal(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
        .replace('\'', "''");
    format!("'{escaped}'")
}

fn render_byte_string_literal(bytes: &[u8]) -> String {
    let mut rendered = String::with_capacity(3 + bytes.len() * 2);
    rendered.push_str("X'");
    for byte in bytes {
        write!(&mut rendered, "{byte:02X}").expect("writing to String cannot fail");
    }
    rendered.push('\'');
    rendered
}
