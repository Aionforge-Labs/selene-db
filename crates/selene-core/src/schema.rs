//! Schema model types per spec 02 section 6.
//!
//! These are structural data carriers. Runtime validation of graph mutations
//! against a [`GraphType`] belongs to `selene-graph`.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use smallvec::SmallVec;
mod stored;

use crate::{
    ByteStringType, CharacterStringType, CoreError, CoreResult, DbString, DecimalType,
    ExtensionTypeId, LabelSet, PropertyValueType, RecordTypeId, Value,
};

/// Graph-type-scoped schema identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(transparent)]
pub struct GraphTypeId(pub u64);

impl<'de> Deserialize<'de> for GraphTypeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = u64::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

impl GraphTypeId {
    /// Construct a graph type ID, rejecting the reserved zero sentinel.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ZeroIdentifier`] when `value` is `0`.
    pub const fn new(value: u64) -> CoreResult<Self> {
        if value == 0 {
            Err(CoreError::ZeroIdentifier)
        } else {
            Ok(Self(value))
        }
    }

    /// Return the raw `u64` value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for GraphTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GraphTypeId({})", self.0)
    }
}

/// Closed-graph schema definition.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphType {
    /// Stable graph type ID.
    pub id: GraphTypeId,
    /// Database-string graph type name.
    pub name: DbString,
    /// Node types keyed by node label.
    pub node_types: BTreeMap<DbString, NodeTypeDef>,
    /// Edge types keyed by edge label.
    pub edge_types: BTreeMap<DbString, EdgeTypeDef>,
    /// Record types keyed by record type ID.
    pub record_types: BTreeMap<RecordTypeId, RecordTypeDef>,
    /// Reserved policy for relationships between key label sets. **Not yet
    /// consulted:** closed-graph element binding uses exact key-label-set
    /// equality, and every type's key label set is currently a singleton
    /// (cardinality 1), so no overlap/containment relationship can arise to
    /// apply a policy to. See [`KeyLabelSetPolicy`].
    pub key_label_set_policy: KeyLabelSetPolicy,
}

impl GraphType {
    /// Construct an empty graph type.
    #[must_use]
    pub fn new(id: GraphTypeId, name: DbString) -> Self {
        Self {
            id,
            name,
            node_types: BTreeMap::new(),
            edge_types: BTreeMap::new(),
            record_types: BTreeMap::new(),
            key_label_set_policy: KeyLabelSetPolicy::default(),
        }
    }
}

/// Node type definition.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeTypeDef {
    /// Label set required by this node type.
    pub labels: LabelSet,
    /// Property definitions in schema order.
    pub properties: SmallVec<[PropertyDef; 8]>,
    /// Optional property-name key.
    pub key: Option<NodeKey>,
    /// Closed-graph validation mode for this node type.
    pub validation_mode: ValidationMode,
}

impl NodeTypeDef {
    /// Construct a node type definition with no properties.
    #[must_use]
    pub fn new(labels: LabelSet) -> Self {
        Self {
            labels,
            properties: SmallVec::new(),
            key: None,
            validation_mode: ValidationMode::Strict,
        }
    }
}

/// Legacy WAL node type definition carried by [`SchemaChange::NodeTypeAdded`](crate::SchemaChange::NodeTypeAdded).
///
/// This freezes the pre-v1.1 catalog-DDL payload shape. New WAL entries use
/// [`SchemaChange::NodeTypeAddedV2`](crate::SchemaChange::NodeTypeAddedV2)
/// with [`NodeTypeDef`]; recovery upgrades this shape with
/// [`ValidationMode::Strict`] plus non-immutable, non-unique properties.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeTypeDefV1 {
    /// Label set required by this node type.
    pub labels: LabelSet,
    /// Property definitions in schema order.
    pub properties: SmallVec<[PropertyDefV1; 8]>,
    /// Optional property-name key.
    pub key: Option<NodeKey>,
}

impl NodeTypeDefV1 {
    /// Construct a legacy node type definition with no properties.
    #[must_use]
    pub fn new(labels: LabelSet) -> Self {
        Self {
            labels,
            properties: SmallVec::new(),
            key: None,
        }
    }
}

