//! LIST and VECTOR property-default helpers.

use rust_decimal::prelude::ToPrimitive;
use selene_core::{
    ByteStringType, CharacterStringType, CoreError, DecimalType, PropertyValueType, VectorValue,
};
use selene_graph::{PropertyDefaultValue, PropertyElementType};

use crate::{DataExceptionSubclass, ExecutorError, Literal, UnaryOp, ValueExpr};

pub(super) fn list_default_value(
    items: &[ValueExpr],
    element_type: &PropertyElementType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    items
        .iter()
        .map(|item| list_element_default_value(item, element_type, span))
        .map(|value| value.map(Box::new))
        .collect::<Result<Vec<_>, _>>()
        .map(PropertyDefaultValue::List)
}

fn list_element_default_value(
    expr: &ValueExpr,
    element_type: &PropertyElementType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    match element_type {
        PropertyElementType::NotNull(inner) => {
            let value = list_element_default_value(expr, inner, span)?;
            if matches!(value, PropertyDefaultValue::Null) {
                return Err(list_default_invalid_type(
                    "LIST DEFAULT element cannot be NULL for a NOT NULL element type",
                    span,
                ));
            }
            Ok(value)
        }
        PropertyElementType::Scalar(PropertyValueType::Vector) => {
            let ValueExpr::ListLiteral { items, .. } = expr else {
                return Err(list_default_invalid_type(
                    "LIST<VECTOR> DEFAULT elements must be numeric list literals",
                    span,
                ));
            };
            vector_default_value(items, span)
        }
        PropertyElementType::Decimal(decimal_type) => {
            decimal_element_default(expr, *decimal_type, span)
        }
        PropertyElementType::CharacterString(character_string_type) => {
            character_string_element_default(expr, *character_string_type, span)
        }
        PropertyElementType::ByteString(byte_string_type) => {
            byte_string_element_default(expr, *byte_string_type, span)
        }
        PropertyElementType::Scalar(value_type) => scalar_element_default(expr, *value_type, span),
        PropertyElementType::List(inner) => {
            let ValueExpr::ListLiteral { items, .. } = expr else {
                return Err(list_default_invalid_type(
                    "nested LIST DEFAULT elements must be list literals",
                    span,
                ));
            };
            list_default_value(items, inner, span)
        }
        _ => Err(list_default_invalid_type(
            "LIST DEFAULT uses an unsupported element type",
            span,
        )),
    }
}

fn character_string_element_default(
    expr: &ValueExpr,
    character_string_type: CharacterStringType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let default = scalar_element_default(expr, PropertyValueType::String, span)?;
    super::descriptor::coerce_element_default(
        default,
        &PropertyElementType::CharacterString(character_string_type),
        span,
    )
}

fn byte_string_element_default(
    expr: &ValueExpr,
    byte_string_type: ByteStringType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let default = scalar_element_default(expr, PropertyValueType::Bytes, span)?;
    super::descriptor::coerce_element_default(
        default,
        &PropertyElementType::ByteString(byte_string_type),
        span,
    )
}

fn decimal_element_default(
    expr: &ValueExpr,
    decimal_type: DecimalType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let default = scalar_element_default(expr, PropertyValueType::Decimal, span)?;
    super::descriptor::coerce_element_default(
        default,
        &PropertyElementType::Decimal(decimal_type),
        span,
    )
}

fn scalar_element_default(
    expr: &ValueExpr,
    value_type: PropertyValueType,
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    if matches!(
        value_type,
        PropertyValueType::List | PropertyValueType::Record | PropertyValueType::RecordTyped
    ) {
        return Err(list_default_invalid_type(
            "LIST DEFAULT uses an unsupported scalar element type",
            span,
        ));
    }
    let ValueExpr::Literal(literal) = expr else {
        return Err(list_default_invalid_type(
            "LIST DEFAULT elements must be literals",
            span,
        ));
    };
    let default = super::literal_property_default_value(literal, span)?;
    let default = super::coerce_property_default_value(value_type, default, span)?;
    let value = default.to_value().map_err(|err| {
        ExecutorError::data_exception(
            DataExceptionSubclass::DataException,
            format!("LIST DEFAULT element is invalid: {err}"),
            span,
        )
    })?;
    if matches!(value, selene_core::Value::Null) || value_type.matches(&value) {
        return Ok(default);
    }
    Err(list_default_invalid_type(
        "LIST DEFAULT element is not assignable to declared element type",
        span,
    ))
}

pub(super) fn vector_default_value(
    items: &[ValueExpr],
    span: crate::SourceSpan,
) -> Result<PropertyDefaultValue, ExecutorError> {
    let components = items
        .iter()
        .map(|item| vector_component(item, span))
        .collect::<Result<Vec<_>, _>>()?;
    let vector = VectorValue::new(components).map_err(|err| vector_default_error(err, span))?;
    Ok(PropertyDefaultValue::Vector(
        vector
            .as_slice()
            .iter()
            .copied()
            .map(canonical_f32_bits)
            .collect(),
    ))
}

