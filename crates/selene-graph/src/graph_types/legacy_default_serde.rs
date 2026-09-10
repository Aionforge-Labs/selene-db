//! Bounded legacy serde defaults; enum tags/field payloads are unchanged.

use super::{PropertyDefaultRecordField, PropertyDefaultValue};
use selene_core::DbString;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cell::Cell;

thread_local! { static DEPTH: Cell<usize> = const { Cell::new(0) }; }
struct Guard;
impl Guard {
    fn enter() -> Result<Self, &'static str> {
        DEPTH.with(|depth| {
            if depth.get() >= selene_core::MAX_STORED_VALUE_DEPTH {
                return Err("property default nesting limit exceeded");
            }
            depth.set(depth.get() + 1);
            Ok(Self)
        })
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        DEPTH.with(|depth| {
            let current_depth = depth.get();
            depth.set(current_depth - 1);
        });
    }
}

#[derive(Deserialize, Serialize)]
#[serde(remote = "PropertyDefaultValue")]
enum LegacyDefault {
    Null,
    Boolean(bool),
    Integer(i64),
    String(DbString),
    Bytes(Vec<u8>),
    // Mirrors the legacy remote enum exactly; this is not a new layout.
    #[allow(clippy::vec_box)]
    List(Vec<Box<PropertyDefaultValue>>),
    Record(Vec<PropertyDefaultRecordField>),
    Uuid(DbString),
    Json(DbString),
    Float(u64),
    Float32(u32),
    ZonedDateTime(DbString),
    LocalDateTime(DbString),
    Date(DbString),
    ZonedTime(DbString),
    LocalTime(DbString),
    Duration(DbString),
    Uint(u64),
    Int128(i128),
    Uint128(u128),
    Decimal(DbString),
    Vector(Vec<u32>),
}

impl<'de> Deserialize<'de> for PropertyDefaultValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let _guard = Guard::enter().map_err(serde::de::Error::custom)?;
        LegacyDefault::deserialize(deserializer)
    }
}
impl Serialize for PropertyDefaultValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let _guard = Guard::enter().map_err(serde::ser::Error::custom)?;
        LegacyDefault::serialize(self, serializer)
    }
}