impl From<NodeTypeDefV1> for NodeTypeDef {
    fn from(value: NodeTypeDefV1) -> Self {
        Self {
            labels: value.labels,
            properties: value.properties.into_iter().map(Into::into).collect(),
            key: value.key,
            validation_mode: ValidationMode::Strict,
        }
    }
}

/// Property-name list that forms a node key.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct NodeKey {
    /// Property names participating in the key.
    pub property_names: SmallVec<[DbString; 2]>,
}

/// Edge endpoint definition.
///
/// `OneOf` carries a sorted, deduplicated, length-≥-2 set of distinct
/// [`NodeTypeRef`]s. Construct it via [`EdgeEndpointDef::one_of`] so the
/// invariants are enforced (singleton inputs collapse to
/// [`EdgeEndpointDef::NodeType`]). The WAL is permissive — recovery re-applies
/// the constructor through the storage-side resolver, so direct struct
/// construction in WAL paths is acceptable and replay canonicalizes.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum EdgeEndpointDef {
    /// Accept any declared node type at this endpoint.
    Any,
    /// Reference one concrete node type.
    NodeType(NodeTypeRef),
    /// Reference any node type drawn from a sorted, deduplicated, length-≥-2
    /// set of distinct node types.
    OneOf(SmallVec<[NodeTypeRef; 4]>),
}

impl EdgeEndpointDef {
    /// Construct an endpoint accepting `refs`, canonicalized.
    ///
    /// References are sorted by database-string identity and deduplicated. A
    /// single resulting reference collapses to [`EdgeEndpointDef::NodeType`].
    ///
    /// # Panics
    ///
    /// Panics when the resulting set is empty; zero-label endpoints are a
    /// caller bug and the upstream resolver must reject them before reaching
    /// this constructor.
    #[must_use]
    pub fn one_of(refs: impl IntoIterator<Item = NodeTypeRef>) -> Self {
        let mut buf: SmallVec<[NodeTypeRef; 4]> = refs.into_iter().collect();
        buf.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        buf.dedup();
        assert!(
            !buf.is_empty(),
            "EdgeEndpointDef::one_of called with empty NodeTypeRef set"
        );
        match buf.len() {
            1 => Self::NodeType(buf[0].clone()),
            _ => Self::OneOf(buf),
        }
    }
}

/// Edge type definition.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeTypeDef {
    /// Single edge label.
    pub label: DbString,
    /// Source endpoint definition.
    pub source_node_type: EdgeEndpointDef,
    /// Target endpoint definition.
    pub target_node_type: EdgeEndpointDef,
    /// Property definitions in schema order.
    pub properties: SmallVec<[PropertyDef; 4]>,
    /// Closed-graph validation mode for this edge type.
    pub validation_mode: ValidationMode,
}

impl EdgeTypeDef {
    /// Construct an edge type definition with no properties.
    #[must_use]
    pub fn new(label: DbString, source: NodeTypeRef, target: NodeTypeRef) -> Self {
        Self::new_with_endpoints(
            label,
            EdgeEndpointDef::NodeType(source),
            EdgeEndpointDef::NodeType(target),
        )
    }

    /// Construct an edge type definition with explicit endpoints and no properties.
    #[must_use]
    pub fn new_with_endpoints(
        label: DbString,
        source: EdgeEndpointDef,
        target: EdgeEndpointDef,
    ) -> Self {
        Self {
            label,
            source_node_type: source,
            target_node_type: target,
            properties: SmallVec::new(),
            validation_mode: ValidationMode::Strict,
        }
    }
}

