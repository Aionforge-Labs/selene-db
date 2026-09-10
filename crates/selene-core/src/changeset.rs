//! WAL change payloads per spec 02 section 9.
//!
//! The principal/audit actor lives in the WAL entry header per D12; these
//! payloads carry only the graph mutation itself. Diff payloads keep key lists
//! in canonical lexicographic order by [`DbString::as_str`] both in memory and on
//! the wire (the derived [`DbString`] `Ord` is lexicographic through the inner
//! string). Serialize canonicalizes (sorts) the lists before emitting — a no-op
//! for diffs built via the constructors, but load-bearing because the diff
//! fields are public and can be set non-canonically. Deserialize then validates
//! the canonical invariant and rejects a non-canonical or out-of-order payload
//! as malformed rather than re-sorting it.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::{
    DbString, EdgeEndpointDef, EdgeId, EdgeTypeDef, EdgeTypeDefV1, GraphId, GraphType, GraphTypeId,
    HnswIndexConfig, IvfIndexConfig, LabelSet, NodeId, NodeTypeDef, NodeTypeDefV1, PropertyDef,
    PropertyMap, RecordTypeDef,
};

mod diff;
mod stored;

pub use diff::{LabelDiff, PropertyDiff};

/// A graph or schema change carried by the WAL.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
// Invariant: serde+postcard tag stability - append new variants, never insert.
// Reordering corrupts WAL files written under prior tag layouts.
pub enum Change {
    /// Node creation.
    NodeCreated {
        /// Created node ID.
        id: NodeId,
        /// Initial labels.
        labels: LabelSet,
        /// Initial properties.
        properties: PropertyMap,
    },
    /// Node update.
    NodeUpdated {
        /// Updated node ID.
        id: NodeId,
        /// Label changes.
        labels_diff: LabelDiff,
        /// Property changes.
        properties_diff: PropertyDiff,
    },
    /// Node deletion.
    NodeDeleted {
        /// Deleted node ID.
        id: NodeId,
    },
    /// Edge creation.
    EdgeCreated {
        /// Created edge ID.
        id: EdgeId,
        /// Intrinsic directionality; endpoint order is not orientation for an undirected edge.
        directionality: crate::EdgeDirectionality,
        /// Edge label.
        label: DbString,
        /// Source node ID, or first canonical undirected endpoint.
        source: NodeId,
        /// Target node ID, or second canonical undirected endpoint.
        target: NodeId,
        /// Initial properties.
        properties: PropertyMap,
    },
    /// Edge update.
    EdgeUpdated {
        /// Updated edge ID.
        id: EdgeId,
        /// Property changes.
        properties_diff: PropertyDiff,
    },
    /// Edge deletion.
    EdgeDeleted {
        /// Deleted edge ID.
        id: EdgeId,
    },
    /// Schema mutation.
    SchemaChanged {
        /// Graph affected by the schema change.
        graph: GraphId,
        /// Schema change payload.
        change: SchemaChange,
    },
    /// Node property removal.
    NodePropertyRemoved {
        /// Updated node ID.
        id: NodeId,
        /// Removed property key.
        property: DbString,
    },
    /// Edge property removal.
    EdgePropertyRemoved {
        /// Updated edge ID.
        id: EdgeId,
        /// Removed property key.
        property: DbString,
    },
    /// Node label removal.
    NodeLabelRemoved {
        /// Updated node ID.
        id: NodeId,
        /// Removed label.
        label: DbString,
    },
    /// Bulk removal of every node carrying `label` plus all incident edges.
    ///
    /// This is the O(1)-WAL declarative truncate change (BRIEF-150, deletion-
    /// reclamation audit Item 11). It carries **only** the label — never the
    /// affected node/edge ids — so a `TRUNCATE NODE TYPE :L` of N nodes still
    /// writes exactly one WAL change. Recovery re-derives the affected rows by
    /// walking the recovered store ("replay walks store"), marking dead every
    /// alive node with `label` and every alive edge incident to such a node, so
    /// the recovered state is byte-identical to `MATCH (n:L) DETACH DELETE n`.
    /// Live commit fan-out substitutes the change with staged per-row
    /// `NodeDeleted`/`EdgeDeleted` tombstones when the mutator captured them
    /// during execution. WAL/recovery replay carries this persisted declarative
    /// variant, so provider-owned derived state must either handle it directly
    /// or rebuild from the recovered graph snapshot before serving reads.
    NodesOfTypeTruncated {
        /// Node label whose instances (and incident edges) were removed.
        label: DbString,
    },
    /// Bulk removal of every edge carrying `label`.
    ///
    /// The edge-type counterpart to [`Change::NodesOfTypeTruncated`]
    /// (`TRUNCATE EDGE TYPE :L`). Carries only the label (O(1) WAL); recovery
    /// re-derives the affected edges from the recovered store. Live commit
    /// fan-out substitutes the change with staged per-row `EdgeDeleted`
    /// tombstones when execution captured them; WAL/recovery replay carries
    /// this persisted declarative variant, so providers must handle it directly
    /// or rebuild before serving reads.
    EdgesOfTypeTruncated {
        /// Edge label whose instances were removed.
        label: DbString,
    },
    /// Factory-reset of the entire graph: wipe **all** nodes and edges (every
    /// label, including untyped/arbitrary-label rows) **and** reset the schema
    /// to open (`bound_type` -> `None`), in one declarative O(1)-WAL change.
    ///
    /// This is the `DROP GRAPH` factory-reset change (BRIEF-152, deletion-
    /// reclamation audit Item 10). Under D1 single-graph it targets the one
    /// bound graph. It carries **nothing** — never the affected node/edge ids
    /// nor any schema payload — so a reset of a graph with N rows still writes
    /// exactly one WAL change. Recovery re-derives every affected row by walking
    /// the recovered store ("replay walks store"), marking dead every alive node
    /// and edge, and forces the recovered `bound_type` to `None`, so the
    /// recovered state is byte-identical to `MATCH (n) DETACH DELETE n` followed
    /// by a full schema drop. Live commit fan-out substitutes the change with
    /// staged per-row `NodeDeleted`/`EdgeDeleted` tombstones when execution
    /// captured them. WAL/recovery replay carries this persisted declarative
    /// variant, so providers must handle it directly or rebuild before serving
    /// reads. The MANIFEST epoch and WAL archive lineage are untouched: a
    /// factory-reset is one committed WAL entry on top of the existing snapshot,
    /// not a file-level wipe.
    GraphReset {},
}

