//! Immutable graph snapshot and read accessors.

use std::borrow::Cow;
use std::ops::RangeBounds;
use std::sync::Arc;

use immutable_chunkmap::map::MapM;
use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use selene_catalog::ElementKind::{Edge, Node};
use selene_catalog::IndexFamily::{Property, Text, Vector};
use selene_core::{DbString, EdgeId, GraphId, LabelSet, NodeId, PropertyMap, Value};

use crate::adjacency::AdjacencyEntry;
use crate::candidate_set::SnapshotLayout;
use crate::composite_typed_index::CompositeTypedIndex;
use crate::graph_types::GraphTypeDef;
use crate::id_map::{EngineIdMap, engine_id_map};
use crate::store::{EdgeRow, EdgeStore, NodeRow, NodeStore};
use crate::text_index::TextIndex;
use crate::typed_index::{TypedIndex, TypedIndexKind};
use crate::vector_index::VectorIndex;

mod cardinality;
mod constraint_declarations;
mod index_entries;
mod index_stats;
mod property_info;
mod registrations;

pub use index_entries::{
    CompositePropertyIndexEntry, CompositePropertyIndexEntryRow, PropertyIndexEntry,
    TextIndexEntry, TextIndexEntryRow, VectorIndexEntry, VectorIndexEntryRow,
};
pub use index_stats::{IndexedEntity, PropertyIndexStatsRow};
pub use property_info::PropertyIndexReadInfo;

/// Snapshot metadata.
#[derive(
    Clone,
    Debug,
    Deserialize,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
pub struct GraphMeta {
    /// Graph identifier.
    pub graph_id: GraphId,
    /// Published generation counter.
    pub generation: u64,
    /// Next node ID to allocate.
    pub next_node_id: u64,
    /// Next edge ID to allocate.
    pub next_edge_id: u64,
    /// Bound closed graph type. `None` means GG01/open graph.
    pub bound_type: Option<Arc<GraphTypeDef>>,
}

/// Immutable graph snapshot.
#[derive(Clone, Debug)]
pub struct SeleneGraph {
    /// Snapshot metadata.
    pub meta: GraphMeta,
    /// Node storage.
    pub node_store: NodeStore,
    /// Edge storage.
    pub edge_store: EdgeStore,
    /// Outgoing adjacency keyed by source node.
    pub adjacency_out: EngineIdMap<NodeId, AdjacencyEntry>,
    /// Incoming adjacency keyed by target node.
    pub adjacency_in: EngineIdMap<NodeId, AdjacencyEntry>,
    /// Undirected incidence, once per distinct endpoint, without orientation.
    pub adjacency_undirected: EngineIdMap<NodeId, AdjacencyEntry>,
    /// Bitmap of node rows carrying each label.
    pub idx_label: MapM<DbString, RoaringBitmap>,
    /// Bitmap of edge rows carrying each edge label.
    pub idx_edge_label: MapM<DbString, RoaringBitmap>,
    /// Per-`(label, property)` node value indexes. See spec 03 section 5.2.
    pub property_index: FxHashMap<(DbString, DbString), PropertyIndexEntry>,
    /// Per-`(edge label, property)` edge value indexes.
    pub edge_property_index: FxHashMap<(DbString, DbString), PropertyIndexEntry>,
    /// Per-`(label, properties...)` node composite value indexes.
    pub composite_property_index:
        FxHashMap<(DbString, SmallVec<[DbString; 4]>), CompositePropertyIndexEntry>,
    /// Per-`(label, property)` node vector indexes.
    pub vector_index: FxHashMap<(DbString, DbString), VectorIndexEntry>,
    /// Per-`(label, property)` node BM25 text indexes.
    pub text_index: FxHashMap<(DbString, DbString), TextIndexEntry>,
    /// Typed node inverse map used by graph internals.
    pub(crate) node_rows: EngineIdMap<NodeId, NodeRow>,
    /// Typed edge inverse map used by graph internals.
    pub(crate) edge_rows: EngineIdMap<EdgeId, EdgeRow>,
    /// Private, non-serialized identity for this physical snapshot layout.
    pub(crate) layout: SnapshotLayout,
    /// Derived catalog binding for facade-owned graphs, never snapshot/WAL payload.
    pub(crate) catalog_binding: Option<registrations::CatalogBinding>,
    pub(crate) expression_indexes: crate::expression_index::ExpressionIndexes,
    /// Complete constraint backing; never serialized and never supplied by callers.
    pub(crate) constraints: crate::type_validator::ConstraintIndexes,
    pub(crate) named_constraints:
        Option<(Arc<GraphTypeDef>, crate::type_validator::ConstraintIndexes)>,
}