/// Legacy WAL edge type definition carried by [`SchemaChange::EdgeTypeAdded`](crate::SchemaChange::EdgeTypeAdded).
///
/// This freezes the pre-v1.1 catalog-DDL payload shape. New WAL entries use
/// [`SchemaChange::EdgeTypeAddedV2`](crate::SchemaChange::EdgeTypeAddedV2)
/// with [`EdgeTypeDef`]; recovery upgrades this shape with
/// [`ValidationMode::Strict`] plus non-immutable, non-unique properties.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeTypeDefV1 {
    /// Single edge label.
    pub label: DbString,
    /// Source node type reference.
    pub source_node_type: NodeTypeRef,
    /// Target node type reference.
    pub target_node_type: NodeTypeRef,
    /// Property definitions in schema order.
    pub properties: SmallVec<[PropertyDefV1; 4]>,
}

impl EdgeTypeDefV1 {
    /// Construct a legacy edge type definition with no properties.
    #[must_use]
    pub fn new(label: DbString, source: NodeTypeRef, target: NodeTypeRef) -> Self {
        Self {
            label,
            source_node_type: source,
            target_node_type: target,
            properties: SmallVec::new(),
        }
    }
}

impl From<EdgeTypeDefV1> for EdgeTypeDef {
    fn from(value: EdgeTypeDefV1) -> Self {
        Self {
            label: value.label,
            source_node_type: EdgeEndpointDef::NodeType(value.source_node_type),
            target_node_type: EdgeEndpointDef::NodeType(value.target_node_type),
            properties: value.properties.into_iter().map(Into::into).collect(),
            validation_mode: ValidationMode::Strict,
        }
    }
}

/// Closed-graph validation mode.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ValidationMode {
    /// Reject type-model violations.
    #[default]
    Strict,
    /// Allow relaxed property-shape writes and report warnings.
    Warn,
}

/// Node type reference by label.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[repr(transparent)]
pub struct NodeTypeRef(pub DbString);

/// Record type reference by ID.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[repr(transparent)]
pub struct RecordTypeRef(pub RecordTypeId);

/// Inline, recursively-nestable closed/typed RECORD field-type structure carried on
/// the WAL change stream (postcard). This is the serde/WAL counterpart of the rkyv
/// snapshot-side `selene_graph::graph_types::RecordFieldTypes`; the two carry the same
/// structure and must round-trip into each other.
///
/// Record structure is inlined on [`PropertyDef::record_fields`] rather than on
/// [`ValueType::record`] (a [`RecordTypeRef`] by-ID that cannot hold inline structure),
/// symmetric to how `LIST` inlines its element type.
// Why: Per ISO 39075:2024 §18.9 <record type> / <field types specification> and §18.10
// <field type>; features GV46 (closed record types) / GV47 (open record types) / GV48
// (nested record types).
//
// Three durable record states are encoded jointly with the `Option` on
// [`PropertyDef::record_fields`]: absent (`None`) ⇒ the property is not a record;
// `Some(Open)` ⇒ an open/bare `RECORD` (no declared fields, any record value conforms);
// `Some(Closed(..))` ⇒ a closed/typed `RECORD{..}`. The open variant is what makes a bare
// `RECORD` property survive WAL replay as `RecordTyped` rather than degrading to `Null`
// (it carries no field list, so the absent/open distinction cannot ride the inner `Vec`).
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum RecordFieldStructure {
    /// Open/bare `RECORD` — no declared field types; any record value conforms (GV47).
    Open,
    /// Closed/typed `RECORD{..}` — the declared field-type list (GV46/GV48).
    Closed(Vec<RecordFieldStructureDef>),
}

/// One declared field of a closed/typed RECORD: name, its (possibly nested) type, and
/// whether the field is required (non-nullable).
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct RecordFieldStructureDef {
    /// Field name.
    pub name: DbString,
    /// Declared field type (recursively nestable).
    pub field_type: RecordFieldStructureType,
    /// `true` when the field is required (NOT NULL).
    pub required: bool,
}

