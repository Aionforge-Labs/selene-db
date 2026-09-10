//! In-memory GQL value representation per spec 02 section 3.
//!
//! [`Value`] is a runtime carrier. Its isolated legacy serde adapter preserves
//! established enum tags until deletion by F02-PR08; that compatibility adapter
//! is not the format-2 stored-value encoding. New WAL framing belongs to F02-PR03.
//! Durable admission uses [`crate::StoredValue`], which excludes query references
//! and process-local identities recursively.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use smallvec::SmallVec;

use crate::db_string::DbString;
use crate::error::{CoreError, CoreResult};
use crate::extension_type_ids::ExtensionTypeId;
use crate::identity::{BindingTableId, EdgeId, GraphId, NodeId, RecordTypeId};
use crate::json_value::JsonValue;

/// Maximum component count for a native dense vector value.
///
/// The cap keeps dimensions representable in a compact `u16` slot for future
/// vector-index metadata while allowing common embedding sizes.
pub const MAX_VECTOR_DIMENSION: usize = u16::MAX as usize;

/// In-memory representation of a GQL value.
///
/// ID037: `Float` uses IEEE 754 binary64 storage, while `Float32` remains
/// distinct with binary32 storage. Rust equality preserves GQL's
/// `+0.0 == -0.0` behavior; IA025 NaN predicate and ordering behavior is
/// handled by the query engine outside this crate.
///
/// Value equality matches IEEE 754 for non-NaN AND treats all NaN bit-patterns
/// as equal for round-trip integrity. This is the internal Rust-level equality
/// used by `PropertyMap` serde round-trip and snapshot diffs. The GQL `=`
/// operator is intercepted at the runtime layer (`runtime::value_compare`)
/// and preserves ISO 3VL semantics — `NaN = NaN` returns NULL there.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Value {
    /// Boolean value.
    Bool(bool),
    /// Signed integer up to 64 bits.
    Int(i64),
    /// Unsigned integer up to 64 bits.
    Uint(u64),
    /// Signed 128-bit integer.
    Int128(i128),
    /// Unsigned 128-bit integer.
    Uint128(u128),
    /// Default floating-point value.
    Float(f64),
    /// Distinct 32-bit floating-point value.
    Float32(f32),
    /// Fixed-precision decimal value.
    Decimal(rust_decimal::Decimal),
    /// String value.
    String(DbString),
    /// Byte-string value.
    Bytes(Arc<[u8]>),
    /// List value.
    List(Vec<Value>),
    /// Open record value.
    Record(Box<Record>),
    /// Closed record value tied to a graph-type-defined record type.
    RecordTyped(Box<RecordTyped>),
    /// Path value.
    ///
    /// Boxed (CORE-06): an inline `Path` is 120 B and was the `size_of::<Value>`
    /// ceiling. Paths are ephemeral traversal results, rarely stored, so moving
    /// one behind a pointer shrinks every `Value` clone/move to a pointer copy.
    Path(Box<Path>),
    /// Node reference value.
    NodeRef(NodeId),
    /// Edge reference value.
    EdgeRef(EdgeId),
    /// Graph reference value.
    GraphRef(GraphId),
    /// Binding-table reference value.
    TableRef(BindingTableId),
    /// Zoned datetime value.
    ///
    /// Boxed (CORE-06): `jiff::Zoned` is 40 B; temporal values are uncommon in
    /// hot graph data, so the indirection is paid only on temporal access.
    ZonedDateTime(Box<jiff::Zoned>),
    /// Local datetime value.
    LocalDateTime(jiff::civil::DateTime),
    /// Date value.
    Date(jiff::civil::Date),
    /// Zoned time value.
    ///
    /// `jiff` 0.2 has no dedicated zoned-time type, so selene-core uses
    /// `jiff::Zoned`; date components are ignored at the GQL boundary.
    ///
    /// Boxed (CORE-06): see [`Value::ZonedDateTime`].
    ZonedTime(Box<jiff::Zoned>),
    /// Local time value.
    LocalTime(jiff::civil::Time),
    /// Duration value.
    ///
    /// Boxed (CORE-06): `jiff::Span` is 64 B (the largest time variant); boxing
    /// it keeps the post-`Path` ceiling off `Value`.
    Duration(Box<jiff::Span>),
    /// Extension-owned opaque payload.
    Extended {
        /// Registered extension type ID.
        type_id: ExtensionTypeId,
        /// Extension-owned byte payload.
        payload: Arc<[u8]>,
    },
    /// Null value.
    Null,
    /// UUID value.
    Uuid(uuid::Uuid),
    /// Native dense vector value.
    Vector(VectorValue),
    /// Native JSON value.
    Json(JsonValue),
}

