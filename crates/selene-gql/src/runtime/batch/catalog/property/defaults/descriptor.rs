//! Descriptor coercion for persisted property defaults.

use selene_core::{
    ByteStringType, CharacterStringCoercionError, CharacterStringType, DecimalType,
    PropertyValueType, Value, coerce_character_string_to_type, db_string, round_decimal_to_type,
};
use selene_graph::{
    PropertyDefaultRecordField, PropertyDefaultValue, PropertyElementType, RecordFieldType,
    RecordFieldTypes,
};

use crate::{DataExceptionSubclass, ExecutorError, SourceSpan};

pub(in crate::runtime::batch::catalog::property) fn coerce_property_descriptor_default(
    context: &super::DefaultValidationContext<'_>,
    default: PropertyDefaultValue,
) -> Result<PropertyDefaultValue, ExecutorError> {
    match context.value_type {
        PropertyValueType::Decimal => match context.decimal_type {
            Some(target) => coerce_decimal_default(default, target, context.span),
            None => Ok(default),
        },
        PropertyValueType::String => match context.character_string_type {
            Some(target) => coerce_character_string_default(default, target, context.span),
            None => Ok(default),
        },
        PropertyValueType::Bytes => match context.byte_string_type {
            Some(target) => coerce_byte_string_default(default, target, context.span),
            None => Ok(default),
        },
        PropertyValueType::List => match (context.list_element_type, default) {
            (Some(element_type), PropertyDefaultValue::List(values)) => values
                .into_iter()
                .map(|value| coerce_element_default(*value, element_type, context.span))
                .map(|value| value.map(Box::new))
                .collect::<Result<Vec<_>, _>>()
                .map(PropertyDefaultValue::List),
            (_, default) => Ok(default),
        },
        PropertyValueType::Record | PropertyValueType::RecordTyped => {
            match (context.record_field_types, default) {
                (Some(fields), default @ PropertyDefaultValue::Record(_)) => {
                    coerce_record_default(default, fields, context.span)
                }
                (_, default) => Ok(default),
            }
        }
        _ => Ok(default),
    }
}

pub(in crate::runtime::batch::catalog::property::defaults) fn coerce_element_default(
    default: PropertyDefaultValue,
    element_type: &PropertyElementType,
    span: SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    match element_type {
        PropertyElementType::NotNull(inner) => coerce_element_default(default, inner, span),
        PropertyElementType::Decimal(target) => coerce_decimal_default(default, *target, span),
        PropertyElementType::CharacterString(target) => {
            coerce_character_string_default(default, *target, span)
        }
        PropertyElementType::ByteString(target) => {
            coerce_byte_string_default(default, *target, span)
        }
        PropertyElementType::List(inner) => match default {
            PropertyDefaultValue::List(values) => values
                .into_iter()
                .map(|value| coerce_element_default(*value, inner, span))
                .map(|value| value.map(Box::new))
                .collect::<Result<Vec<_>, _>>()
                .map(PropertyDefaultValue::List),
            default => Ok(default),
        },
        _ => Ok(default),
    }
}

pub(in crate::runtime::batch::catalog::property::defaults) fn coerce_record_field_default(
    default: PropertyDefaultValue,
    field_type: &RecordFieldType,
    span: SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    match field_type {
        RecordFieldType::NotNull(inner) => coerce_record_field_default(default, inner, span),
        RecordFieldType::Decimal(target) => coerce_decimal_default(default, *target, span),
        RecordFieldType::CharacterString(target) => {
            coerce_character_string_default(default, *target, span)
        }
        RecordFieldType::ByteString(target) => coerce_byte_string_default(default, *target, span),
        RecordFieldType::List(inner) => match default {
            PropertyDefaultValue::List(values) => values
                .into_iter()
                .map(|value| coerce_record_field_default(*value, inner, span))
                .map(|value| value.map(Box::new))
                .collect::<Result<Vec<_>, _>>()
                .map(PropertyDefaultValue::List),
            default => Ok(default),
        },
        RecordFieldType::Record(fields) => coerce_record_default(default, fields, span),
        _ => Ok(default),
    }
}