/// Recursively-nestable field-type for a closed/typed RECORD declaration (serde/WAL side).
///
/// Deliberately **not** `#[non_exhaustive]`: it is matched cross-crate by the
/// selene-graph rkyv⇄serde conversions, where exhaustive matching is wanted so a future
/// variant forces both conversion directions to be updated.
// Why: Per ISO 39075:2024 §18.10 CR1 (GV48 nested record types) — a field type may itself
// contain a list or record type.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum RecordFieldStructureType {
    /// Scalar field type.
    Scalar(PropertyValueType),
    /// STRING field type with a user-specified length envelope.
    CharacterString(CharacterStringType),
    /// DECIMAL field type with a user-specified precision/scale envelope.
    Decimal(DecimalType),
    /// BYTES field type with a user-specified length envelope.
    ByteString(ByteStringType),
    /// LIST field type.
    List(Box<RecordFieldStructureType>),
    /// Nested RECORD field type.
    Record(Box<RecordFieldStructure>),
    /// Explicitly non-null field or nested element type.
    NotNull(Box<RecordFieldStructureType>),
}

/// Property schema definition.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyDef {
    /// Property name.
    pub name: DbString,
    /// Property value type.
    pub value_type: ValueType,
    /// Whether `Value::Null` is allowed.
    pub nullable: bool,
    /// Optional default value.
    pub default: Option<Value>,
    /// Whether updates to this property are forbidden after creation.
    pub immutable: bool,
    /// Whether non-null property values must be unique within the declaring type.
    pub unique: bool,
    /// Inline RECORD field structure when [`PropertyDef::value_type`] resolves to a
    /// `RecordTyped` property. `None` for every non-record property; `Some(Open)` for an
    /// open/bare `RECORD`; `Some(Closed(..))` for a closed/typed `RECORD{..}`. The
    /// `None`-vs-`Some(Open)` distinction is load-bearing: it is the only durable signal
    /// that a bare `RECORD` property is record-typed (its `ValueType` is otherwise
    /// indistinguishable from a scalar `Null` on the WAL side), so without it WAL replay
    /// would degrade an open record to `Null`. Carried for WAL durability, symmetric to
    /// the rkyv snapshot-side
    /// `selene_graph::graph_types::PropertyTypeDef::record_field_types`.
    pub record_fields: Option<Box<RecordFieldStructure>>,
}

/// Legacy WAL property definition carried by v1 catalog-DDL schema changes.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyDefV1 {
    /// Property name.
    pub name: DbString,
    /// Property value type.
    pub value_type: ValueType,
    /// Whether `Value::Null` is allowed.
    pub nullable: bool,
    /// Optional default value.
    pub default: Option<Value>,
}

impl From<PropertyDefV1> for PropertyDef {
    fn from(value: PropertyDefV1) -> Self {
        Self {
            name: value.name,
            value_type: value.value_type,
            nullable: value.nullable,
            default: value.default,
            immutable: false,
            unique: false,
            record_fields: None,
        }
    }
}

/// Structural value type definition.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ValueType {
    /// Scalar predefined type.
    pub predefined: Option<PredefinedValueType>,
    /// User-specified decimal precision/scale descriptor.
    ///
    /// Only meaningful when [`Self::predefined`] is
    /// [`PredefinedValueType::Decimal`].
    #[serde(default)]
    pub decimal_type: Option<DecimalType>,
    /// User-specified character-string length descriptor.
    ///
    /// Only meaningful when [`Self::predefined`] is
    /// [`PredefinedValueType::String`].
    #[serde(default)]
    pub character_string_type: Option<CharacterStringType>,
    /// User-specified byte-string length descriptor.
    ///
    /// Only meaningful when [`Self::predefined`] is [`PredefinedValueType::Bytes`].
    #[serde(default)]
    pub byte_string_type: Option<ByteStringType>,
    /// Union member types.
    pub union: Option<Vec<ValueType>>,
    /// List element type. When present, this takes precedence over scalar
    /// fields for callers interpreting the type.
    pub list_of: Option<Box<ValueType>>,
    /// Closed record type reference.
    pub record: Option<RecordTypeRef>,
    /// Whether null is forbidden at this level.
    pub not_null: bool,
    /// Currently supported scalar cardinality.
    pub cardinality: ValueTypeCardinality,
}

