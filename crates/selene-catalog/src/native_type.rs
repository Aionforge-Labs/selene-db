//! Depth-budgeted deserialization before recursive signature values are built.

use serde::{
    Deserialize, Deserializer,
    de::{DeserializeSeed, EnumAccess, VariantAccess, Visitor},
};

use crate::NativeType;

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