/// Compile-time ceiling on `size_of::<Value>` — a zero-cost re-bloat tripwire.
///
/// `Value` is moved and cloned on every property read, `PropertyMap` copy, and
/// binding-table row, so its in-memory size is a hot-path cost multiplier (see
/// the `value_clone` bench). CORE-06 boxed the four oversized variants —
/// `Path` (120 B, which a `size_of` profile proved was the real former ceiling,
/// **not** the time variants the design doc assumed), `Duration(jiff::Span)`
/// (64 B), and the two `jiff::Zoned` variants (40 B) — bringing `Value` from
/// 128 B down to 32 B. The current ceiling is set by 24-byte payload families
/// such as `List(Vec)`, plus the discriminant and 16-byte alignment from the
/// `i128`/`Decimal` variants. This assert fails the build if a future variant
/// regrows the enum; box the offending payload or lift the ceiling
/// deliberately.
const _: () = assert!(core::mem::size_of::<Value>() <= 32);

#[path = "value/legacy_serde.rs"]
mod legacy_serde;

impl Value {
    /// Factory table with one sample value for each [`Value`] variant.
    ///
    /// The table is used by tests as an append-only ANCHOR: adding a new
    /// variant requires adding one factory here so the source-of-truth crate
    /// owns the variant census.
    pub const ALL: &[fn() -> Self] = &[
        || Self::Bool(false),
        || Self::Int(0),
        || Self::Uint(0),
        || Self::Int128(0),
        || Self::Uint128(0),
        || Self::Float(0.0),
        || Self::Float32(0.0),
        || Self::Decimal(rust_decimal::Decimal::ZERO),
        || Self::String(value_variant_string("value.all.string")),
        || Self::Bytes(Arc::from([0_u8])),
        || Self::List(Vec::new()),
        || Self::Record(Box::new(Record::Open(SmallVec::new()))),
        || {
            Self::RecordTyped(Box::new(RecordTyped {
                type_id: RecordTypeId::new(1),
                values: SmallVec::new(),
            }))
        },
        || {
            Self::Path(Box::new(Path {
                graph: GraphId::new(1),
                start: NodeId::new(1),
                segments: SmallVec::new(),
            }))
        },
        || Self::NodeRef(NodeId::new(1)),
        || Self::EdgeRef(EdgeId::new(1)),
        || Self::GraphRef(GraphId::new(1)),
        || Self::TableRef(BindingTableId::new(1)),
        || Self::ZonedDateTime(Box::new(value_variant_zoned())),
        || Self::LocalDateTime("2024-01-01T00:00:00".parse().unwrap()),
        || Self::Date("2024-01-01".parse().unwrap()),
        || Self::ZonedTime(Box::new(value_variant_zoned())),
        || Self::LocalTime("00:00:00".parse().unwrap()),
        || Self::Duration(Box::new("PT1S".parse().unwrap())),
        || Self::Extended {
            type_id: ExtensionTypeId::FIRST_PARTY_MIN,
            payload: Arc::from([0_u8]),
        },
        || Self::Null,
        || Self::Uuid(uuid::Uuid::nil()),
        || Self::Vector(VectorValue::new(vec![0.0]).expect("fixture vector is valid")),
        || Self::Json(JsonValue::new(serde_json::json!({"fixture": true})).unwrap()),
    ];

    /// Number of known [`Value`] variants in this build.
    pub const VARIANT_COUNT: usize = Self::ALL.len();

    /// Whether this value is a *number* in the ISO sense.
    ///
    /// ISO/IEC 39075:2024 §4.16.5.2 states that "any two numbers are
    /// essentially comparable values", which makes the numeric types the one
    /// family whose values compare across distinct variants. Every other family
    /// is narrower: temporal instants are comparable only at "the same most
    /// specific static value types" (§4.16.6.2), temporal durations only within
    /// one unit group (§4.16.6.3), and otherwise only identical values are
    /// essentially comparable (§4.4.2 NOTE 25). Absent Feature GA04, "Universal
    /// comparison" — which the generated runtime inventory does not report as
    /// supported — §4.4.2 NOTE 26 leaves no further comparability, so a
    /// cross-variant pair outside this family can never compare equal.
    ///
    /// That is why this predicate is engine-wide rather than local to one
    /// consumer: query evaluation and index-drift classification have to agree
    /// on it exactly, and a second copy could only ever drift from the first.
    ///
    /// The match is written out rather than as a `matches!` of the numeric arms
    /// because `#[non_exhaustive]` does not apply inside `selene-core`: a new
    /// variant is a compile error here and has to be classified deliberately.
    /// A catch-all would answer `false` for a new numeric variant, and a caller
    /// that skips a row on the strength of that answer would silently omit it
    /// from results it belongs in.
    #[must_use]
    pub const fn is_number(&self) -> bool {
        match self {
            Self::Int(_)
            | Self::Uint(_)
            | Self::Int128(_)
            | Self::Uint128(_)
            | Self::Float(_)
            | Self::Float32(_)
            | Self::Decimal(_) => true,
            Self::Bool(_)
            | Self::String(_)
            | Self::Bytes(_)
            | Self::List(_)
            | Self::Record(_)
            | Self::RecordTyped(_)
            | Self::Path(_)
            | Self::NodeRef(_)
            | Self::EdgeRef(_)
            | Self::GraphRef(_)
            | Self::TableRef(_)
            | Self::ZonedDateTime(_)
            | Self::LocalDateTime(_)
            | Self::Date(_)
            | Self::ZonedTime(_)
            | Self::LocalTime(_)
            | Self::Duration(_)
            | Self::Extended { .. }
            | Self::Null
            | Self::Uuid(_)
            | Self::Vector(_)
            | Self::Json(_) => false,
        }
    }

