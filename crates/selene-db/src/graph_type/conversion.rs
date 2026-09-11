//! Lossless adapter into the graph owner's existing schema families, not a new type system.
use crate::{Error, PathSegment, Result, ScalarType as S, Type, TypeKind as K};
use selene_core::PropertyValueType as P;
use selene_graph::{
    PropertyElementType as E, PropertyTypeDef, RecordFieldType as F, RecordFieldTypeDef,
    RecordFieldTypes,
};

fn unsupported() -> Error {
    Error::invalid_graph_type("structural type is not supported by native property schema")
}

pub(super) fn property(name: PathSegment, ty: &Type) -> Result<PropertyTypeDef> {
    if ty.depth() > 64 {
        return Err(unsupported());
    }
    let mut pending = vec![ty];
    let mut count = 0usize;
    while let Some(ty) = pending.pop() {
        count += 1;
        let children = match ty.kind() {
            K::Record(Some(fields)) => fields.len(),
            K::List { .. } => 1,
            _ => 0,
        };
        if count.saturating_add(pending.len()).saturating_add(children) > 4096 {
            return Err(unsupported());
        }
        match ty.kind() {
            K::Record(Some(fields)) => pending.extend(fields.iter().map(|(_, ty)| ty)),
            K::List { element, .. } => pending.push(element),
            _ => {}
        }
    }
    let mut p = PropertyTypeDef {
        name: selene_core::db_string(name.display()).map_err(Error::invalid_graph_type_source)?,
        value_type: P::Null,
        list_element_type: None,
        required: !ty.is_nullable(),
        default: None,
        immutable: false,
        unique: false,
        decimal_type: None,
        character_string_type: None,
        byte_string_type: None,
        record_field_types: None,
    };
    match ty.kind() {
        K::Scalar(S::Decimal(bounds)) => {
            p.value_type = P::Decimal;
            p.decimal_type = *bounds;
        }
        K::Scalar(S::String(bounds)) => {
            p.value_type = P::String;
            p.character_string_type = *bounds;
        }
        K::Scalar(S::Bytes(bounds)) => {
            p.value_type = P::Bytes;
            p.byte_string_type = *bounds;
        }
        K::Scalar(s) => p.value_type = scalar(*s)?,
        K::List {
            element,
            max_len: None,
        } => {
            p.value_type = P::List;
            p.list_element_type = Some(list(element)?);
        }
        K::Record(None) => p.value_type = P::Record,
        K::Record(Some(fields)) => {
            p.value_type = P::RecordTyped;
            p.record_field_types = Some(record(fields)?);
        }
        _ => return Err(unsupported()),
    }
    if p.structural_type()
        .map_err(Error::invalid_graph_type_source)?
        != *ty
    {
        return Err(unsupported());
    }
    Ok(p)
}
fn scalar(s: S) -> Result<P> {
    let candidates = [
        P::Bool,
        P::Int,
        P::Uint,
        P::Int128,
        P::Uint128,
        P::Float,
        P::Float32,
        P::Decimal,
        P::String,
        P::Bytes,
        P::Uuid,
        P::Json,
        P::Vector,
        P::Date,
        P::LocalDateTime,
        P::ZonedDateTime,
        P::LocalTime,
        P::ZonedTime,
        P::Duration,
        P::DurationYearToMonth,
        P::DurationDayToSecond,
    ];
    candidates
        .into_iter()
        .find(|p| p.structural_type().kind() == &K::Scalar(s))
        .ok_or_else(unsupported)
}
fn list(ty: &Type) -> Result<E> {
    let base = match ty.kind() {
        K::Scalar(S::String(Some(b))) => E::CharacterString(*b),
        K::Scalar(S::Decimal(Some(b))) => E::Decimal(*b),
        K::Scalar(S::Bytes(Some(b))) => E::ByteString(*b),
        K::Scalar(s) => E::Scalar(scalar(*s)?),
        K::List {
            element,
            max_len: None,
        } => E::List(Box::new(list(element)?)),
        _ => return Err(unsupported()),
    };
    Ok(if ty.is_nullable() {
        base
    } else {
        E::NotNull(Box::new(base))
    })
}
fn field(ty: &Type) -> Result<F> {
    let base = match ty.kind() {
        K::Scalar(S::String(Some(b))) => F::CharacterString(*b),
        K::Scalar(S::Decimal(Some(b))) => F::Decimal(*b),
        K::Scalar(S::Bytes(Some(b))) => F::ByteString(*b),
        K::Scalar(s) => F::Scalar(scalar(*s)?),
        K::List {
            element,
            max_len: None,
        } => F::List(Box::new(field(element)?)),
        K::Record(None) => F::OpenRecord,
        K::Record(Some(fields)) => F::Record(Box::new(record(fields)?)),
        _ => return Err(unsupported()),
    };
    Ok(if ty.is_nullable() {
        base
    } else {
        F::NotNull(Box::new(base))
    })
}
fn record(fields: &[(selene_core::DbString, Type)]) -> Result<RecordFieldTypes> {
    if fields.len() > 4096 {
        return Err(unsupported());
    }
    Ok(RecordFieldTypes(
        fields
            .iter()
            .map(|(name, ty)| {
                Ok(RecordFieldTypeDef {
                    name: name.clone(),
                    field_type: field(ty)?,
                    required: !ty.is_nullable(),
                })
            })
            .collect::<Result<_>>()?,
    ))
}
