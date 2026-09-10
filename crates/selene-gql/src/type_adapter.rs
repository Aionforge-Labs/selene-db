//! The single source/structural-type adapter. Source spellings remain on the
//! immutable AST; semantic descriptors never retain a spelling or arena ID.
//! The reverse adapter exists only for the current planner (deletion F03-PR04).

use selene_core::{ScalarType as S, StructuralType as T, StructuralTypeError as E, TypeKind as K};

use crate::{BindingTableType, GqlType as G, RecordType};

/// Normalize an admitted source descriptor. This does not select optional
/// syntax; the Flagger remains responsible for source capability admission.
#[doc(hidden)]
pub fn normalize_value_type(source: &G) -> Result<T, E> {
    normalize(source, 1)
}

fn normalize(source: &G, depth: usize) -> Result<T, E> {
    if depth > selene_core::MAX_STRUCTURAL_TYPE_DEPTH {
        return Err(E::DepthLimit);
    }
    let scalar = match source {
        G::Any => return Ok(T::DYNAMIC),
        G::AnyProperty => return T::new(K::Property, true),
        G::ClosedDynamicUnion(members) => {
            return T::union(
                members
                    .iter()
                    .map(|ty| normalize(ty, depth + 1))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        G::NotNull(inner) => {
            return normalize(inner, depth + 1).map(|ty| ty.with_nullability(false));
        }
        G::Null => return Ok(T::NULL),
        G::Nothing => return Ok(T::EMPTY),
        G::List(element) => return T::list(normalize(element, depth + 1)?, None),
        G::BoundedList {
            element_type,
            max_len,
        } => return T::list(normalize(element_type, depth + 1)?, Some(*max_len)),
        G::Record(RecordType::Open) => return T::new(K::Record(None), true),
        G::Record(RecordType::Closed(fields)) => {
            return T::record(normalize_fields(fields, depth)?);
        }
        G::TableRef(BindingTableType::Any) => return T::new(K::TableRef(None), true),
        G::TableRef(BindingTableType::Closed(fields)) => {
            return T::new(
                K::TableRef(Some(normalize_fields(fields, depth)?.into())),
                true,
            );
        }
        G::GraphRef => return T::new(K::GraphRef, true),
        G::NodeRef => return Ok(T::NODE),
        G::EdgeRef => return Ok(T::EDGE),
        G::Path => return Ok(T::PATH),
        G::Boolean => S::Boolean,
        G::Int8 => S::Int8,
        G::Int16 | G::SmallInt => S::Int16,
        G::Int32 => S::Int32,
        G::Integer | G::Int64 | G::BigInt => S::Int64,
        G::Int128 => S::Int128,
        G::Uint8 => S::Uint8,
        G::Uint16 | G::USmallInt => S::Uint16,
        G::Uint32 | G::Uint => S::Uint32,
        G::Uint64 | G::UBigInt => S::Uint64,
        G::Uint128 => S::Uint128,
        G::Float => S::Float,
        G::Float32 | G::Real => S::Float32,
        G::Float64 | G::Double => S::Float64,
        G::Decimal => S::Decimal(None),
        G::DecimalExact(bounds) => S::Decimal(Some(*bounds)),
        G::String => S::String(None),
        G::CharacterString(bounds) => S::String(Some(selene_core::CharacterStringType {
            min_len: bounds.min_len,
            max_len: bounds.max_len,
        })),
        G::Bytes => S::Bytes(None),
        G::ByteString(bounds) => S::Bytes(Some(selene_core::ByteStringType {
            min_len: bounds.min_len,
            max_len: bounds.max_len,
        })),
        G::Uuid => S::Uuid,
        G::Json => S::Json,
        G::Vector => S::Vector,
        G::Date => S::Date,
        G::LocalDateTime => S::LocalDateTime,
        G::ZonedDateTime => S::ZonedDateTime,
        G::LocalTime => S::LocalTime,
        G::ZonedTime => S::ZonedTime,
        G::Duration => S::Duration(None),
        G::DurationYearToMonth => {
            S::Duration(Some(selene_core::DurationTypeQualifier::YearToMonth))
        }
        G::DurationDayToSecond => {
            S::Duration(Some(selene_core::DurationTypeQualifier::DayToSecond))
        }
    };
    T::from_scalar(scalar)
}

fn normalize_fields(
    fields: &[(selene_core::DbString, G)],
    depth: usize,
) -> Result<Vec<(selene_core::DbString, T)>, E> {
    fields
        .iter()
        .map(|(name, ty)| Ok((name.clone(), normalize(ty, depth + 1)?)))
        .collect()
}

/// Lower a normalized descriptor into the legacy planner's type vocabulary.
/// This is a derived view, not a second semantic authority or source syntax.
#[doc(hidden)]
pub fn lower_value_type(ty: &T) -> G {
    let base = match ty.kind() {
        K::Dynamic => G::Any,
        K::Property => G::AnyProperty,
        K::Null => return G::Null,
        K::Empty => return G::Nothing,
        K::Union(members) => {
            return G::ClosedDynamicUnion(
                members
                    .iter()
                    .map(|member| {
                        lower_value_type(&member.clone().with_nullability(ty.is_nullable()))
                    })
                    .collect(),
            );
        }
        K::List {
            element,
            max_len: None,
        } => G::List(Box::new(lower_value_type(element))),
        K::List {
            element,
            max_len: Some(max_len),
        } => G::BoundedList {
            element_type: Box::new(lower_value_type(element)),
            max_len: *max_len,
        },
        K::Record(None) => G::Record(RecordType::Open),
        K::Record(Some(fields)) => G::Record(RecordType::Closed(lower_fields(fields))),
        K::TableRef(None) => G::TableRef(BindingTableType::Any),
        K::TableRef(Some(fields)) => G::TableRef(BindingTableType::Closed(lower_fields(fields))),
        K::NodeRef => G::NodeRef,
        K::EdgeRef => G::EdgeRef,
        K::GraphRef => G::GraphRef,
        K::Path => G::Path,
        K::Scalar(scalar) => lower_scalar(*scalar),
    };
    if ty.is_nullable() {
        base
    } else {
        G::NotNull(Box::new(base))
    }
}

fn lower_fields(fields: &[(selene_core::DbString, T)]) -> Vec<(selene_core::DbString, G)> {
    fields
        .iter()
        .map(|(name, ty)| (name.clone(), lower_value_type(ty)))
        .collect()
}

fn lower_scalar(scalar: S) -> G {
    match scalar {
        S::Boolean => G::Boolean,
        S::Int8 => G::Int8,
        S::Int16 => G::Int16,
        S::Int32 => G::Int32,
        S::Int64 => G::Integer,
        S::Int128 => G::Int128,
        S::Uint8 => G::Uint8,
        S::Uint16 => G::Uint16,
        S::Uint32 => G::Uint32,
        S::Uint64 => G::Uint64,
        S::Uint128 => G::Uint128,
        S::Float => G::Float,
        S::Float32 => G::Float32,
        S::Float64 => G::Float64,
        S::Decimal(None) => G::Decimal,
        S::Decimal(Some(bounds)) => G::DecimalExact(bounds),
        S::String(None) => G::String,
        S::String(Some(bounds)) => G::CharacterString(crate::ast::types::CharacterStringType {
            min_len: bounds.min_len,
            max_len: bounds.max_len,
            form: crate::ast::types::CharacterStringTypeForm::StringMinMax,
        }),
        S::Bytes(None) => G::Bytes,
        S::Bytes(Some(bounds)) => G::ByteString(crate::ast::types::ByteStringType {
            min_len: bounds.min_len,
            max_len: bounds.max_len,
            form: crate::ast::types::ByteStringTypeForm::BytesMinMax,
        }),
        S::Uuid => G::Uuid,
        S::Json => G::Json,
        S::Vector => G::Vector,
        S::Date => G::Date,
        S::LocalDateTime => G::LocalDateTime,
        S::ZonedDateTime => G::ZonedDateTime,
        S::LocalTime => G::LocalTime,
        S::ZonedTime => G::ZonedTime,
        S::Duration(None) => G::Duration,
        S::Duration(Some(selene_core::DurationTypeQualifier::YearToMonth)) => {
            G::DurationYearToMonth
        }
        S::Duration(Some(selene_core::DurationTypeQualifier::DayToSecond)) => {
            G::DurationDayToSecond
        }
    }
}
