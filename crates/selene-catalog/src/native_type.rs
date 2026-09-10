//! Depth-budgeted deserialization before recursive signature values are built.

use serde::{
    Deserialize, Deserializer,
    de::{DeserializeSeed, EnumAccess, VariantAccess, Visitor},
};

use crate::NativeType;

impl NativeType {
    /// Resolve catalog signature metadata through the shared structural type
    /// service. Catalog encoding is not the semantic type authority.
    pub fn structural_type(
        &self,
    ) -> Result<selene_core::StructuralType, selene_core::StructuralTypeError> {
        use selene_core::{ScalarType as S, StructuralType as T, TypeKind as K};
        Ok(match self {
            Self::Any => T::DYNAMIC,
            Self::AnyProperty => T::new(K::Property, true)?,
            Self::Boolean => T::BOOLEAN,
            Self::Integer | Self::Int64 => T::INT64,
            Self::Uint64 => T::UINT64,
            Self::Float => T::from_scalar(S::Float)?,
            Self::Float64 => T::FLOAT64,
            Self::String => T::STRING,
            Self::Vector => T::VECTOR,
            Self::Json => T::JSON,
            Self::NodeRef => T::NODE,
            Self::EdgeRef => T::EDGE,
            Self::GraphRef => T::new(K::GraphRef, true)?,
            Self::OpenRecord => T::new(K::Record(None), true)?,
            Self::List(element) => T::list(element.structural_type()?, None)?,
        })
    }
}

pub(crate) const MAX_NATIVE_TYPE_DEPTH: u8 = 64;

#[derive(Deserialize)]
#[serde(field_identifier)]
enum Variant {
    Any,
    AnyProperty,
    Boolean,
    Integer,
    Int64,
    Uint64,
    Float,
    Float64,
    String,
    Vector,
    Json,
    NodeRef,
    EdgeRef,
    GraphRef,
    OpenRecord,
    List,
}

struct TypeSeed(u8);

impl<'de> DeserializeSeed<'de> for TypeSeed {
    type Value = NativeType;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<NativeType, D::Error> {
        if self.0 > MAX_NATIVE_TYPE_DEPTH {
            return Err(serde::de::Error::custom("native_type_depth"));
        }
        deserializer.deserialize_enum(
            "NativeType",
            &[
                "Any",
                "AnyProperty",
                "Boolean",
                "Integer",
                "Int64",
                "Uint64",
                "Float",
                "Float64",
                "String",
                "Vector",
                "Json",
                "NodeRef",
                "EdgeRef",
                "GraphRef",
                "OpenRecord",
                "List",
            ],
            self,
        )
    }
}

impl<'de> Visitor<'de> for TypeSeed {
    type Value = NativeType;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a native signature type with at most 64 list wrappers")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, access: A) -> Result<NativeType, A::Error> {
        let (variant, payload) = access.variant::<Variant>()?;
        let ty = match variant {
            Variant::Any => NativeType::Any,
            Variant::AnyProperty => NativeType::AnyProperty,
            Variant::Boolean => NativeType::Boolean,
            Variant::Integer => NativeType::Integer,
            Variant::Int64 => NativeType::Int64,
            Variant::Uint64 => NativeType::Uint64,
            Variant::Float => NativeType::Float,
            Variant::Float64 => NativeType::Float64,
            Variant::String => NativeType::String,
            Variant::Vector => NativeType::Vector,
            Variant::Json => NativeType::Json,
            Variant::NodeRef => NativeType::NodeRef,
            Variant::EdgeRef => NativeType::EdgeRef,
            Variant::GraphRef => NativeType::GraphRef,
            Variant::OpenRecord => NativeType::OpenRecord,
            Variant::List => {
                return payload
                    .newtype_variant_seed(TypeSeed(self.0 + 1))
                    .map(|inner| NativeType::List(Box::new(inner)));
            }
        };
        payload.unit_variant()?;
        Ok(ty)
    }
}

impl<'de> Deserialize<'de> for NativeType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        TypeSeed(0).deserialize(deserializer)
    }
}