/// Schema change payload.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum SchemaChange {
    /// Graph creation.
    GraphCreated {
        /// Created graph ID.
        id: GraphId,
        /// Graph name.
        name: DbString,
        /// Optional graph type assigned at creation.
        graph_type: Option<GraphTypeId>,
    },
    /// Graph deletion.
    GraphDropped {
        /// Dropped graph ID.
        id: GraphId,
    },
    /// Graph type creation.
    GraphTypeCreated {
        /// Created graph type definition.
        graph_type: GraphType,
    },
    /// Graph type deletion.
    GraphTypeDropped {
        /// Dropped graph type ID.
        id: GraphTypeId,
    },
    /// Node type addition.
    NodeTypeAdded {
        /// Owning graph type.
        graph_type: GraphTypeId,
        /// Node type label.
        label: DbString,
        /// Legacy node type definition.
        def: NodeTypeDefV1,
    },
    /// Edge type addition.
    EdgeTypeAdded {
        /// Owning graph type.
        graph_type: GraphTypeId,
        /// Edge type label.
        label: DbString,
        /// Legacy edge type definition.
        def: EdgeTypeDefV1,
    },
    /// Node type deletion.
    NodeTypeDropped {
        /// Owning graph type.
        graph_type: GraphTypeId,
        /// Dropped node type name.
        name: DbString,
    },
    /// Edge type deletion.
    EdgeTypeDropped {
        /// Owning graph type.
        graph_type: GraphTypeId,
        /// Dropped edge type name.
        name: DbString,
    },
    /// Record type addition.
    RecordTypeAdded {
        /// Owning graph type.
        graph_type: GraphTypeId,
        /// Record type definition.
        def: RecordTypeDef,
    },
    /// Property index creation.
    PropertyIndexCreated {
        /// Indexed node label.
        label: DbString,
        /// Indexed property key.
        property: DbString,
        /// Declared index value kind.
        kind: SchemaPropertyIndexKind,
    },
    /// Property index deletion.
    PropertyIndexDropped {
        /// Indexed node label.
        label: DbString,
        /// Indexed property key.
        property: DbString,
    },
    /// Property index creation with optional explicit catalog name.
    ///
    /// Declared after every existing v1.1 variant so the `postcard`
    /// discriminants of all earlier variants remain stable. Old WALs continue
    /// to decode through [`SchemaChange::PropertyIndexCreated`].
    PropertyIndexCreatedNamed {
        /// Indexed node label.
        label: DbString,
        /// Indexed property key.
        property: DbString,
        /// Declared index value kind.
        kind: SchemaPropertyIndexKind,
        /// Optional explicit catalog name.
        name: Option<DbString>,
    },
    /// Node type addition carrying v2 type-model fields.
    ///
    /// Declared after every existing v1.1 variant so the `postcard`
    /// discriminants of all earlier variants remain stable. New code emits this
    /// variant; old WALs continue to decode through [`SchemaChange::NodeTypeAdded`].
    NodeTypeAddedV2 {
        /// Owning graph type.
        graph_type: GraphTypeId,
        /// Node type label.
        label: DbString,
        /// Node type definition.
        def: NodeTypeDef,
    },
    /// Edge type addition carrying v2 type-model fields.
    ///
    /// Declared after every existing v1.1 variant so the `postcard`
    /// discriminants of all earlier variants remain stable. New code emits this
    /// variant; old WALs continue to decode through [`SchemaChange::EdgeTypeAdded`].
    EdgeTypeAddedV2 {
        /// Owning graph type.
        graph_type: GraphTypeId,
        /// Edge type label.
        label: DbString,
        /// Edge type definition.
        def: EdgeTypeDef,
    },
    /// Composite property index creation with optional explicit catalog name.
    ///
    /// Declared after every existing v1.1 variant so the `postcard`
    /// discriminants of all earlier variants remain stable.
    CompositePropertyIndexCreated {
        /// Indexed node label.
        label: DbString,
        /// Indexed property keys in declaration order.
        properties: SmallVec<[DbString; 4]>,
        /// Declared index value kinds in declaration order.
        kinds: SmallVec<[SchemaPropertyIndexKind; 4]>,
        /// Optional explicit catalog name.
        name: Option<DbString>,
    },
    /// Composite property index deletion.
    ///
    /// Declared after every existing v1.1 variant so the `postcard`
    /// discriminants of all earlier variants remain stable.
    CompositePropertyIndexDropped {
        /// Indexed node label.
        label: DbString,
        /// Indexed property keys in declaration order.
        properties: SmallVec<[DbString; 4]>,
    },
    /// Vector property index creation with optional explicit catalog name.
    ///
    /// Declared after every existing v1.1 variant so the `postcard`
    /// discriminants of all earlier variants remain stable.
    VectorIndexCreated {
        /// Indexed node label.
        label: DbString,
        /// Indexed vector property key.
        property: DbString,
        /// Declared vector index algorithm.
        kind: SchemaVectorIndexKind,
        /// Required vector dimensionality for indexed rows.
        dimension: u32,
        /// Optional explicit catalog name.
        name: Option<DbString>,
        /// Optional HNSW construction parameters.
        hnsw_config: Option<HnswIndexConfig>,
        /// Optional IVF construction parameters.
        ivf_config: Option<IvfIndexConfig>,
    },
    /// Vector property index deletion.
    ///
    /// Declared after every existing v1.1 variant so the `postcard`
    /// discriminants of all earlier variants remain stable.
    VectorIndexDropped {
        /// Indexed node label.
        label: DbString,
        /// Indexed vector property key.
        property: DbString,
    },
    /// Text property index creation with optional explicit catalog name.
    ///
    /// Declared after every existing v1.1 variant so the `postcard`
    /// discriminants of all earlier variants remain stable.
    TextIndexCreated {
        /// Indexed node label.
        label: DbString,
        /// Indexed string property key.
        property: DbString,
        /// Optional explicit catalog name.
        name: Option<DbString>,
    },
    /// Text property index deletion.
    ///
    /// Declared after every existing v1.1 variant so the `postcard`
    /// discriminants of all earlier variants remain stable.
    TextIndexDropped {
        /// Indexed node label.
        label: DbString,
        /// Indexed string property key.
        property: DbString,
    },
    /// Edge property index creation with optional explicit catalog name.
    ///
    /// Declared after every existing variant so the `postcard` discriminants of
    /// all earlier variants remain stable.
    EdgePropertyIndexCreated {
        /// Indexed edge label.
        label: DbString,
        /// Indexed edge property key.
        property: DbString,
        /// Declared index value kind.
        kind: SchemaPropertyIndexKind,
        /// Optional explicit catalog name.
        name: Option<DbString>,
    },
    /// Edge property index deletion.
    ///
    /// Declared after every existing variant so the `postcard` discriminants of
    /// all earlier variants remain stable.
    EdgePropertyIndexDropped {
        /// Indexed edge label.
        label: DbString,
        /// Indexed edge property key.
        property: DbString,
    },
    /// In-place node type alteration carrying newly added properties.
    ///
    /// Declared after every existing variant so the `postcard` discriminants of
    /// all earlier variants remain stable. Recovery validates that the payload
    /// is additive before extending the named node-type slot in place.
    NodeTypeAlteredV2 {
        /// Owning graph type.
        graph_type: GraphTypeId,
        /// Altered node type label.
        label: DbString,
        /// Newly added property descriptors in declaration order.
        properties: SmallVec<[PropertyDef; 8]>,
    },
    /// In-place edge type alteration carrying only additive changes.
    ///
    /// Declared after every existing variant so the `postcard` discriminants of
    /// all earlier variants remain stable. Endpoint references identify node
    /// types by name. Recovery validates endpoint widening and property
    /// additions before replacing the named edge-type slot in place.
    EdgeTypeAlteredV2 {
        /// Owning graph type.
        graph_type: GraphTypeId,
        /// Altered edge type name.
        name: DbString,
        /// Replacement source endpoint when it changed.
        source_node_type: Option<EdgeEndpointDef>,
        /// Replacement target endpoint when it changed.
        target_node_type: Option<EdgeEndpointDef>,
        /// Newly added property descriptors in declaration order.
        properties: SmallVec<[PropertyDef; 4]>,
    },
}

