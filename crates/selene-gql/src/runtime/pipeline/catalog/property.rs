//! Property-definition helpers for catalog DDL.

mod defaults;

pub(super) use defaults::render_property_default_value;
use defaults::{
    DefaultValidationContext, coerce_property_default_value, coerce_property_descriptor_default,
    property_default_value, validate_default_value,
};
use selene_core::{
    ByteStringType as CoreByteStringType, CharacterStringType as CoreCharacterStringType,
    DecimalType, PropertyValueType,
};
use selene_graph::{
    PropertyElementType, PropertyTypeDef, RecordFieldType, RecordFieldTypeDef, RecordFieldTypes,
};

use crate::{
    ExecutorError, GqlType, PlannedTypePropertyConstraint, PlannedTypePropertyDef, RecordType,
    ast::format_ident::fmt_ident, parser::MAX_NESTING_DEPTH,
};

pub(super) fn property_defs(
    properties: &[PlannedTypePropertyDef],
    allow_inline_indexed: bool,
) -> Result<Vec<PropertyTypeDef>, ExecutorError> {
    properties
        .iter()
        .map(|property| property_def(property, allow_inline_indexed))
        .collect()
}

fn property_def(
    property: &PlannedTypePropertyDef,
    allow_inline_indexed: bool,
) -> Result<PropertyTypeDef, ExecutorError> {
    let (gql_type, required_from_type) = strip_top_level_not_null(&property.gql_type);
    let (
        value_type,
        list_element_type,
        record_field_types,
        decimal_type,
        character_string_type,
        byte_string_type,
    ) = gql_type_to_property_value_type(gql_type, property.span)?;
    let mut required = required_from_type;
    let mut default = None;
    let mut default_span = property.span;
    let mut immutable = false;
    let mut unique = false;
    for constraint in &property.constraints {
        match constraint {
            PlannedTypePropertyConstraint::NotNull(_) => required = true,
            PlannedTypePropertyConstraint::Default(project, span) => {
                default = Some(property_default_value(
                    project,
                    value_type,
                    list_element_type.as_ref(),
                    record_field_types.as_ref(),
                    *span,
                )?);
                default_span = *span;
            }
            PlannedTypePropertyConstraint::Immutable(_) => immutable = true,
            PlannedTypePropertyConstraint::Unique(_) => unique = true,
            PlannedTypePropertyConstraint::Indexed { span, .. } if !allow_inline_indexed => {
                return Err(ExecutorError::FeatureNotSupportedYet {
                    feature: "inline INDEXED on edge properties",
                    span: *span,
                });
            }
            PlannedTypePropertyConstraint::Indexed { .. } => {}
        }
    }
    let default = default
        .map(|default| coerce_property_default_value(value_type, default, default_span))
        .transpose()?;
    let context = DefaultValidationContext {
        property: property.name.clone(),
        value_type,
        decimal_type,
        character_string_type,
        byte_string_type,
        list_element_type: list_element_type.as_ref(),
        record_field_types: record_field_types.as_ref(),
        required,
        span: default_span,
    };
    let default = default
        .map(|default| coerce_property_descriptor_default(&context, default))
        .transpose()?;
    if let Some(default) = &default {
        validate_default_value(&context, default)?;
    }
    Ok(PropertyTypeDef {
        name: property.name.clone(),
        value_type,
        list_element_type,
        required,
        default,
        immutable,
        unique,
        decimal_type,
        character_string_type,
        byte_string_type,
        record_field_types,
    })
}

type LoweredPropertyType = (
    PropertyValueType,
    Option<PropertyElementType>,
    Option<RecordFieldTypes>,
    Option<DecimalType>,
    Option<CoreCharacterStringType>,
    Option<CoreByteStringType>,
);
type LoweredScalarPropertyType = (
    PropertyValueType,
    Option<DecimalType>,
    Option<CoreCharacterStringType>,
    Option<CoreByteStringType>,
);