impl SeleneGraph {
    /// Construct an empty graph snapshot.
    #[must_use]
    pub fn new(graph_id: GraphId) -> Self {
        Self {
            meta: GraphMeta {
                graph_id,
                generation: 0,
                next_node_id: 1,
                next_edge_id: 1,
                bound_type: None,
            },
            node_store: NodeStore::new(),
            edge_store: EdgeStore::new(),
            adjacency_out: engine_id_map(),
            adjacency_in: engine_id_map(),
            adjacency_undirected: engine_id_map(),
            idx_label: MapM::new(),
            idx_edge_label: MapM::new(),
            property_index: FxHashMap::default(),
            edge_property_index: FxHashMap::default(),
            composite_property_index: FxHashMap::default(),
            vector_index: FxHashMap::default(),
            text_index: FxHashMap::default(),
            node_rows: engine_id_map(),
            edge_rows: engine_id_map(),
            layout: SnapshotLayout::new(),
            catalog_binding: None,
            expression_indexes: Default::default(),
            constraints: crate::type_validator::ConstraintIndexes::default(),
            named_constraints: None,
        }
    }

    /// Return this graph snapshot's stable graph ID.
    #[must_use]
    pub const fn graph_id(&self) -> GraphId {
        self.meta.graph_id
    }

    /// Number of alive nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.node_store.alive.len() as usize
    }

    /// Number of alive edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edge_store.alive.len() as usize
    }

    /// Return current row-space pressure for compaction planning.
    ///
    /// This is a cheap read over store lengths and liveness bitmaps; it does not
    /// rebuild indexes or allocate a dense graph.
    #[must_use]
    pub fn compaction_stats(&self) -> crate::compaction::CompactionStats {
        crate::compaction::CompactionStats::from_graph(self)
    }

    pub(crate) fn node_row_for_id(&self, id: NodeId) -> Option<NodeRow> {
        self.node_rows.get(&id).copied()
    }

    pub(crate) fn edge_row_for_id(&self, id: EdgeId) -> Option<EdgeRow> {
        self.edge_rows.get(&id).copied()
    }

    pub(crate) fn node_id_for_node_row(&self, row: NodeRow) -> Option<NodeId> {
        self.node_store
            .row_to_id
            .get(row.index())
            .copied()
            .filter(|id| *id != NodeId::TOMBSTONE)
    }

    pub(crate) fn edge_id_for_edge_row(&self, row: EdgeRow) -> Option<EdgeId> {
        self.edge_store
            .row_to_id
            .get(row.index())
            .copied()
            .filter(|id| *id != EdgeId::TOMBSTONE)
    }

    /// Return true when `id` names an alive node.
    #[must_use]
    pub fn is_node_alive(&self, id: NodeId) -> bool {
        self.live_node_row(id).is_some()
    }

    /// Return true when `id` names an alive edge.
    #[must_use]
    pub fn is_edge_alive(&self, id: EdgeId) -> bool {
        self.live_edge_row(id).is_some()
    }

    /// Return node labels for an alive node.
    #[must_use]
    pub fn node_labels(&self, id: NodeId) -> Option<&LabelSet> {
        self.live_node_row(id)
            .and_then(|row| self.node_store.labels.get(row))
    }

    /// Return node properties for an alive node.
    #[must_use]
    pub fn node_properties(&self, id: NodeId) -> Option<&PropertyMap> {
        self.live_node_row(id)
            .and_then(|row| self.node_store.properties.get(row))
    }

    /// Return edge label for an alive edge.
    #[must_use]
    pub fn edge_label(&self, id: EdgeId) -> Option<&DbString> {
        self.live_edge_row(id)
            .and_then(|row| self.edge_store.label.get(row))
    }

    /// Return stored endpoints for an alive edge. For undirected edges these
    /// are canonical ID order, never a semantic source/destination pair.
    #[must_use]
    pub fn edge_endpoints(&self, id: EdgeId) -> Option<(NodeId, NodeId)> {
        self.live_edge_row(id).and_then(|row| {
            Some((
                *self.edge_store.source.get(row)?,
                *self.edge_store.target.get(row)?,
            ))
        })
    }

    /// Return edge properties for an alive edge.
    #[must_use]
    pub fn edge_properties(&self, id: EdgeId) -> Option<&PropertyMap> {
        self.live_edge_row(id)
            .and_then(|row| self.edge_store.properties.get(row))
    }

    /// Return intrinsic directionality for an alive edge.
    #[must_use]
    pub fn edge_directionality(&self, id: EdgeId) -> Option<selene_core::EdgeDirectionality> {
        self.live_edge_row(id)
            .and_then(|row| self.edge_store.directionality.get(row).copied())
    }

    /// Construct a versioned logical record independent of physical row layout.
    #[must_use]
    pub fn edge_record(&self, id: EdgeId) -> Option<selene_core::EdgeRecordV1> {
        let (first, second) = self.edge_endpoints(id)?;
        Some(selene_core::EdgeRecordV1 {
            id,
            label: self.edge_label(id)?.clone(),
            directionality: self.edge_directionality(id)?,
            first,
            second,
            properties: self.edge_properties(id)?.clone(),
        })
    }

    /// Return undirected incidence, once per edge identity at this endpoint.
    #[must_use]
    pub fn undirected_edges(&self, endpoint: NodeId) -> Option<&AdjacencyEntry> {
        self.adjacency_undirected.get(&endpoint)
    }

    /// Return outgoing adjacency for `source`.
    #[must_use]
    pub fn outgoing_edges(&self, source: NodeId) -> Option<&AdjacencyEntry> {
        self.adjacency_out.get(&source)
    }

    /// Return incoming adjacency for `target`.
    #[must_use]
    pub fn incoming_edges(&self, target: NodeId) -> Option<&AdjacencyEntry> {
        self.adjacency_in.get(&target)
    }

    /// Return true when an alive node has at least one incident edge.
    #[must_use]
    pub fn node_has_incident_edges(&self, id: NodeId) -> bool {
        self.outgoing_edges(id)
            .is_some_and(|entry| !entry.is_empty())
            || self
                .incoming_edges(id)
                .is_some_and(|entry| !entry.is_empty())
            || self
                .undirected_edges(id)
                .is_some_and(|entry| !entry.is_empty())
    }

    /// Return the bitmap of node rows carrying `label`.
    #[must_use]
    pub(crate) fn nodes_with_label(&self, label: &DbString) -> Option<&RoaringBitmap> {
        self.idx_label.get(label)
    }

    /// Return the bitmap of edge rows carrying `label`.
    #[must_use]
    pub(crate) fn edges_with_label(&self, label: &DbString) -> Option<&RoaringBitmap> {
        self.idx_edge_label.get(label)
    }

    /// Number of distinct node labels currently indexed.
    #[must_use]
    pub fn label_count(&self) -> usize {
        self.idx_label.len()
    }

    /// Number of distinct edge labels currently indexed.
    #[must_use]
    pub fn edge_label_count(&self) -> usize {
        self.idx_edge_label.len()
    }

    /// Return a clone of the registered `(label, property)` index.
    ///
    /// `None` also means the index omits live rows it cannot key, matching the
    /// probe accessors. Keeping the two in step stops the optimizer costing a
    /// plan against an index whose probes will decline at runtime.
    #[must_use]
    pub fn property_index_for(
        &self,
        label: &DbString,
        property: &DbString,
    ) -> Option<Arc<TypedIndex>> {
        self.query_property_entry(Node, label, property)
            .and_then(PropertyIndexEntry::probe_arc)
    }

    /// Return a clone of the registered edge `(label, property)` index.
    ///
    /// Declines on an incomplete index, as [`SeleneGraph::property_index_for`].
    #[must_use]
    pub fn edge_property_index_for(
        &self,
        label: &DbString,
        property: &DbString,
    ) -> Option<Arc<TypedIndex>> {
        self.query_property_entry(Edge, label, property)
            .and_then(PropertyIndexEntry::probe_arc)
    }

    /// Return a clone of the registered composite index.
    #[must_use]
    pub fn composite_property_index_for(
        &self,
        label: &DbString,
        properties: &[DbString],
    ) -> Option<Arc<CompositeTypedIndex>> {
        self.composite_property_index_entry_for(label, properties)
            .and_then(CompositePropertyIndexEntry::probe_arc)
    }

    /// Return composite index metadata for a property set.
    #[must_use]
    pub fn composite_property_index_entry_for(
        &self,
        label: &DbString,
        properties: &[DbString],
    ) -> Option<&CompositePropertyIndexEntry> {
        if !self.catalog_index_usable(Node, label, properties, Property) {
            return None;
        }
        let key = composite_property_key(properties);
        self.composite_property_index.get(&(label.clone(), key))
    }

    /// Return a clone of the registered vector index, declining incomplete
    /// accelerators left by a lenient rebuild. Exact search does not need one.
    #[must_use]
    pub fn vector_index_for(
        &self,
        label: &DbString,
        property: &DbString,
    ) -> Option<Arc<VectorIndex>> {
        if !self.catalog_index_usable(Node, label, std::slice::from_ref(property), Vector) {
            return None;
        }
        self.vector_index
            .get(&(label.clone(), property.clone()))
            .filter(|entry| entry.index.is_complete())
            .map(|entry| Arc::clone(&entry.index))
    }

    /// Return a clone of the registered text index.
    #[must_use]
    pub fn text_index_for(&self, label: &DbString, property: &DbString) -> Option<Arc<TextIndex>> {
        if !self.catalog_index_usable(Node, label, std::slice::from_ref(property), Text) {
            return None;
        }
        self.text_index
            .get(&(label.clone(), property.clone()))
            .filter(|entry| entry.index.has_current_contract())
            .map(|entry| Arc::clone(&entry.index))
    }

    /// Number of distinct `(label, property)` indexes currently registered.
    #[must_use]
    pub fn property_index_count(&self) -> usize {
        self.property_index.len()
    }

    /// Number of registered edge property indexes.
    #[must_use]
    pub fn edge_property_index_count(&self) -> usize {
        self.edge_property_index.len()
    }

    /// Number of distinct `(label, properties...)` indexes currently registered.
    #[must_use]
    pub fn composite_property_index_count(&self) -> usize {
        self.composite_property_index.len()
    }

    /// Number of distinct `(label, property)` vector indexes currently registered.
    #[must_use]
    pub fn vector_index_count(&self) -> usize {
        self.vector_index.len()
    }

    /// Number of distinct `(label, property)` text indexes currently registered.
    #[must_use]
    pub fn text_index_count(&self) -> usize {
        self.text_index.len()
    }

    /// Iterate built-in property indexes as owned `(label, property, kind)` tuples.
    ///
    /// This covers only SeleneGraph's built-in property indexes.
    /// Extension-provider index state is surfaced through that provider's own
    /// procedures.
    pub fn iter_property_indexes(
        &self,
    ) -> impl Iterator<Item = (DbString, DbString, TypedIndexKind)> + '_ {
        self.property_index
            .iter()
            .map(|((label, property), entry)| (label.clone(), property.clone(), entry.kind()))
    }

    /// Iterate built-in property indexes with optional explicit catalog names.
    pub fn iter_property_index_entries(
        &self,
    ) -> impl Iterator<Item = (DbString, DbString, TypedIndexKind, Option<DbString>)> + '_ {
        self.property_index
            .iter()
            .map(|((label, property), entry)| {
                (
                    label.clone(),
                    property.clone(),
                    entry.kind(),
                    entry.name.clone(),
                )
            })
    }

    /// Iterate built-in composite property indexes with optional explicit catalog names.
    pub fn iter_composite_property_index_entries(
        &self,
    ) -> impl Iterator<Item = CompositePropertyIndexEntryRow> + '_ {
        self.composite_property_index
            .iter()
            .map(|((label, _), entry)| {
                (
                    label.clone(),
                    entry.declared_properties.clone(),
                    entry.kinds(),
                    entry.name.clone(),
                )
            })
    }

    /// Iterate built-in vector indexes with optional explicit catalog names.
    pub fn iter_vector_index_entries(&self) -> impl Iterator<Item = VectorIndexEntryRow> + '_ {
        self.vector_index.iter().map(|((label, property), entry)| {
            (
                label.clone(),
                property.clone(),
                entry.kind(),
                entry.dimension(),
                entry.hnsw_config(),
                entry.ivf_config(),
                entry.name.clone(),
            )
        })
    }

    /// Iterate built-in text indexes with optional explicit catalog names.
    pub fn iter_text_index_entries(&self) -> impl Iterator<Item = TextIndexEntryRow> + '_ {
        self.text_index.iter().map(|((label, property), entry)| {
            (
                label.clone(),
                property.clone(),
                entry.stats(),
                entry.memory_usage(),
                entry.name.clone(),
            )
        })
    }

    /// Return rows matching `value` under a registered property index.
    ///
    /// `None` means the index cannot answer and the caller must scan; every
    /// probe on this type shares that contract. It covers three cases: no index
    /// is registered for `(label, property)`, the supplied value cannot be used
    /// with the registered kind, or the index omits live rows whose variant it
    /// cannot key and is therefore an incomplete view of the column.
    ///
    /// That last case is why probing with the index's own kind is not enough to
    /// stay correct. GQL equality is cross-variant, so `Int(3)` and
    /// `Float(3.0)` are equal, and an index keyed on one variant silently drops
    /// the other. Declining keeps the indexed and unindexed answers identical.
    ///
    /// `Some(empty)` means the index answered and no row matches.
    #[must_use]
    pub(crate) fn nodes_with_property_eq(
        &self,
        label: &DbString,
        property: &DbString,
        value: &Value,
    ) -> Option<Cow<'_, RoaringBitmap>> {
        self.query_property_entry(Node, label, property)
            .and_then(|entry| entry.lookup_eq(value))
    }

    /// Return the union of node rows matching any indexed scalar value.
    ///
    /// Shares the tri-state contract documented on
    /// [`SeleneGraph::nodes_with_property_eq`]: `None` also covers an index that
    /// omits live rows it cannot key, so the caller must scan.
    #[must_use]
    pub(crate) fn nodes_with_property_any(
        &self,
        label: &DbString,
        property: &DbString,
        values: &[Value],
    ) -> Option<RoaringBitmap> {
        let entry = self.query_property_entry(Node, label, property)?;
        let mut rows = RoaringBitmap::new();
        for value in values {
            rows |= entry.lookup_eq(value)?.as_ref();
        }
        Some(rows)
    }

    /// Return edge rows matching `value` under a registered edge property index.
    ///
    /// Shares the tri-state contract documented on
    /// [`SeleneGraph::nodes_with_property_eq`]: `None` also covers an index that
    /// omits live rows it cannot key, so the caller must scan.
    #[must_use]
    pub(crate) fn edges_with_property_eq(
        &self,
        label: &DbString,
        property: &DbString,
        value: &Value,
    ) -> Option<Cow<'_, RoaringBitmap>> {
        self.query_property_entry(Edge, label, property)
            .and_then(|entry| entry.lookup_eq(value))
    }

    /// Return the union of edge rows matching any indexed scalar value.
    ///
    /// Shares the tri-state contract documented on
    /// [`SeleneGraph::nodes_with_property_eq`]: `None` also covers an index that
    /// omits live rows it cannot key, so the caller must scan.
    #[must_use]
    pub(crate) fn edges_with_property_any(
        &self,
        label: &DbString,
        property: &DbString,
        values: &[Value],
    ) -> Option<RoaringBitmap> {
        let entry = self.query_property_entry(Edge, label, property)?;
        let mut rows = RoaringBitmap::new();
        for value in values {
            rows |= entry.lookup_eq(value)?.as_ref();
        }
        Some(rows)
    }

    /// Return rows matching `range` under a registered property index.
    ///
    /// Shares the tri-state contract documented on
    /// [`SeleneGraph::nodes_with_property_eq`]: `None` also covers an index that
    /// omits live rows it cannot key, so the caller must scan.
    #[must_use]
    pub(crate) fn nodes_with_property_range<R>(
        &self,
        label: &DbString,
        property: &DbString,
        range: R,
    ) -> Option<RoaringBitmap>
    where
        R: RangeBounds<Value>,
    {
        self.query_property_entry(Node, label, property)
            .and_then(|entry| entry.lookup_range(range))
    }

    /// Return edge rows matching `range` under a registered edge property index.
    ///
    /// Shares the tri-state contract documented on
    /// [`SeleneGraph::nodes_with_property_eq`]: `None` also covers an index that
    /// omits live rows it cannot key, so the caller must scan.
    #[must_use]
    pub(crate) fn edges_with_property_range<R>(
        &self,
        label: &DbString,
        property: &DbString,
        range: R,
    ) -> Option<RoaringBitmap>
    where
        R: RangeBounds<Value>,
    {
        self.query_property_entry(Edge, label, property)
            .and_then(|entry| entry.lookup_range(range))
    }

    /// Return rows whose string property key starts with `prefix`.
    ///
    /// Shares the tri-state contract documented on
    /// [`SeleneGraph::nodes_with_property_eq`]: `None` also covers an index that
    /// omits live rows it cannot key, so the caller must scan.
    #[must_use]
    pub(crate) fn nodes_with_property_prefix(
        &self,
        label: &DbString,
        property: &DbString,
        prefix: &str,
    ) -> Option<RoaringBitmap> {
        self.query_property_entry(Node, label, property)
            .and_then(|entry| entry.lookup_prefix(prefix))
    }

    fn live_node_row(&self, id: NodeId) -> Option<usize> {
        let row = self.node_row_for_id(id)?;
        (row.index() < self.node_store.len() && self.node_store.is_alive_row(row))
            .then_some(row.index())
    }

    fn live_edge_row(&self, id: EdgeId) -> Option<usize> {
        let row = self.edge_row_for_id(id)?;
        (row.index() < self.edge_store.len() && self.edge_store.is_alive_row(row))
            .then_some(row.index())
    }
}

pub(crate) fn composite_property_key(properties: &[DbString]) -> SmallVec<[DbString; 4]> {
    let mut key: SmallVec<[DbString; 4]> = properties.iter().cloned().collect();
    key.sort();
    key
}

#[cfg(test)]
mod tests;