fn coerce_record_default(
    default: PropertyDefaultValue,
    fields: &RecordFieldTypes,
    span: SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let PropertyDefaultValue::Record(default_fields) = default else {
        return Ok(default);
    };
    default_fields
        .into_iter()
        .map(|field| {
            let value = match fields
                .0
                .iter()
                .find(|candidate| candidate.name == field.name)
            {
                Some(field_type) => {
                    coerce_record_field_default(*field.value, &field_type.field_type, span)?
                }
                None => *field.value,
            };
            Ok(PropertyDefaultRecordField {
                name: field.name,
                value: Box::new(value),
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(PropertyDefaultValue::Record)
}

fn coerce_decimal_default(
    default: PropertyDefaultValue,
    target: DecimalType,
    span: SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    match default.to_value().map_err(|err| {
        ExecutorError::data_exception(
            DataExceptionSubclass::DataException,
            format!("DECIMAL DEFAULT value is invalid: {err}"),
            span,
        )
    })? {
        Value::Null => Ok(PropertyDefaultValue::Null),
        Value::Decimal(value) => round_decimal_to_type(value, target)
            .and_then(|value| PropertyDefaultValue::from_value(&Value::Decimal(value)))
            .ok_or_else(|| {
                ExecutorError::data_exception(
                    DataExceptionSubclass::NumericValueOutOfRange,
                    "DECIMAL DEFAULT value cannot be represented by declared precision/scale",
                    span,
                )
            }),
        _ => Ok(default),
    }
}

fn coerce_character_string_default(
    default: PropertyDefaultValue,
    target: CharacterStringType,
    span: SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let PropertyDefaultValue::String(value) = default else {
        return Ok(default);
    };
    // The shared core coercion keeps DEFAULT descriptor coercion on the same
    // IV023 space-only truncation policy as character CAST and store
    // assignment.
    let coerced = coerce_character_string_to_type(value.as_str(), target).map_err(|err| {
        let detail = match err {
            CharacterStringCoercionError::SourceLengthOverflow => {
                "STRING DEFAULT source length exceeds supported range"
            }
            CharacterStringCoercionError::TargetMinOverflow => {
                "STRING DEFAULT target minimum length exceeds supported range"
            }
            CharacterStringCoercionError::TargetMaxOverflow => {
                "STRING DEFAULT target maximum length exceeds supported range"
            }
            CharacterStringCoercionError::NonSpaceTruncation => {
                "STRING DEFAULT would truncate non-space trailing characters"
            }
        };
        ExecutorError::data_exception(
            DataExceptionSubclass::StringDataRightTruncation,
            detail,
            span,
        )
    })?;
    db_string(&coerced)
        .map(PropertyDefaultValue::String)
        .map_err(|err| {
            ExecutorError::data_exception(
                DataExceptionSubclass::DataException,
                format!("STRING DEFAULT value is invalid: {err}"),
                span,
            )
        })
}

fn coerce_byte_string_default(
    default: PropertyDefaultValue,
    target: ByteStringType,
    span: SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let PropertyDefaultValue::Bytes(mut bytes) = default else {
        return Ok(default);
    };
    let len = u64::try_from(bytes.len()).map_err(|_| {
        ExecutorError::data_exception(
            DataExceptionSubclass::StringDataRightTruncation,
            "BYTES DEFAULT source length exceeds supported range",
            span,
        )
    })?;
    if len > target.max_len {
        let max_len = usize::try_from(target.max_len).map_err(|_| {
            ExecutorError::data_exception(
                DataExceptionSubclass::StringDataRightTruncation,
                "BYTES DEFAULT target maximum length exceeds supported range",
                span,
            )
        })?;
        if bytes[max_len..].iter().any(|byte| *byte != 0) {
            return Err(ExecutorError::data_exception(
                DataExceptionSubclass::StringDataRightTruncation,
                "BYTES DEFAULT would truncate non-zero trailing bytes",
                span,
            ));
        }
        bytes.truncate(max_len);
    } else if len < target.min_len {
        let min_len = usize::try_from(target.min_len).map_err(|_| {
            ExecutorError::data_exception(
                DataExceptionSubclass::StringDataRightTruncation,
                "BYTES DEFAULT target minimum length exceeds supported range",
                span,
            )
        })?;
        bytes.resize(min_len, 0);
    }
    Ok(PropertyDefaultValue::Bytes(bytes))
}