fn gql_type_to_property_value_type(
    gql_type: &GqlType,
    span: crate::SourceSpan,
) -> Result<LoweredPropertyType, ExecutorError> {
    match gql_type {
        GqlType::List(inner) => {
            let element_type = gql_type_to_property_element_type(inner, 1)?;
            Ok((
                PropertyValueType::List,
                Some(element_type),
                None,
                None,
                None,
                None,
            ))
        }
        // A top-level `RECORD` declaration lowers to the RecordTyped tag. A closed
        // `RECORD { .. }` carries its field-type descriptor; an open/bare `RECORD` carries
        // `None` (permissive, accepts any record value).
        GqlType::Record(record_type) => {
            let record_field_types = gql_record_to_record_field_types(record_type, 1, span)?;
            Ok((
                PropertyValueType::RecordTyped,
                None,
                record_field_types,
                None,
                None,
                None,
            ))
        }
        GqlType::NotNull(inner) => gql_type_to_property_value_type(inner, span),
        _ => gql_type_to_scalar_property_value_type(gql_type).map(
            |(value_type, decimal_type, character_string_type, byte_string_type)| {
                (
                    value_type,
                    None,
                    None,
                    decimal_type,
                    character_string_type,
                    byte_string_type,
                )
            },
        ),
    }
}

fn strip_top_level_not_null(gql_type: &GqlType) -> (&GqlType, bool) {
    match gql_type {
        GqlType::NotNull(inner) => (inner, true),
        _ => (gql_type, false),
    }
}

/// Lower a record TYPE into the catalog descriptor. `RecordType::Open` (bare `RECORD`)
/// yields `None` (permissive); `RecordType::Closed` yields the field-type list.
///
/// Field names remain required-present per ISO 39075:2024 §4.15.4 (a closed
/// record value has the same field-name set as the descriptor). A field-level
/// `NOT NULL` marker is carried on [`RecordFieldTypeDef::required`]; nested
/// markers inside LIST/RECORD field types are carried as [`RecordFieldType::NotNull`].
fn gql_record_to_record_field_types(
    record_type: &RecordType,
    depth: u32,
    span: crate::SourceSpan,
) -> Result<Option<RecordFieldTypes>, ExecutorError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(ExecutorError::ImplementationDefined {
            detail: "nested RECORD property type exceeds parser nesting limit",
        });
    }
    match record_type {
        RecordType::Open => Ok(None),
        RecordType::Closed(fields) => {
            let defs = fields
                .iter()
                .map(|(name, gql_type)| {
                    let (gql_type, required) = strip_top_level_not_null(gql_type);
                    Ok(RecordFieldTypeDef {
                        name: name.clone(),
                        field_type: gql_type_to_record_field_type(gql_type, depth, span)?,
                        required,
                    })
                })
                .collect::<Result<Vec<_>, ExecutorError>>()?;
            Ok(Some(RecordFieldTypes(defs)))
        }
    }
}

fn gql_type_to_record_field_type(
    gql_type: &GqlType,
    depth: u32,
    span: crate::SourceSpan,
) -> Result<RecordFieldType, ExecutorError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(ExecutorError::ImplementationDefined {
            detail: "nested RECORD field type exceeds parser nesting limit",
        });
    }
    match gql_type {
        GqlType::List(inner) => Ok(RecordFieldType::List(Box::new(
            gql_type_to_record_field_type(inner, depth + 1, span)?,
        ))),
        GqlType::Record(record_type) => {
            match gql_record_to_record_field_types(record_type, depth + 1, span)? {
                Some(fields) => Ok(RecordFieldType::Record(Box::new(fields))),
                None => Ok(RecordFieldType::OpenRecord),
            }
        }
        GqlType::NotNull(inner) => Ok(RecordFieldType::NotNull(Box::new(
            gql_type_to_record_field_type(inner, depth, span)?,
        ))),
        _ => gql_type_to_scalar_property_value_type(gql_type).map(
            |(value_type, decimal_type, character_string_type, byte_string_type)| {
                if let Some(character_string_type) = character_string_type {
                    return RecordFieldType::CharacterString(character_string_type);
                }
                if let Some(decimal_type) = decimal_type {
                    return RecordFieldType::Decimal(decimal_type);
                }
                if let Some(byte_string_type) = byte_string_type {
                    return RecordFieldType::ByteString(byte_string_type);
                }
                RecordFieldType::Scalar(value_type)
            },
        ),
    }
}

