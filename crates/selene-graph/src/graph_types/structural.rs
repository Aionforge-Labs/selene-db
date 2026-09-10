//! Legacy graph schema descriptors are adapters to the core structural service.

use super::{PropertyElementType, PropertyTypeDef, RecordFieldType, RecordFieldTypes};
use selene_core::{ScalarType as S, StructuralType as T, StructuralTypeError as E, TypeKind as K};

impl PropertyTypeDef {
    /// Resolve legacy schema metadata into its normalized structural meaning.
    pub fn structural_type(&self) -> Result<T, E> {
        let base = if let Some(fields) = &self.record_field_types {
            fields.structural_type()?
        } else if let Some(element) = &self.list_element_type {
            T::list(element.structural_type()?, None)?
        } else if let Some(bounds) = self.decimal_type {
            T::from_scalar(S::Decimal(Some(bounds)))?
        } else if let Some(bounds) = self.character_string_type {
            T::from_scalar(S::String(Some(bounds)))?
        } else if let Some(bounds) = self.byte_string_type {
            T::from_scalar(S::Bytes(Some(bounds)))?
        } else {
            self.value_type.structural_type()
        };
        Ok(base.with_nullability(!self.required))
    }
}

impl PropertyElementType {
    /// Normalize a legacy list-element descriptor.
    pub fn structural_type(&self) -> Result<T, E> {
        let mut cursor = self;
        let mut depth = 1;
        loop {
            if depth > selene_core::MAX_STRUCTURAL_TYPE_DEPTH {
                return Err(E::DepthLimit);
            }
            match cursor {
                Self::List(inner) | Self::NotNull(inner) => {
                    cursor = inner;
                    depth += 1;
                }
                _ => break,
            }
        }
        self.normalize()
    }

    fn normalize(&self) -> Result<T, E> {
        match self {
            Self::Scalar(tag) => Ok(tag.structural_type()),
            Self::CharacterString(bounds) => T::from_scalar(S::String(Some(*bounds))),
            Self::Decimal(bounds) => T::from_scalar(S::Decimal(Some(*bounds))),
            Self::ByteString(bounds) => T::from_scalar(S::Bytes(Some(*bounds))),
            Self::List(inner) => T::list(inner.normalize()?, None),
            Self::NotNull(inner) => Ok(inner.normalize()?.with_nullability(false)),
        }
    }
}

impl RecordFieldType {
    /// Normalize a legacy record-field descriptor.
    pub fn structural_type(&self) -> Result<T, E> {
        validate_record_depth(vec![(self, 1)])?;
        self.normalize()
    }

    fn normalize(&self) -> Result<T, E> {
        match self {
            Self::Scalar(tag) => Ok(tag.structural_type()),
            Self::CharacterString(bounds) => T::from_scalar(S::String(Some(*bounds))),
            Self::Decimal(bounds) => T::from_scalar(S::Decimal(Some(*bounds))),
            Self::ByteString(bounds) => T::from_scalar(S::Bytes(Some(*bounds))),
            Self::List(inner) => T::list(inner.normalize()?, None),
            Self::NotNull(inner) => Ok(inner.normalize()?.with_nullability(false)),
            Self::OpenRecord => T::new(K::Record(None), true),
            Self::Record(fields) => fields.normalize(),
        }
    }
}

impl RecordFieldTypes {
    /// Normalize exact field names and their recursive descriptors.
    pub fn structural_type(&self) -> Result<T, E> {
        validate_record_depth(self.0.iter().map(|field| (&field.field_type, 2)).collect())?;
        self.normalize()
    }

    fn normalize(&self) -> Result<T, E> {
        T::record(
            self.0
                .iter()
                .map(|field| {
                    let ty = field.field_type.normalize()?;
                    let nullable = !field.required && ty.is_nullable();
                    Ok((field.name.clone(), ty.with_nullability(nullable)))
                })
                .collect::<Result<Vec<_>, E>>()?,
        )
    }
}

fn validate_record_depth(mut pending: Vec<(&RecordFieldType, usize)>) -> Result<(), E> {
    while let Some((ty, depth)) = pending.pop() {
        if depth > selene_core::MAX_STRUCTURAL_TYPE_DEPTH {
            return Err(E::DepthLimit);
        }
        match ty {
            RecordFieldType::List(inner) | RecordFieldType::NotNull(inner) => {
                pending.push((inner, depth + 1))
            }
            RecordFieldType::Record(fields) => {
                pending.extend(fields.0.iter().map(|field| (&field.field_type, depth + 1)))
            }
            _ => {}
        }
    }
    Ok(())
}