impl ValueType {
    /// Construct a predefined scalar value type.
    #[must_use]
    pub const fn predefined(predefined: PredefinedValueType) -> Self {
        Self {
            predefined: Some(predefined),
            decimal_type: None,
            character_string_type: None,
            byte_string_type: None,
            union: None,
            list_of: None,
            record: None,
            not_null: false,
            cardinality: ValueTypeCardinality::ExactlyOne,
        }
    }

    /// Construct a list value type.
    #[must_use]
    pub fn list_of(item: Self) -> Self {
        Self {
            predefined: None,
            decimal_type: None,
            character_string_type: None,
            byte_string_type: None,
            union: None,
            list_of: Some(Box::new(item)),
            record: None,
            not_null: false,
            cardinality: ValueTypeCardinality::ExactlyOne,
        }
    }
}

/// Predefined value types exposed by the current runtime surface.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum PredefinedValueType {
    /// Boolean.
    Bool,
    /// Default signed integer.
    Int,
    /// 8-bit signed integer.
    Int8,
    /// 16-bit signed integer.
    Int16,
    /// 32-bit signed integer.
    Int32,
    /// 64-bit signed integer.
    Int64,
    /// 128-bit signed integer.
    Int128,
    /// Default unsigned integer.
    Uint,
    /// 8-bit unsigned integer.
    Uint8,
    /// 16-bit unsigned integer.
    Uint16,
    /// 32-bit unsigned integer.
    Uint32,
    /// 64-bit unsigned integer.
    Uint64,
    /// 128-bit unsigned integer.
    Uint128,
    /// Default floating-point number.
    Float,
    /// 32-bit floating-point number.
    Float32,
    /// 64-bit floating-point number.
    Float64,
    /// Fixed-precision decimal.
    Decimal,
    /// Database string.
    String,
    /// Byte string.
    Bytes,
    /// Date.
    Date,
    /// Local time.
    LocalTime,
    /// Zoned time.
    ZonedTime,
    /// Local datetime.
    LocalDateTime,
    /// Zoned datetime.
    ZonedDateTime,
    /// Duration.
    Duration,
    /// `DURATION (YEAR TO MONTH)`.
    DurationYearToMonth,
    /// `DURATION (DAY TO SECOND)`.
    DurationDayToSecond,
    /// Node reference.
    NodeRef,
    /// Edge reference.
    EdgeRef,
    /// Graph reference.
    GraphRef,
    /// Binding-table reference.
    TableRef,
    /// Path.
    Path,
    /// UUID.
    Uuid,
    /// Extension-owned value type.
    Extended(ExtensionTypeId),
    /// Native dense vector.
    Vector,
    /// Native JSON.
    Json,
}

/// Currently supported value cardinality.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ValueTypeCardinality {
    /// Exactly one value.
    ExactlyOne,
    /// Zero or one value.
    ZeroOrOne,
}

/// Closed record type definition.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordTypeDef {
    /// Stable record type ID.
    pub id: RecordTypeId,
    /// Database-string record type name.
    pub name: DbString,
    /// Field definitions in schema order.
    pub fields: SmallVec<[PropertyDef; 4]>,
}

/// Reserved policy for relationships between the key label sets of a closed
/// graph type's element types.
///
/// **Currently inert.** This value is persisted on [`GraphType`] but is not yet
/// consulted by any binding or validation path: closed-graph element binding
/// uses exact key-label-set equality, and the catalog DDL only produces
/// singleton key label sets (one label per type — see
/// `selene-gql`'s `create_node_type` lowering), so no two key label sets can
/// stand in an overlap/containment relationship. The variants reserve the two
/// ISO postures that become meaningful once future multi-label key label set
/// support lands.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum KeyLabelSetPolicy {
    /// Reserved: key label sets must be pairwise disjoint.
    NoOverlap,
    /// Reserved: key label sets may contain one another, enabling ISO/IEC
    /// 39075:2024 §4.13.2.7 *key label set implication consistency* — a type
    /// whose key label set is a subset of another type's label set "implies"
    /// that super-type. The current default; activated when multi-label key
    /// label sets ship.
    #[default]
    Containment,
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
