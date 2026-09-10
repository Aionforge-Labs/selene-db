//! Bounded adapter for the existing serde value encoding (deletion F02-PR08).
//! The remote derive preserves enum tags and scalar representations. It is not
//! the StoredValue codec and must not be used to define format 2.

use super::{Path, Record, RecordTyped, Value, VectorValue};
use crate::{BindingTableId, DbString, EdgeId, ExtensionTypeId, GraphId, JsonValue, NodeId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{cell::Cell, sync::Arc};

thread_local! {
    // A synchronous serde call chain owns this counter only until its guards
    // unwind. No values, graph identities, descriptors, or arenas are retained.
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct DepthGuard;

impl DepthGuard {
    fn enter() -> Result<Self, &'static str> {
        DEPTH.with(|depth| {
            if depth.get() >= crate::MAX_STORED_VALUE_DEPTH {
                return Err("legacy value nesting limit exceeded");
            }
            depth.set(depth.get() + 1);
            Ok(Self)
        })
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

// Remote derive delegates every scalar/collection primitive to serde. The
// declaration order intentionally matches the established legacy Value tags.
#[derive(Deserialize, Serialize)]
#[serde(remote = "Value")]
enum LegacyValue {
    Bool(bool),
    Int(i64),
    Uint(u64),
    Int128(#[serde(with = "super::serde_i128_le")] i128),
    Uint128(#[serde(with = "super::serde_u128_le")] u128),
    Float(f64),
    Float32(f32),
    Decimal(#[serde(with = "super::serde_decimal_str")] rust_decimal::Decimal),
    String(DbString),
    Bytes(Arc<[u8]>),
    List(Vec<Value>),
    Record(Box<Record>),
    RecordTyped(Box<RecordTyped>),
    Path(Box<Path>),
    NodeRef(NodeId),
    EdgeRef(EdgeId),
    GraphRef(GraphId),
    TableRef(BindingTableId),
    ZonedDateTime(Box<jiff::Zoned>),
    LocalDateTime(jiff::civil::DateTime),
    Date(jiff::civil::Date),
    ZonedTime(Box<jiff::Zoned>),
    LocalTime(jiff::civil::Time),
    Duration(Box<jiff::Span>),
    Extended {
        type_id: ExtensionTypeId,
        payload: Arc<[u8]>,
    },
    Null,
    Uuid(uuid::Uuid),
    Vector(VectorValue),
    Json(JsonValue),
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let _depth = DepthGuard::enter().map_err(serde::de::Error::custom)?;
        LegacyValue::deserialize(deserializer)
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let _depth = DepthGuard::enter().map_err(serde::ser::Error::custom)?;
        LegacyValue::serialize(self, serializer)
    }
}