    /// Stable telemetry name for this value variant.
    ///
    /// This match is exhaustive in `selene-core`, so a future variant addition
    /// forces the defining crate to choose the new public name once.
    #[must_use]
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "Bool",
            Self::Int(_) => "Int",
            Self::Uint(_) => "Uint",
            Self::Int128(_) => "Int128",
            Self::Uint128(_) => "Uint128",
            Self::Float(_) => "Float",
            Self::Float32(_) => "Float32",
            Self::Decimal(_) => "Decimal",
            Self::String(_) => "String",
            Self::Bytes(_) => "Bytes",
            Self::List(_) => "List",
            Self::Record(_) => "Record",
            Self::RecordTyped(_) => "RecordTyped",
            Self::Path(_) => "Path",
            Self::NodeRef(_) => "NodeRef",
            Self::EdgeRef(_) => "EdgeRef",
            Self::GraphRef(_) => "GraphRef",
            Self::TableRef(_) => "TableRef",
            Self::ZonedDateTime(_) => "ZonedDateTime",
            Self::LocalDateTime(_) => "LocalDateTime",
            Self::Date(_) => "Date",
            Self::ZonedTime(_) => "ZonedTime",
            Self::LocalTime(_) => "LocalTime",
            Self::Duration(_) => "Duration",
            Self::Extended { .. } => "Extended",
            Self::Null => "Null",
            Self::Uuid(_) => "Uuid",
            Self::Vector(_) => "Vector",
            Self::Json(_) => "Json",
        }
    }
}

fn value_variant_string(name: &str) -> DbString {
    crate::db_string(name).expect("Value::ALL fixture strings fit DB string cap")
}

fn value_variant_zoned() -> jiff::Zoned {
    jiff::Timestamp::new(0, 0)
        .expect("Value::ALL timestamp fixture is in range")
        .to_zoned(jiff::tz::TimeZone::UTC)
}