fn gql_type_to_property_element_type(
    gql_type: &GqlType,
    depth: u32,
) -> Result<PropertyElementType, ExecutorError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(ExecutorError::ImplementationDefined {
            detail: "nested LIST property type exceeds parser nesting limit",
        });
    }
    match gql_type {
        GqlType::List(inner) => Ok(PropertyElementType::List(Box::new(
            gql_type_to_property_element_type(inner, depth + 1)?,
        ))),
        GqlType::NotNull(inner) => Ok(PropertyElementType::NotNull(Box::new(
            gql_type_to_property_element_type(inner, depth)?,
        ))),
        _ => gql_type_to_scalar_property_value_type(gql_type).map(
            |(value_type, decimal_type, character_string_type, byte_string_type)| {
                if let Some(character_string_type) = character_string_type {
                    return PropertyElementType::CharacterString(character_string_type);
                }
                if let Some(decimal_type) = decimal_type {
                    return PropertyElementType::Decimal(decimal_type);
                }
                if let Some(byte_string_type) = byte_string_type {
                    return PropertyElementType::ByteString(byte_string_type);
                }
                PropertyElementType::Scalar(value_type)
            },
        ),
    }
}

fn gql_type_to_scalar_property_value_type(
    gql_type: &GqlType,
) -> Result<LoweredScalarPropertyType, ExecutorError> {
    let value_type = match gql_type {
        GqlType::NotNull(inner) => return gql_type_to_scalar_property_value_type(inner),
        GqlType::String | GqlType::CharacterString(_) => PropertyValueType::String,
        GqlType::Boolean => PropertyValueType::Bool,
        GqlType::Integer
        | GqlType::Int8
        | GqlType::Int16
        | GqlType::Int32
        | GqlType::Int64
        | GqlType::SmallInt
        | GqlType::BigInt => PropertyValueType::Int,
        GqlType::Uint8
        | GqlType::Uint16
        | GqlType::Uint32
        | GqlType::Uint64
        | GqlType::USmallInt
        | GqlType::Uint
        | GqlType::UBigInt => PropertyValueType::Uint,
        GqlType::Int128 => PropertyValueType::Int128,
        GqlType::Uint128 => PropertyValueType::Uint128,
        GqlType::Float | GqlType::Float64 | GqlType::Double => PropertyValueType::Float,
        GqlType::Float32 | GqlType::Real => PropertyValueType::Float32,
        GqlType::Decimal | GqlType::DecimalExact(_) => PropertyValueType::Decimal,
        GqlType::Bytes | GqlType::ByteString(_) => PropertyValueType::Bytes,
        GqlType::Uuid => PropertyValueType::Uuid,
        GqlType::Json => PropertyValueType::Json,
        GqlType::ZonedDateTime => PropertyValueType::ZonedDateTime,
        GqlType::LocalDateTime => PropertyValueType::LocalDateTime,
        GqlType::Date => PropertyValueType::Date,
        GqlType::ZonedTime => PropertyValueType::ZonedTime,
        GqlType::LocalTime => PropertyValueType::LocalTime,
        GqlType::Duration => PropertyValueType::Duration,
        GqlType::DurationYearToMonth => PropertyValueType::DurationYearToMonth,
        GqlType::DurationDayToSecond => PropertyValueType::DurationDayToSecond,
        GqlType::Vector => PropertyValueType::Vector,
        GqlType::Path
        | GqlType::GraphRef
        | GqlType::NodeRef
        | GqlType::EdgeRef
        | GqlType::TableRef(_) => {
            return Err(ExecutorError::data_exception(
                crate::DataExceptionSubclass::InvalidValueType,
                "references and paths are query-only, not property types (IV011)",
                crate::SourceSpan::default(),
            ));
        }
        GqlType::Null => PropertyValueType::Null,
        GqlType::Any
        | GqlType::AnyProperty
        | GqlType::ClosedDynamicUnion(_)
        | GqlType::Record(_)
        | GqlType::List(_)
        | GqlType::BoundedList { .. }
        | GqlType::Nothing => {
            return Err(ExecutorError::ImplementationDefined {
                detail: "type property GQL type not supported as property value type (Phase A)",
            });
        }
    };
    let decimal_type = match gql_type {
        GqlType::DecimalExact(decimal_type) => Some(*decimal_type),
        _ => None,
    };
    let character_string_type = match gql_type {
        GqlType::CharacterString(character_string_type) => {
            Some(core_character_string_type(character_string_type))
        }
        _ => None,
    };
    let byte_string_type = match gql_type {
        GqlType::ByteString(byte_string_type) => Some(core_byte_string_type(byte_string_type)),
        _ => None,
    };
    Ok((
        value_type,
        decimal_type,
        character_string_type,
        byte_string_type,
    ))
}

