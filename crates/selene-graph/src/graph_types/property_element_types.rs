//! Typed `LIST<T>` element descriptors for the closed-graph catalog.

use selene_core::{ByteStringType, CharacterStringType, DecimalType, PropertyValueType, Value};
use serde::{Deserialize, Serialize};

/// Persistable element-type descriptor for `LIST<T>` property declarations.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
#[rkyv(
    bytecheck(bounds(__C: rkyv::validation::ArchiveContext)),
    deserialize_bounds(__D::Error: rkyv::rancor::Source),
    serialize_bounds(__S: rkyv::ser::Writer)
)]
#[non_exhaustive]
pub enum PropertyElementType {
    /// Scalar list element type.
    Scalar(PropertyValueType),
    /// STRING list element type with a user-specified length envelope.
    CharacterString(CharacterStringType),
    /// DECIMAL list element type with a user-specified precision/scale envelope.
    Decimal(DecimalType),
    /// BYTES list element type with a user-specified length envelope.
    ByteString(ByteStringType),
    /// Nested list element type.
    List(#[rkyv(omit_bounds)] Box<PropertyElementType>),
    /// Explicitly non-null element type.
    NotNull(#[rkyv(omit_bounds)] Box<PropertyElementType>),
}

impl PropertyElementType {
    /// Return the coarse property-value type for this descriptor.
    #[must_use]
    pub const fn value_type(&self) -> PropertyValueType {
        match self {
            Self::Scalar(value_type) => *value_type,
            Self::CharacterString(_) => PropertyValueType::String,
            Self::Decimal(_) => PropertyValueType::Decimal,
            Self::ByteString(_) => PropertyValueType::Bytes,
            Self::List(_) => PropertyValueType::List,
            Self::NotNull(inner) => inner.value_type(),
        }
    }

    /// Return true when `value` belongs to this element type.
    #[must_use]
    pub fn matches(&self, value: &Value) -> bool {
        self.structural_type().is_ok_and(|ty| ty.matches(value))
    }
}
