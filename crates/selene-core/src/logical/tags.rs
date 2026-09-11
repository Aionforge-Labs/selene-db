//! Assigned leaf tags. Numeric assignments, not declaration order, define bytes.

use super::{CodecError as E, CodecResult, Decoder, Encoder};
use crate::{
    PredefinedValueType as P, PropertyValueType as V, SchemaPropertyIndexKind as I,
    SchemaVectorIndexKind as A,
};

macro_rules! tags {
    ($method:ident, $ty:ty, {$($tag:literal => $variant:path),+ $(,)?}) => {
        impl Encoder {
            #[doc = concat!("Encode the assigned format-2 ", stringify!($method), " tag.")]
            pub fn $method(&mut self, value: $ty) -> CodecResult<()> {
                self.u8(match value { $($variant => $tag,)+ })
            }
        }
        impl Decoder<'_, '_> {
            #[doc = concat!("Decode the assigned format-2 ", stringify!($method), " tag.")]
            pub fn $method(&mut self) -> CodecResult<$ty> {
                match self.u8()? { $($tag => Ok($variant),)+ _ => Err(E::Invalid(stringify!($method))) }
            }
        }
    };
}

tags!(index_kind, I, {
    1=>I::Bool, 2=>I::I64, 3=>I::U64, 4=>I::I128, 5=>I::U128, 6=>I::Decimal,
    7=>I::F32, 8=>I::F64, 9=>I::String, 10=>I::Date, 11=>I::LocalDateTime,
    12=>I::ZonedDateTime, 13=>I::LocalTime, 14=>I::ZonedTime, 15=>I::Duration, 16=>I::Uuid
});
tags!(vector_kind, A, {
    1=>A::Flat, 2=>A::HnswSquaredEuclidean, 3=>A::HnswCosine, 4=>A::HnswNegativeInnerProduct,
    5=>A::IvfSquaredEuclidean, 6=>A::IvfCosine, 7=>A::IvfNegativeInnerProduct, 8=>A::TurboQuantCosine
});
tags!(property_kind, V, {
    0=>V::Null, 1=>V::Bool, 2=>V::Int, 3=>V::Uint, 4=>V::Int128, 5=>V::Uint128,
    6=>V::Float, 7=>V::Float32, 8=>V::Decimal, 9=>V::String, 10=>V::Bytes,
    11=>V::List, 12=>V::Record, 13=>V::RecordTyped, 14=>V::Uuid, 15=>V::Vector, 16=>V::Json,
    17=>V::Date, 18=>V::LocalTime, 19=>V::ZonedTime, 20=>V::LocalDateTime, 21=>V::ZonedDateTime,
    22=>V::Duration, 23=>V::DurationYearToMonth, 24=>V::DurationDayToSecond,
    240=>V::Path, 241=>V::NodeRef, 242=>V::EdgeRef, 243=>V::GraphRef, 244=>V::TableRef
});

impl Encoder {
    /// Encode a selected predefined schema type; opaque extension IDs have no representation.
    pub fn predefined(&mut self, value: P) -> CodecResult<()> {
        self.u8(match value {
            P::Bool => 1,
            P::Int => 2,
            P::Int8 => 3,
            P::Int16 => 4,
            P::Int32 => 5,
            P::Int64 => 6,
            P::Int128 => 7,
            P::Uint => 8,
            P::Uint8 => 9,
            P::Uint16 => 10,
            P::Uint32 => 11,
            P::Uint64 => 12,
            P::Uint128 => 13,
            P::Float => 14,
            P::Float32 => 15,
            P::Float64 => 16,
            P::Decimal => 17,
            P::String => 18,
            P::Bytes => 19,
            P::Date => 20,
            P::LocalTime => 21,
            P::ZonedTime => 22,
            P::LocalDateTime => 23,
            P::ZonedDateTime => 24,
            P::Duration => 25,
            P::DurationYearToMonth => 26,
            P::DurationDayToSecond => 27,
            P::Uuid => 28,
            P::Vector => 29,
            P::Json => 30,
            P::NodeRef | P::EdgeRef | P::GraphRef | P::TableRef | P::Path | P::Extended(_) => {
                return Err(E::Semantic);
            }
        })
    }
}
impl Decoder<'_, '_> {
    /// Decode a selected predefined schema type, rejecting query-only families.
    pub fn predefined(&mut self) -> CodecResult<P> {
        Ok(match self.u8()? {
            1 => P::Bool,
            2 => P::Int,
            3 => P::Int8,
            4 => P::Int16,
            5 => P::Int32,
            6 => P::Int64,
            7 => P::Int128,
            8 => P::Uint,
            9 => P::Uint8,
            10 => P::Uint16,
            11 => P::Uint32,
            12 => P::Uint64,
            13 => P::Uint128,
            14 => P::Float,
            15 => P::Float32,
            16 => P::Float64,
            17 => P::Decimal,
            18 => P::String,
            19 => P::Bytes,
            20 => P::Date,
            21 => P::LocalTime,
            22 => P::ZonedTime,
            23 => P::LocalDateTime,
            24 => P::ZonedDateTime,
            25 => P::Duration,
            26 => P::DurationYearToMonth,
            27 => P::DurationDayToSecond,
            28 => P::Uuid,
            29 => P::Vector,
            30 => P::Json,
            _ => return Err(E::Invalid("predefined type")),
        })
    }
}
