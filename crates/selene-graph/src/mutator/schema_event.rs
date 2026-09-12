//! Live schema-event adapter used by ALTER admission, not a persisted decoder.
//! The compiler/event model remains F03-PR04/F04-PR09 owned. Format 2 stores its
//! complete logical schema directly and never dispatches these events.

use crate::{
    EdgeEndpointDef, GraphTypeDef, PropertyDefaultValue, PropertyElementType as L, PropertyTypeDef,
    ProviderError, RecordFieldType as R, RecordFieldTypeDef, RecordFieldTypes,
};
use selene_core::{
    ByteStringType, CharacterStringType, PredefinedValueType as P, PropertyValueType as V,
    ValueType,
};

fn inconsistent(reason: impl Into<String>) -> ProviderError {
    ProviderError::Inconsistent {
        reason: reason.into(),
    }
}

pub(crate) fn property(
    property: &selene_core::PropertyDef,
) -> Result<PropertyTypeDef, ProviderError> {
    let (
        value_type,
        list_element_type,
        record_field_types,
        character_string_type,
        byte_string_type,
    ) = match property.record_fields.as_deref() {
        Some(selene_core::RecordFieldStructure::Open) => (V::RecordTyped, None, None, None, None),
        Some(selene_core::RecordFieldStructure::Closed(defs)) => (
            V::RecordTyped,
            None,
            Some(record_fields(defs, 1)?),
            None,
            None,
        ),
        None => {
            let (ty, list) = value_type(&property.value_type)?;
            (
                ty,
                list,
                None,
                character_type(&property.value_type, ty),
                byte_type(&property.value_type, ty),
            )
        }
    };
    Ok(PropertyTypeDef {
        name: property.name.clone(),
        value_type,
        list_element_type,
        required: !property.nullable || property.value_type.not_null,
        default: property
            .default
            .as_ref()
            .map(|value| {
                PropertyDefaultValue::from_value(value)
                    .ok_or_else(|| inconsistent("schema event default has unsupported value type"))
            })
            .transpose()?,
        immutable: property.immutable,
        unique: property.unique,
        decimal_type: if value_type == V::Decimal {
            property.value_type.decimal_type
        } else {
            None
        },
        character_string_type,
        byte_string_type,
        record_field_types,
    })
}
fn record_fields(
    defs: &[selene_core::RecordFieldStructureDef],
    depth: u32,
) -> Result<RecordFieldTypes, ProviderError> {
    if depth > crate::graph_types::MAX_RECORD_TYPE_NESTING {
        return Err(inconsistent("schema event exceeds RECORD nesting limit"));
    }
    Ok(RecordFieldTypes(
        defs.iter()
            .map(|field| {
                Ok(RecordFieldTypeDef {
                    name: field.name.clone(),
                    field_type: record_field(&field.field_type, depth)?,
                    required: field.required,
                })
            })
            .collect::<Result<_, ProviderError>>()?,
    ))
}
fn record_field(
    field: &selene_core::RecordFieldStructureType,
    depth: u32,
) -> Result<R, ProviderError> {
    use selene_core::{RecordFieldStructure as S, RecordFieldStructureType as F};
    Ok(match field {
        F::Scalar(ty) => R::Scalar(*ty),
        F::CharacterString(ty) => R::CharacterString(*ty),
        F::Decimal(ty) => R::Decimal(*ty),
        F::ByteString(ty) => R::ByteString(*ty),
        F::List(inner) => R::List(Box::new(record_field(inner, depth + 1)?)),
        F::Record(inner) => match inner.as_ref() {
            S::Closed(fields) => R::Record(Box::new(record_fields(fields, depth + 1)?)),
            S::Open => R::OpenRecord,
        },
        F::NotNull(inner) => R::NotNull(Box::new(record_field(inner, depth)?)),
    })
}
fn scalar_descriptors(ty: &ValueType, container: bool) -> Result<(), ProviderError> {
    if ty.decimal_type.is_some() && (container || ty.predefined != Some(P::Decimal))
        || ty.character_string_type.is_some() && (container || ty.predefined != Some(P::String))
        || ty.byte_string_type.is_some() && (container || ty.predefined != Some(P::Bytes))
    {
        return Err(inconsistent(
            "schema event scalar descriptor disagrees with value type",
        ));
    }
    Ok(())
}
fn value_type(ty: &ValueType) -> Result<(V, Option<L>), ProviderError> {
    if let Some(inner) = ty.list_of.as_deref() {
        scalar_descriptors(ty, true)?;
        return Ok((V::List, Some(element(inner, 1)?)));
    }
    if ty.record.is_some() {
        scalar_descriptors(ty, true)?;
        return Ok((V::RecordTyped, None));
    }
    if ty.union.is_some() {
        return Err(inconsistent("schema event uses unsupported union type"));
    }
    scalar_descriptors(ty, false)?;
    Ok((
        ty.predefined
            .map(predefined)
            .transpose()?
            .unwrap_or(V::Null),
        None,
    ))
}
fn element(ty: &ValueType, depth: u32) -> Result<L, ProviderError> {
    if depth > crate::graph_types::MAX_LIST_TYPE_NESTING {
        return Err(inconsistent("schema event exceeds LIST nesting limit"));
    }
    let inner = if let Some(inner) = ty.list_of.as_deref() {
        scalar_descriptors(ty, true)?;
        L::List(Box::new(element(inner, depth + 1)?))
    } else {
        if ty.record.is_some() || ty.union.is_some() {
            return Err(inconsistent(
                "schema event list has unsupported nested type",
            ));
        }
        scalar_descriptors(ty, false)?;
        match ty.predefined {
            Some(P::String) if ty.character_string_type.is_some() => L::CharacterString(
                ty.character_string_type
                    .ok_or_else(|| inconsistent("missing string descriptor"))?,
            ),
            Some(P::Decimal) if ty.decimal_type.is_some() => L::Decimal(
                ty.decimal_type
                    .ok_or_else(|| inconsistent("missing decimal descriptor"))?,
            ),
            Some(P::Bytes) if ty.byte_string_type.is_some() => L::ByteString(
                ty.byte_string_type
                    .ok_or_else(|| inconsistent("missing bytes descriptor"))?,
            ),
            Some(value) => L::Scalar(predefined(value)?),
            None => L::Scalar(V::Null),
        }
    };
    Ok(if ty.not_null {
        L::NotNull(Box::new(inner))
    } else {
        inner
    })
}
fn character_type(ty: &ValueType, value: V) -> Option<CharacterStringType> {
    if value == V::String {
        ty.character_string_type
    } else {
        None
    }
}
fn byte_type(ty: &ValueType, value: V) -> Option<ByteStringType> {
    if value == V::Bytes {
        ty.byte_string_type
    } else {
        None
    }
}
fn predefined(ty: P) -> Result<V, ProviderError> {
    Ok(match ty {
        P::Bool => V::Bool,
        P::Int | P::Int8 | P::Int16 | P::Int32 | P::Int64 => V::Int,
        P::Int128 => V::Int128,
        P::Uint | P::Uint8 | P::Uint16 | P::Uint32 | P::Uint64 => V::Uint,
        P::Uint128 => V::Uint128,
        P::Float | P::Float64 => V::Float,
        P::Float32 => V::Float32,
        P::Decimal => V::Decimal,
        P::String => V::String,
        P::Bytes => V::Bytes,
        P::Date => V::Date,
        P::LocalTime => V::LocalTime,
        P::ZonedTime => V::ZonedTime,
        P::LocalDateTime => V::LocalDateTime,
        P::ZonedDateTime => V::ZonedDateTime,
        P::Duration => V::Duration,
        P::DurationYearToMonth => V::DurationYearToMonth,
        P::DurationDayToSecond => V::DurationDayToSecond,
        P::NodeRef => V::NodeRef,
        P::EdgeRef => V::EdgeRef,
        P::GraphRef => V::GraphRef,
        P::TableRef => V::TableRef,
        P::Path => V::Path,
        P::Uuid => V::Uuid,
        P::Vector => V::Vector,
        P::Json => V::Json,
        P::Extended(_) => return Err(inconsistent("schema event uses unsupported extended type")),
    })
}
pub(crate) fn endpoint(
    graph: &GraphTypeDef,
    endpoint: &selene_core::EdgeEndpointDef,
    role: &str,
) -> Result<EdgeEndpointDef, ProviderError> {
    let resolve = |name: &selene_core::NodeTypeRef| {
        graph.node_type_index_for(name.0.clone()).ok_or_else(|| {
            inconsistent(format!(
                "schema event references unknown {role} node type {}",
                name.0
            ))
        })
    };
    Ok(match endpoint {
        selene_core::EdgeEndpointDef::Any => EdgeEndpointDef::Any,
        selene_core::EdgeEndpointDef::NodeType(name) => EdgeEndpointDef::NodeType(resolve(name)?),
        selene_core::EdgeEndpointDef::OneOf(names) => {
            EdgeEndpointDef::one_of(names.iter().map(resolve).collect::<Result<Vec<_>, _>>()?)
        }
    })
}