pub(super) fn render_property_value_type(
    value_type: PropertyValueType,
    list_element_type: Option<&PropertyElementType>,
    record_field_types: Option<&RecordFieldTypes>,
    decimal_type: Option<DecimalType>,
    character_string_type: Option<CoreCharacterStringType>,
    byte_string_type: Option<CoreByteStringType>,
) -> String {
    if value_type == PropertyValueType::Decimal
        && let Some(decimal_type) = decimal_type
    {
        return render_decimal_property_type(decimal_type);
    }
    if value_type == PropertyValueType::String
        && let Some(character_string_type) = character_string_type
    {
        return render_character_string_property_type(character_string_type);
    }
    if value_type == PropertyValueType::Bytes
        && let Some(byte_string_type) = byte_string_type
    {
        return render_byte_string_property_type(byte_string_type);
    }
    if value_type == PropertyValueType::List
        && let Some(element_type) = list_element_type
    {
        return format!("LIST<{}>", render_property_element_type(element_type));
    }
    // A closed RECORD carries its field-type descriptor; render the structure
    // so SHOW round-trips `RECORD { name :: TYPE, ... }` rather than a bare
    // `RECORD` that loses the open-vs-closed distinction. An open/bare RECORD
    // (no descriptor) stays `RECORD`.
    if value_type == PropertyValueType::RecordTyped
        && let Some(fields) = record_field_types
    {
        return render_record_field_types(fields);
    }
    scalar_property_value_type_name(value_type).to_owned()
}

