//! Structural value membership; shared by runtime and typed parameter admission.

use crate::{ScalarType as S, StructuralType, TypeKind, Value};

impl StructuralType {
    /// Test type membership, not assignment conversion or predicate equality.
    /// Reference membership does not dereference or validate ownership.
    #[must_use]
    pub fn matches(&self, value: &Value) -> bool {
        supported_query_shape(value) && self.matches_supported(value)
    }

    fn matches_supported(&self, value: &Value) -> bool {
        if matches!(value, Value::Null) {
            return self.is_nullable();
        }
        match self.kind() {
            TypeKind::Dynamic => true,
            TypeKind::Property => crate::StoredValue::validate(value).is_ok(),
            TypeKind::Union(members) => members.iter().any(|ty| ty.matches_supported(value)),
            TypeKind::Null | TypeKind::Empty => false,
            TypeKind::Scalar(scalar) => scalar_matches(*scalar, value),
            TypeKind::List { element, max_len } => matches!(value, Value::List(values)
                if max_len.is_none_or(|max| values.len() as u64 <= max)
                    && values.iter().all(|value| element.matches_supported(value))),
            TypeKind::Record(fields) => match value {
                Value::Record(record) => match record.as_ref() {
                    crate::Record::Open(values) => fields.as_ref().is_none_or(|fields| {
                        fields.len() == values.len()
                            && fields.iter().all(|(name, ty)| {
                                values
                                    .iter()
                                    .find(|(other, _)| other == name)
                                    .is_some_and(|(_, value)| ty.matches_supported(value))
                            })
                    }),
                },
                // Legacy positional record IDs are not structural descriptors.
                Value::RecordTyped(_) => false,
                _ => false,
            },
            TypeKind::NodeRef => matches!(value, Value::NodeRef(_)),
            TypeKind::EdgeRef => matches!(value, Value::EdgeRef(_)),
            TypeKind::Path => matches!(value, Value::Path(_)),
            TypeKind::GraphRef => matches!(value, Value::GraphRef(_)),
            TypeKind::TableRef(_) => matches!(value, Value::TableRef(_)),
        }
    }
}

fn supported_query_shape(value: &Value) -> bool {
    let mut pending = vec![(value, 1)];
    while let Some((value, depth)) = pending.pop() {
        if depth > crate::MAX_STORED_VALUE_DEPTH {
            return false;
        }
        match value {
            Value::RecordTyped(_) | Value::Extended { .. } => return false,
            Value::List(values) => pending.extend(values.iter().map(|value| (value, depth + 1))),
            Value::Record(record) => {
                let crate::Record::Open(fields) = record.as_ref();
                let mut names = std::collections::BTreeSet::new();
                for (name, value) in fields {
                    if !names.insert(name) {
                        return false;
                    }
                    pending.push((value, depth + 1));
                }
            }
            _ => {}
        }
    }
    true
}

fn scalar_matches(scalar: S, value: &Value) -> bool {
    match (scalar, value) {
        (S::Boolean, Value::Bool(_)) => true,
        (S::Int8, Value::Int(v)) => i8::try_from(*v).is_ok(),
        (S::Int16, Value::Int(v)) => i16::try_from(*v).is_ok(),
        (S::Int32, Value::Int(v)) => i32::try_from(*v).is_ok(),
        (S::Int64, Value::Int(_)) | (S::Int128, Value::Int128(_)) => true,
        (S::Uint8, Value::Uint(v)) => u8::try_from(*v).is_ok(),
        (S::Uint16, Value::Uint(v)) => u16::try_from(*v).is_ok(),
        (S::Uint32, Value::Uint(v)) => u32::try_from(*v).is_ok(),
        (S::Uint64, Value::Uint(_)) | (S::Uint128, Value::Uint128(_)) => true,
        (S::Float | S::Float64, Value::Float(_)) | (S::Float | S::Float32, Value::Float32(_)) => {
            true
        }
        (S::Decimal(bounds), Value::Decimal(v)) => {
            bounds.is_none_or(|bounds| crate::decimal_fits_type(*v, bounds))
        }
        (S::String(bounds), Value::String(v)) => {
            bounds.is_none_or(|bounds| crate::character_string_fits_type(v, bounds))
        }
        (S::Bytes(bounds), Value::Bytes(v)) => {
            bounds.is_none_or(|bounds| crate::byte_string_fits_type(v, bounds))
        }
        (S::Uuid, Value::Uuid(_))
        | (S::Json, Value::Json(_))
        | (S::Vector, Value::Vector(_))
        | (S::Date, Value::Date(_))
        | (S::LocalDateTime, Value::LocalDateTime(_))
        | (S::ZonedDateTime, Value::ZonedDateTime(_))
        | (S::LocalTime, Value::LocalTime(_))
        | (S::ZonedTime, Value::ZonedTime(_)) => true,
        (S::Duration(qualifier), Value::Duration(v)) => qualifier.is_none_or(|q| q.matches_span(v)),
        _ => false,
    }
}