/// Schema-level vector index algorithm kind.
///
/// This mirrors storage-level vector index algorithm selection without making
/// `selene-core` depend on graph storage internals.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SchemaVectorIndexKind {
    /// Exact in-memory row-set accelerator. ANN algorithms can be added as new
    /// variants without changing the `(label, property)` catalog identity.
    Flat,
    /// Approximate HNSW index using squared Euclidean distance.
    HnswSquaredEuclidean,
    /// Approximate HNSW index using cosine distance.
    HnswCosine,
    /// Approximate HNSW index using negative inner product distance.
    HnswNegativeInnerProduct,
    /// Approximate IVF index using squared Euclidean distance.
    IvfSquaredEuclidean,
    /// Approximate IVF index using cosine distance.
    IvfCosine,
    /// Approximate IVF index using negative inner product distance.
    IvfNegativeInnerProduct,
    /// Compressed TurboQuant candidate index using cosine distance.
    TurboQuantCosine,
}

/// Schema-level property index value kind.
///
/// This mirrors `selene_graph::TypedIndexKind` without making `selene-core`
/// depend on graph storage internals.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SchemaPropertyIndexKind {
    /// Boolean value.
    Bool,
    /// Signed 64-bit integer.
    I64,
    /// Unsigned 64-bit integer.
    U64,
    /// Signed 128-bit integer.
    I128,
    /// Unsigned 128-bit integer.
    U128,
    /// Fixed-precision decimal value.
    Decimal,
    /// Finite 32-bit floating-point value.
    F32,
    /// Finite 64-bit floating-point value.
    F64,
    /// Database string.
    String,
    /// Civil date.
    Date,
    /// Civil local date-time.
    LocalDateTime,
    /// Zoned date-time.
    ZonedDateTime,
    /// Civil local time.
    LocalTime,
    /// Zoned time.
    ZonedTime,
    /// Duration.
    Duration,
    /// UUID.
    Uuid,
}

#[cfg(test)]
#[path = "changeset/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "changeset/proptests.rs"]
mod proptests;