fn render_record_field_types(fields: &RecordFieldTypes) -> String {
    if fields.0.is_empty() {
        return "RECORD {}".to_owned();
    }
    let rendered = fields
        .0
        .iter()
        .map(|field| {
            let nullability = if field.required { " NOT NULL" } else { "" };
            format!(
                "{} :: {}{}",
                fmt_ident(field.name.clone()),
                render_record_field_type(&field.field_type),
                nullability
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("RECORD {{ {rendered} }}")
}

fn render_record_field_type(field_type: &RecordFieldType) -> String {
    match field_type {
        RecordFieldType::Scalar(value_type) => {
            scalar_property_value_type_name(*value_type).to_owned()
        }
        RecordFieldType::CharacterString(character_string_type) => {
            render_character_string_property_type(*character_string_type)
        }
        RecordFieldType::Decimal(decimal_type) => render_decimal_property_type(*decimal_type),
        RecordFieldType::ByteString(byte_string_type) => {
            render_byte_string_property_type(*byte_string_type)
        }
        RecordFieldType::List(inner) => format!("LIST<{}>", render_record_field_type(inner)),
        RecordFieldType::OpenRecord => "RECORD".to_owned(),
        RecordFieldType::Record(inner) => render_record_field_types(inner),
        RecordFieldType::NotNull(inner) => format!("{} NOT NULL", render_record_field_type(inner)),
        _ => "<unsupported-record-field>".to_owned(),
    }
}

fn render_property_element_type(element_type: &PropertyElementType) -> String {
    match element_type {
        PropertyElementType::Scalar(value_type) => {
            scalar_property_value_type_name(*value_type).to_owned()
        }
        PropertyElementType::CharacterString(character_string_type) => {
            render_character_string_property_type(*character_string_type)
        }
        PropertyElementType::Decimal(decimal_type) => render_decimal_property_type(*decimal_type),
        PropertyElementType::ByteString(byte_string_type) => {
            render_byte_string_property_type(*byte_string_type)
        }
        PropertyElementType::List(inner) => {
            format!("LIST<{}>", render_property_element_type(inner))
        }
        PropertyElementType::NotNull(inner) => {
            format!("{} NOT NULL", render_property_element_type(inner))
        }
        _ => "<unsupported-element>".to_owned(),
    }
}

fn render_decimal_property_type(decimal_type: DecimalType) -> String {
    if decimal_type.scale == 0 {
        format!("DECIMAL({})", decimal_type.precision)
    } else {
        format!(
            "DECIMAL({}, {})",
            decimal_type.precision, decimal_type.scale
        )
    }
}

fn render_character_string_property_type(character_string_type: CoreCharacterStringType) -> String {
    if character_string_type.min_len == 0 {
        format!("STRING({})", character_string_type.max_len)
    } else {
        format!(
            "STRING({}, {})",
            character_string_type.min_len, character_string_type.max_len
        )
    }
}

fn render_byte_string_property_type(byte_string_type: CoreByteStringType) -> String {
    if byte_string_type.min_len == 0 {
        format!("BYTES({})", byte_string_type.max_len)
    } else {
        format!(
            "BYTES({}, {})",
            byte_string_type.min_len, byte_string_type.max_len
        )
    }
}

const fn core_character_string_type(
    character_string_type: &crate::ast::CharacterStringType,
) -> CoreCharacterStringType {
    CoreCharacterStringType {
        min_len: character_string_type.min_len,
        max_len: character_string_type.max_len,
    }
}

const fn core_byte_string_type(
    byte_string_type: &crate::ast::ByteStringType,
) -> CoreByteStringType {
    CoreByteStringType {
        min_len: byte_string_type.min_len,
        max_len: byte_string_type.max_len,
    }
}

fn scalar_property_value_type_name(value_type: PropertyValueType) -> &'static str {
    match value_type {
        PropertyValueType::Bool => "BOOLEAN",
        PropertyValueType::Int => "INTEGER",
        PropertyValueType::Uint => "UINT64",
        PropertyValueType::Int128 => "INT128",
        PropertyValueType::Uint128 => "UINT128",
        PropertyValueType::Float => "FLOAT",
        PropertyValueType::Float32 => "FLOAT32",
        PropertyValueType::Decimal => "DECIMAL",
        PropertyValueType::String => "STRING",
        PropertyValueType::Bytes => "BYTES",
        PropertyValueType::List => "LIST",
        PropertyValueType::Record | PropertyValueType::RecordTyped => "RECORD",
        PropertyValueType::Path => "PATH",
        PropertyValueType::NodeRef => "NODE",
        PropertyValueType::EdgeRef => "EDGE",
        PropertyValueType::GraphRef => "GRAPH",
        PropertyValueType::TableRef => "TABLE",
        PropertyValueType::ZonedDateTime => "ZONED DATETIME",
        PropertyValueType::LocalDateTime => "LOCAL DATETIME",
        PropertyValueType::Date => "DATE",
        PropertyValueType::ZonedTime => "ZONED TIME",
        PropertyValueType::LocalTime => "LOCAL TIME",
        PropertyValueType::Duration => "DURATION",
        PropertyValueType::DurationYearToMonth => "DURATION (YEAR TO MONTH)",
        PropertyValueType::DurationDayToSecond => "DURATION (DAY TO SECOND)",
        PropertyValueType::Null => "NULL",
        PropertyValueType::Uuid => "UUID",
        PropertyValueType::Vector => "VECTOR",
        PropertyValueType::Json => "JSON",
    }
}
