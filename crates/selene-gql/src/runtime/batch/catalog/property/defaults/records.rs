//! RECORD property-default helpers.

use std::collections::BTreeSet;

use selene_core::{ByteStringType, CharacterStringType, DbString, DecimalType, PropertyValueType};
use selene_graph::{
    PropertyDefaultRecordField, PropertyDefaultValue, RecordFieldType, RecordFieldTypes,
};

use crate::{DataExceptionSubclass, ExecutorError, ValueExpr};

pub(super) fn record_default_value(
    fields: &[(DbString, ValueExpr)],
    record_field_types: Option<&RecordFieldTypes>,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    match record_field_types {
        Some(field_types) => closed_record_default_value(fields, field_types, span),
        None => open_record_default_value(fields, span),
    }
}

fn open_record_default_value(
    fields: &[(DbString, ValueExpr)],
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let mut seen = BTreeSet::new();
    fields
        .iter()
        .map(|(name, expr)| {
            if !seen.insert(name.clone()) {
                return Err(record_field_unassignable(
                    format!("duplicate RECORD DEFAULT field: {name}"),
                    span,
                ));
            }
            Ok(PropertyDefaultRecordField {
                name: name.clone(),
                value: Box::new(untyped_default_value(expr, span)?),
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(PropertyDefaultValue::Record)
}

fn closed_record_default_value(
    fields: &[(DbString, ValueExpr)],
    field_types: &RecordFieldTypes,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let mut seen = BTreeSet::new();
    let mut output = Vec::with_capacity(fields.len());
    for (name, expr) in fields {
        if !seen.insert(name.clone()) {
            return Err(record_field_unassignable(
                format!("duplicate RECORD DEFAULT field: {name}"),
                span,
            ));
        }
        let Some(field_type) = field_types.0.iter().find(|field| field.name == *name) else {
            return Err(record_fields_mismatch(
                format!("RECORD DEFAULT field {name} is not declared in the target RECORD type"),
                span,
            ));
        };
        let value = typed_field_default_value(expr, &field_type.field_type, span)?;
        if field_type.required && matches!(value, PropertyDefaultValue::Null) {
            return Err(record_field_unassignable(
                format!(
                    "RECORD DEFAULT field {} cannot be NULL for a NOT NULL field",
                    field_type.name
                ),
                span,
            ));
        }
        output.push(PropertyDefaultRecordField {
            name: name.clone(),
            value: Box::new(value),
        });
    }
    for field_type in &field_types.0 {
        if !seen.contains(&field_type.name) {
            return Err(record_fields_mismatch(
                format!(
                    "RECORD DEFAULT is missing declared field {}",
                    field_type.name
                ),
                span,
            ));
        }
    }
    Ok(PropertyDefaultValue::Record(output))
}

fn typed_field_default_value(
    expr: &ValueExpr,
    field_type: &RecordFieldType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    match field_type {
        RecordFieldType::NotNull(inner) => {
            let value = typed_field_default_value(expr, inner, span)?;
            if matches!(value, PropertyDefaultValue::Null) {
                return Err(record_field_unassignable(
                    "RECORD DEFAULT field cannot be NULL for a NOT NULL field type".to_owned(),
                    span,
                ));
            }
            Ok(value)
        }
        RecordFieldType::Scalar(PropertyValueType::Vector) => {
            let ValueExpr::ListLiteral { items, .. } = expr else {
                return Err(record_field_unassignable(
                    "VECTOR RECORD DEFAULT fields must use numeric list literals".to_owned(),
                    span,
                ));
            };
            super::lists::vector_default_value(items, span)
        }
        RecordFieldType::Decimal(decimal_type) => decimal_field_default(expr, *decimal_type, span),
        RecordFieldType::CharacterString(character_string_type) => {
            character_string_field_default(expr, *character_string_type, span)
        }
        RecordFieldType::ByteString(byte_string_type) => {
            byte_string_field_default(expr, *byte_string_type, span)
        }
        RecordFieldType::Scalar(value_type) => scalar_field_default(expr, *value_type, span),
        RecordFieldType::List(inner) => {
            let ValueExpr::ListLiteral { items, .. } = expr else {
                return Err(record_field_unassignable(
                    "LIST RECORD DEFAULT fields must use list literals".to_owned(),
                    span,
                ));
            };
            list_field_default_value(items, inner, span)
        }
        RecordFieldType::OpenRecord => {
            let ValueExpr::RecordLiteral { fields, .. } = expr else {
                return Err(record_fields_mismatch(
                    "open RECORD DEFAULT fields must use record literals".to_owned(),
                    span,
                ));
            };
            record_default_value(fields, None, span)
        }
        RecordFieldType::Record(inner) => {
            let ValueExpr::RecordLiteral { fields, .. } = expr else {
                return Err(record_fields_mismatch(
                    "nested RECORD DEFAULT fields must use record literals".to_owned(),
                    span,
                ));
            };
            record_default_value(fields, Some(inner), span)
        }
        _ => Err(record_field_unassignable(
            "RECORD DEFAULT uses an unsupported field type".to_owned(),
            span,
        )),
    }
}

fn character_string_field_default(
    expr: &ValueExpr,
    character_string_type: CharacterStringType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let default = scalar_field_default(expr, PropertyValueType::String, span)?;
    super::descriptor::coerce_record_field_default(
        default,
        &RecordFieldType::CharacterString(character_string_type),
        span,
    )
}

fn byte_string_field_default(
    expr: &ValueExpr,
    byte_string_type: ByteStringType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let default = scalar_field_default(expr, PropertyValueType::Bytes, span)?;
    super::descriptor::coerce_record_field_default(
        default,
        &RecordFieldType::ByteString(byte_string_type),
        span,
    )
}

fn decimal_field_default(
    expr: &ValueExpr,
    decimal_type: DecimalType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let default = scalar_field_default(expr, PropertyValueType::Decimal, span)?;
    super::descriptor::coerce_record_field_default(
        default,
        &RecordFieldType::Decimal(decimal_type),
        span,
    )
}

fn list_field_default_value(
    items: &[ValueExpr],
    element_type: &RecordFieldType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    items
        .iter()
        .map(|item| typed_field_default_value(item, element_type, span))
        .map(|value| value.map(Box::new))
        .collect::<Result<Vec<_>, _>>()
        .map(PropertyDefaultValue::List)
}

fn scalar_field_default(
    expr: &ValueExpr,
    value_type: PropertyValueType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    if matches!(
        value_type,
        PropertyValueType::List | PropertyValueType::Record | PropertyValueType::RecordTyped
    ) {
        return Err(record_field_unassignable(
            "RECORD DEFAULT uses an unsupported scalar field type".to_owned(),
            span,
        ));
    }
    let ValueExpr::Literal(literal) = expr else {
        return Err(record_field_unassignable(
            "scalar RECORD DEFAULT fields must use literals".to_owned(),
            span,
        ));
    };
    let default = super::literal_property_default_value(literal, span)?;
    let default = super::coerce_property_default_value(value_type, default, span)?;
    let value = default.to_value().map_err(|err| {
        ExecutorError::data_exception(
            DataExceptionSubclass::DataException,
            format!("RECORD DEFAULT field is invalid: {err}"),
            span,
        )
    })?;
    if matches!(value, selene_core::Value::Null) || value_type.matches(&value) {
        return Ok(default);
    }
    Err(record_field_unassignable(
        "RECORD DEFAULT field is not assignable to declared field type".to_owned(),
        span,
    ))
}

fn untyped_default_value(
    expr: &ValueExpr,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    match expr {
        ValueExpr::Literal(literal) => super::literal_property_default_value(literal, span),
        ValueExpr::ListLiteral { items, .. } => items
            .iter()
            .map(|item| untyped_default_value(item, span))
            .map(|value| value.map(Box::new))
            .collect::<Result<Vec<_>, _>>()
            .map(PropertyDefaultValue::List),
        ValueExpr::RecordLiteral { fields, .. } => record_default_value(fields, None, span),
        ValueExpr::UnaryOp { .. } => Err(record_field_unassignable(
            "open RECORD DEFAULT fields must use literals, list literals, or record literals"
                .to_owned(),
            span,
        )),
        _ => Err(record_field_unassignable(
            "open RECORD DEFAULT fields must use literals, list literals, or record literals"
                .to_owned(),
            span,
        )),
    }
}

pub(super) fn render_record_literal(
    fields: &[PropertyDefaultRecordField],
) -> Result<String, ExecutorError> {
    let mut rendered = String::from("RECORD{");
    let mut seen = BTreeSet::new();
    for (index, field) in fields.iter().enumerate() {
        if !seen.insert(&field.name) {
            return Err(ExecutorError::ImplementationDefined {
                detail: "duplicate field in persisted RECORD property default",
            });
        }
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(field.name.as_str());
        rendered.push_str(": ");
        rendered.push_str(&super::render_property_default_value(&field.value)?);
    }
    rendered.push('}');
    Ok(rendered)
}

fn record_fields_mismatch(message: String, span: crate::SourceSpan) -> ExecutorError {
    ExecutorError::data_exception(DataExceptionSubclass::RecordFieldsDoNotMatch, message, span)
}

fn record_field_unassignable(message: String, span: crate::SourceSpan) -> ExecutorError {
    ExecutorError::data_exception(
        DataExceptionSubclass::RecordDataFieldUnassignable,
        message,
        span,
    )
}