impl PartialEq for Value {
    fn eq(&self, rhs: &Self) -> bool {
        match (self, rhs) {
            (Self::Bool(lhs), Self::Bool(rhs)) => lhs == rhs,
            (Self::Int(lhs), Self::Int(rhs)) => lhs == rhs,
            (Self::Uint(lhs), Self::Uint(rhs)) => lhs == rhs,
            (Self::Int128(lhs), Self::Int128(rhs)) => lhs == rhs,
            (Self::Uint128(lhs), Self::Uint128(rhs)) => lhs == rhs,
            (Self::Float(lhs), Self::Float(rhs)) => lhs == rhs || (lhs.is_nan() && rhs.is_nan()),
            (Self::Float32(lhs), Self::Float32(rhs)) => {
                lhs == rhs || (lhs.is_nan() && rhs.is_nan())
            }
            (Self::Decimal(lhs), Self::Decimal(rhs)) => lhs == rhs,
            (Self::String(lhs), Self::String(rhs)) => lhs == rhs,
            (Self::Bytes(lhs), Self::Bytes(rhs)) => lhs == rhs,
            (Self::List(lhs), Self::List(rhs)) => lhs == rhs,
            (Self::Record(lhs), Self::Record(rhs)) => lhs == rhs,
            (Self::RecordTyped(lhs), Self::RecordTyped(rhs)) => lhs == rhs,
            (Self::Path(lhs), Self::Path(rhs)) => lhs == rhs,
            (Self::NodeRef(lhs), Self::NodeRef(rhs)) => lhs == rhs,
            (Self::EdgeRef(lhs), Self::EdgeRef(rhs)) => lhs == rhs,
            (Self::GraphRef(lhs), Self::GraphRef(rhs)) => lhs == rhs,
            (Self::TableRef(lhs), Self::TableRef(rhs)) => lhs == rhs,
            (Self::ZonedDateTime(lhs), Self::ZonedDateTime(rhs)) => lhs == rhs,
            (Self::LocalDateTime(lhs), Self::LocalDateTime(rhs)) => lhs == rhs,
            (Self::Date(lhs), Self::Date(rhs)) => lhs == rhs,
            (Self::ZonedTime(lhs), Self::ZonedTime(rhs)) => lhs == rhs,
            (Self::LocalTime(lhs), Self::LocalTime(rhs)) => lhs == rhs,
            (Self::Duration(lhs), Self::Duration(rhs)) => lhs.fieldwise() == rhs.fieldwise(),
            (
                Self::Extended {
                    type_id: lhs_type_id,
                    payload: lhs_payload,
                },
                Self::Extended {
                    type_id: rhs_type_id,
                    payload: rhs_payload,
                },
            ) => lhs_type_id == rhs_type_id && lhs_payload == rhs_payload,
            (Self::Null, Self::Null) => true,
            (Self::Uuid(lhs), Self::Uuid(rhs)) => lhs == rhs,
            (Self::Vector(lhs), Self::Vector(rhs)) => lhs == rhs,
            (Self::Json(lhs), Self::Json(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}

/// Native dense vector payload stored as a first-class [`Value`].
///
/// `VectorValue` validates once at construction and deserialization so every
/// engine layer can assume a non-empty finite `f32` slice. NaN and infinity are
/// rejected because they make distance metrics, ordering fallbacks, hashing,
/// and approximate-nearest-neighbor indexes ambiguous.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct VectorValue {
    components: Arc<[f32]>,
}

impl VectorValue {
    /// Build a validated vector from owned components.
    pub fn new(components: impl Into<Vec<f32>>) -> CoreResult<Self> {
        let components = components.into();
        if components.is_empty() {
            return Err(CoreError::VectorEmpty);
        }
        if components.len() > MAX_VECTOR_DIMENSION {
            return Err(CoreError::VectorTooLarge {
                got: components.len(),
                max: MAX_VECTOR_DIMENSION,
            });
        }
        for (index, value) in components.iter().copied().enumerate() {
            if !value.is_finite() {
                return Err(CoreError::VectorComponentNotFinite { index, value });
            }
        }
        Ok(Self {
            components: Arc::from(components),
        })
    }

    /// Number of `f32` components in this vector.
    #[must_use]
    pub fn dimension(&self) -> usize {
        self.components.len()
    }

    /// Borrow the vector components.
    #[must_use]
    pub fn as_slice(&self) -> &[f32] {
        &self.components
    }

    /// Clone the shared component storage.
    #[must_use]
    pub fn as_arc(&self) -> Arc<[f32]> {
        Arc::clone(&self.components)
    }
}

impl TryFrom<Vec<f32>> for VectorValue {
    type Error = CoreError;

    fn try_from(value: Vec<f32>) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<VectorValue> for Vec<f32> {
    fn from(value: VectorValue) -> Self {
        value.components.as_ref().to_vec()
    }
}

impl<'de> Deserialize<'de> for VectorValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<f32>::deserialize(deserializer)
            .and_then(|components| Self::new(components).map_err(serde::de::Error::custom))
    }
}

/// Open record value.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub enum Record {
    /// Open `RECORD` literal in expressions.
    Open(SmallVec<[(DbString, Value); 4]>),
}

/// Closed record value tied to a graph-type-defined record type.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RecordTyped {
    /// Identifier pointing to a `RecordTypeDef` in the graph type catalog.
    pub type_id: RecordTypeId,
    /// Positional values aligned with the record type's field list.
    pub values: SmallVec<[Option<Value>; 4]>,
}

/// Path value.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Path {
    /// Graph the path lives within.
    pub graph: GraphId,
    /// Starting node of the path.
    pub start: NodeId,
    /// Ordered segments traversed.
    pub segments: SmallVec<[PathSegment; 4]>,
}

/// One traversal step in a [`Path`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PathSegment {
    /// Edge traversed in this step.
    pub edge: EdgeId,
    /// Direction of edge traversal.
    pub direction: EdgeDirection,
    /// Node reached after this step.
    pub node: NodeId,
}

/// Direction of edge traversal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum EdgeDirection {
    /// Source-to-target traversal of a directed edge.
    Outgoing,
    /// Target-to-source traversal of a directed edge.
    Incoming,
    /// Undirected edge.
    Undirected,
}

mod serde_i128_le {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S>(value: &i128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.to_le_bytes().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<i128, D::Error>
    where
        D: Deserializer<'de>,
    {
        <[u8; 16]>::deserialize(deserializer).map(i128::from_le_bytes)
    }
}

mod serde_u128_le {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.to_le_bytes().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        <[u8; 16]>::deserialize(deserializer).map(u128::from_le_bytes)
    }
}

mod serde_decimal_str {
    use std::str::FromStr;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(
        value: &rust_decimal::Decimal,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<rust_decimal::Decimal, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        rust_decimal::Decimal::from_str(&value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests;