fn vector_component(expr: &ValueExpr, span: crate::SourceSpan) -> Result<f32, ExecutorError> {
    match expr {
        ValueExpr::Literal(Literal::Integer(value, _))
        | ValueExpr::Literal(Literal::RadixInteger(value, _, _)) => Ok(*value as f32),
        ValueExpr::Literal(Literal::Decimal(value, _, _)) => finite_decimal_to_f32(value, span),
        ValueExpr::Literal(Literal::Float(value, _, _)) => finite_f64_to_f32(*value, span),
        ValueExpr::UnaryOp {
            op: UnaryOp::Negate,
            operand,
            ..
        } => negated_vector_component(operand, span),
        _ => Err(vector_default_invalid_type(
            "VECTOR DEFAULT list elements must be numeric literals",
            span,
        )),
    }
}

fn negated_vector_component(
    expr: &ValueExpr,
    span: crate::SourceSpan,
) -> Result<f32, ExecutorError> {
    match expr {
        ValueExpr::Literal(Literal::Integer(value, _))
        | ValueExpr::Literal(Literal::RadixInteger(value, _, _)) => Ok(-(*value as f32)),
        ValueExpr::Literal(Literal::Decimal(value, _, _)) => finite_decimal_to_f32(&-*value, span),
        ValueExpr::Literal(Literal::Float(value, _, _)) => finite_f64_to_f32(-*value, span),
        _ => Err(vector_default_invalid_type(
            "VECTOR DEFAULT negation must apply to a numeric literal",
            span,
        )),
    }
}

fn finite_decimal_to_f32(
    value: &rust_decimal::Decimal,
    span: crate::SourceSpan,
) -> Result<f32, ExecutorError> {
    value.to_f32().ok_or_else(|| {
        vector_default_out_of_range("VECTOR DEFAULT component exceeds FLOAT32 range", span)
    })
}

fn finite_f64_to_f32(value: f64, span: crate::SourceSpan) -> Result<f32, ExecutorError> {
    if !value.is_finite() {
        return Err(vector_default_out_of_range(
            "VECTOR DEFAULT component must be finite",
            span,
        ));
    }
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = value as f32;
    if !narrowed.is_finite() {
        return Err(vector_default_out_of_range(
            "VECTOR DEFAULT component exceeds FLOAT32 range",
            span,
        ));
    }
    Ok(narrowed)
}

fn vector_default_error(err: CoreError, span: crate::SourceSpan) -> ExecutorError {
    match err {
        CoreError::VectorEmpty | CoreError::VectorTooLarge { .. } => vector_default_invalid_type(
            "VECTOR DEFAULT must be a non-empty dimension-bounded numeric list",
            span,
        ),
        CoreError::VectorComponentNotFinite { .. } => {
            vector_default_out_of_range("VECTOR DEFAULT component must be finite", span)
        }
        _ => ExecutorError::data_exception(
            DataExceptionSubclass::DataException,
            format!("VECTOR DEFAULT value is invalid: {err}"),
            span,
        ),
    }
}

fn vector_default_invalid_type(message: &'static str, span: crate::SourceSpan) -> ExecutorError {
    ExecutorError::data_exception(DataExceptionSubclass::InvalidValueType, message, span)
}

fn vector_default_out_of_range(message: &'static str, span: crate::SourceSpan) -> ExecutorError {
    ExecutorError::data_exception(DataExceptionSubclass::NumericValueOutOfRange, message, span)
}

fn list_default_invalid_type(message: &'static str, span: crate::SourceSpan) -> ExecutorError {
    ExecutorError::data_exception(DataExceptionSubclass::InvalidValueType, message, span)
}

fn canonical_f32_bits(value: f32) -> u32 {
    if value == 0.0 {
        0.0_f32.to_bits()
    } else {
        value.to_bits()
    }
}

pub(super) fn render_list_literal(
    values: &[Box<PropertyDefaultValue>],
) -> Result<String, ExecutorError> {
    let mut rendered = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(&super::render_property_default_value(value)?);
    }
    rendered.push(']');
    Ok(rendered)
}

pub(super) fn render_vector_literal(bits: &[u32]) -> Result<String, ExecutorError> {
    if bits.is_empty() {
        return Err(ExecutorError::ImplementationDefined {
            detail: "empty VECTOR property default in catalog DDL rendering",
        });
    }
    let mut rendered = String::from("[");
    for (index, bits) in bits.iter().copied().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        let value = f32::from_bits(bits);
        if !value.is_finite() {
            return Err(ExecutorError::ImplementationDefined {
                detail: "non-finite VECTOR property default in catalog DDL rendering",
            });
        }
        rendered.push_str(&super::render_float_literal(f64::from(value))?);
    }
    rendered.push(']');
    Ok(rendered)
}
