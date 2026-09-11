//! Recovery-mode state for the core graph provider.

use std::collections::BTreeMap;
use std::sync::Arc;

use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;
use selene_core::{EdgeId, GraphId, NodeId, SchemaChange};
use smallvec::SmallVec;

use crate::core_provider::sections::{
    CompositeSchemaEntry, CompositeSchemaKey, EdgeRow, MetaPayload, NodeRow, SchemaEntityKind,
    SchemaEntry, SchemaKey, TextSchemaEntry, TextSchemaKey, VectorSchemaEntry, VectorSchemaKey,
    decode_composite_schemas, decode_edges, decode_graph_types, decode_meta, decode_nodes,
    decode_schemas, decode_text_schemas, decode_vector_schemas,
};
use crate::core_provider::{
    CORE_CPIX_SUB, CORE_EDGE_SUB, CORE_GTYP_SUB, CORE_META_SUB, CORE_NODE_SUB, CORE_SCMA_SUB,
    CORE_TIDX_SUB, CORE_VIDX_SUB, inconsistent, invalid_payload,
};
use crate::graph::{
    CompositePropertyIndexEntry, GraphMeta, PropertyIndexEntry, SeleneGraph, TextIndexEntry,
    VectorIndexEntry,
};
use crate::graph_types::{EdgeEndpointDef, GraphTypeDef, PropertyTypeDef};
use crate::store::{EdgeRow as PhysicalEdgeRow, NodeRow as PhysicalNodeRow};
use crate::typed_index::TypedIndex;

mod change_replay;
mod index_replay;
mod logical;
mod materialize;
pub(crate) use logical::logical_graph;
mod schema_replay;

pub(super) fn decode_schema_property(
    property: &selene_core::PropertyDef,
) -> Result<PropertyTypeDef, crate::ProviderError> {
    schema_replay::runtime_property(property)
}

pub(super) fn decode_schema_edge_endpoint(
    graph_type: &GraphTypeDef,
    endpoint: &selene_core::EdgeEndpointDef,
    role: &str,
) -> Result<EdgeEndpointDef, crate::ProviderError> {
    schema_replay::runtime_altered_edge_endpoint_def(graph_type, endpoint, role)
}

use index_replay::{
    PendingCompositeIndex, PendingIndex, PendingTextIndex, PendingVectorIndex,
    replay_composite_property_index_changes, replay_property_index_changes,
    replay_text_index_changes, replay_vector_index_changes,
};
use materialize::{insert_edge_row, insert_node_row};

/// Accumulator populated by snapshot sections and WAL replay.
#[derive(Default)]
pub(crate) struct RecoveryState {
    meta: Option<MetaPayload>,
    graph_types: BTreeMap<u32, Arc<GraphTypeDef>>,
    pending_schema_changes: Vec<SchemaChange>,
    pending_property_index_changes: Vec<PendingIndex>,
    pending_composite_property_index_changes: Vec<PendingCompositeIndex>,
    pending_vector_index_changes: Vec<PendingVectorIndex>,
    pending_text_index_changes: Vec<PendingTextIndex>,
    /// Recovered node rows keyed by external id. Snapshot rows carry their
    /// decoded section position; WAL-created rows carry no position and append
    /// at the dense end during materialization. Companion order vectors preserve
    /// positional snapshot placement without per-row tree inserts.
    nodes: FxHashMap<NodeId, RecoveredNodeRow>,
    /// Snapshot node IDs in decoded positional order.
    snapshot_node_order: Vec<NodeId>,
    /// WAL-created node IDs. Sorted only at materialization time.
    wal_node_order: Vec<NodeId>,
    /// Recovered edge rows keyed by external id; see `nodes` for the positional
    /// recovery contract.
    edges: FxHashMap<EdgeId, RecoveredEdgeRow>,
    /// Snapshot edge IDs in decoded positional order.
    snapshot_edge_order: Vec<EdgeId>,
    /// WAL-created edge IDs. Sorted only at materialization time.
    wal_edge_order: Vec<EdgeId>,
    schemas: BTreeMap<SchemaKey, SchemaEntry>,
    composite_schemas: Vec<(CompositeSchemaKey, CompositeSchemaEntry)>,
    vector_schemas: Vec<(VectorSchemaKey, VectorSchemaEntry)>,
    text_schemas: Vec<(TextSchemaKey, TextSchemaEntry)>,
    sequence: u64,
    /// Set once a [`Change::GraphReset`] (BRIEF-152, audit Item 10) is replayed.
    ///
    /// A factory-reset moots all schema/index intents seen so far in the WAL, so
    /// the reset arm clears the pending lists and sets this flag. `into_graph`
    /// then short-circuits the snapshot/caller bound-type reconciliation and
    /// forces `bound_type = None` (open), matching the runtime reset. Without
    /// this, a `recover_closed(bound_type)` after a reset would reject (snapshot
    /// declares no binding, caller asserts one) or silently restore the
    /// pre-reset type from the snapshot.
    schema_reset_to_open: bool,
    /// Graph id carried by the first replayed [`Change::SchemaChanged`].
    ///
    /// Production DDL stamps the authoring graph's own id into every schema
    /// record, so for a WAL-only directory this is the only identity that
    /// exists on disk. A [`Change::GraphReset`] does not clear it: a reset
    /// truncates and re-opens a graph, it does not rename one.
    wal_graph_id: Option<GraphId>,
    /// First replayed schema-record graph id that disagreed with
    /// [`Self::wal_graph_id`], if any. A WAL holding two identities is already
    /// cross-wired, and is retained only to name the second id in the error.
    wal_graph_id_conflict: Option<GraphId>,
}

const V1_BOUND_GRAPH_TYPE_INDEX: u32 = 0;

pub(super) struct RecoveredNodeRow {
    pub(super) row: NodeRow,
    /// BRIEF-Item-4a STEP 9: snapshot rows materialize at decoded section
    /// position. WAL-created ids absent from the snapshot append at the dense
    /// end (BRIEF-Item-4c).
    snapshot_position: Option<u32>,
}

impl RecoveredNodeRow {
    fn from_snapshot(row: NodeRow, position: u32) -> Self {
        Self {
            row,
            snapshot_position: Some(position),
        }
    }

    pub(super) fn from_wal(row: NodeRow) -> Self {
        Self {
            row,
            snapshot_position: None,
        }
    }
}

pub(super) struct RecoveredEdgeRow {
    pub(super) row: EdgeRow,
    /// Decoded snapshot section position, or `None` for post-snapshot WAL rows.
    snapshot_position: Option<u32>,
}

impl RecoveredEdgeRow {
    fn from_snapshot(row: EdgeRow, position: u32) -> Self {
        Self {
            row,
            snapshot_position: Some(position),
        }
    }

    pub(super) fn from_wal(row: EdgeRow) -> Self {
        Self {
            row,
            snapshot_position: None,
        }
    }
}

impl RecoveryState {
    /// Construct an empty recovery accumulator.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn read_section(
        &mut self,
        sub_tag: crate::SubTag,
        bytes: &[u8],
    ) -> Result<(), crate::ProviderError> {
        match sub_tag.0 {
            CORE_GTYP_SUB => {
                let mut graph_types = BTreeMap::new();
                for (index, graph_type) in decode_graph_types(bytes)? {
                    let graph_type = graph_type
                        .validate()
                        .map_err(|error| inconsistent(format!("CORE/GTYP is invalid: {error}")))?;
                    graph_types.insert(index, Arc::new(graph_type));
                }
                self.graph_types = graph_types;
            }
            CORE_META_SUB => {
                let payload = decode_meta(bytes)?;
                self.sequence = payload.sequence;
                self.meta = Some(payload);
            }
            CORE_NODE_SUB => {
                let rows = decode_nodes(bytes)?;
                self.nodes.reserve(rows.len());
                // BRIEF-Item-4a STEP 9: the section is positional. Record each
                // committed id's row (= decode position) for positional
                // materialization; skip `NodeId::TOMBSTONE` hole rows — they are
                // re-padded between the real rows in `into_graph` and binding
                // their (absent) id would resurrect an aborted-tx id as NotAlive.
                for (position, (id, row)) in rows.into_iter().enumerate() {
                    if id == NodeId::TOMBSTONE {
                        continue;
                    }
                    let position = u32::try_from(position).map_err(|_| {
                        invalid_payload("CORE/NODE row position exceeds u32::MAX".to_string())
                    })?;
                    self.nodes
                        .insert(id, RecoveredNodeRow::from_snapshot(row, position));
                    self.snapshot_node_order.push(id);
                }
            }
            CORE_EDGE_SUB => {
                let rows = decode_edges(bytes)?;
                self.edges.reserve(rows.len());
                for (position, (id, row)) in rows.into_iter().enumerate() {
                    if id == EdgeId::TOMBSTONE {
                        continue;
                    }
                    let position = u32::try_from(position).map_err(|_| {
                        invalid_payload("CORE/EDGE row position exceeds u32::MAX".to_string())
                    })?;
                    self.edges
                        .insert(id, RecoveredEdgeRow::from_snapshot(row, position));
                    self.snapshot_edge_order.push(id);
                }
            }
            CORE_SCMA_SUB => {
                self.schemas = decode_schemas(bytes)?.into_iter().collect();
            }
            CORE_CPIX_SUB => {
                self.composite_schemas = decode_composite_schemas(bytes)?;
            }
            CORE_VIDX_SUB => {
                self.vector_schemas = decode_vector_schemas(bytes)?;
            }
            CORE_TIDX_SUB => {
                self.text_schemas = decode_text_schemas(bytes)?;
            }
            _ => {
                return Err(invalid_payload(format!("unknown CORE sub-tag {sub_tag}")));
            }
        }
        Ok(())
    }

    /// Refuse a WAL whose schema records were authored under a different graph.
    ///
    /// `CORE/META` carries the identity of a snapshot-bearing directory and is
    /// checked against the caller's assertion below. A WAL-only directory has
    /// no `CORE/META`, so before this check nothing on disk contradicted a
    /// wrong `expected_graph_id`: recovery reconstructed another graph's rows
    /// under the caller's name and then kept writing into them.
    ///
    /// Production DDL stamps the live graph's own id into every
    /// `Change::SchemaChanged`, so any schema record in the WAL is evidence of
    /// the authoring identity. Coverage is therefore partial by construction —
    /// a WAL carrying only data changes declares no identity, and a wrong
    /// assertion against it still cannot be detected here.
    fn check_wal_graph_id(&self, expected_graph_id: GraphId) -> crate::GraphResult<()> {
        // A conflicting id is reported even when the first one matches: a WAL
        // holding two identities is cross-wired whichever one the caller
        // asserted, so recovering it replays some other graph's commits.
        let Some(observed) = self
            .wal_graph_id
            .filter(|id| *id != expected_graph_id)
            .or(self.wal_graph_id_conflict)
        else {
            return Ok(());
        };
        Err(crate::GraphError::Provider(inconsistent(format!(
            "WAL schema records declare {observed} but caller asserted \
             {expected_graph_id} during recovery; refusing to silently \
             reconstruct under the wrong identity",
        ))))
    }

    pub(crate) fn into_graph(
        self,
        expected_graph_id: GraphId,
        expected_bound_type: Option<Arc<GraphTypeDef>>,
    ) -> crate::GraphResult<SeleneGraph> {
        // F5: GTYP non-empty + META missing is a structurally inconsistent
        // snapshot. Type rows must have a META to bind to, otherwise recovery
        // would silently downgrade a closed graph to open.
        if self.meta.is_none() && !self.graph_types.is_empty() {
            return Err(crate::GraphError::Provider(inconsistent(
                "CORE/GTYP non-empty but CORE/META missing; snapshot is \
                 structurally inconsistent",
            )));
        }
        self.check_wal_graph_id(expected_graph_id)?;
        // BRIEF-152: a replayed GraphReset forces the recovered graph open and
        // moots every prior schema intent. Short-circuit the snapshot/caller
        // bound-type reconciliation entirely — bind to None regardless of what
        // the snapshot or caller asserted — so a `recover_closed(bound_type)`
        // after a reset reconstructs the identical empty+open post-state the
        // runtime produced instead of rejecting on the reconciliation conflict.
        let schema_reset_to_open = self.schema_reset_to_open;
        let meta = match self.meta {
            Some(meta) if schema_reset_to_open => {
                if meta.graph_id != expected_graph_id {
                    return Err(crate::GraphError::Provider(inconsistent(format!(
                        "CORE/META declares {} but caller asserted {} during recovery; \
                         refusing to silently reconstruct under the wrong identity",
                        meta.graph_id, expected_graph_id,
                    ))));
                }
                GraphMeta {
                    graph_id: meta.graph_id,
                    generation: meta.generation,
                    next_node_id: meta.next_node_id,
                    next_edge_id: meta.next_edge_id,
                    bound_type: None,
                }
            }
            None if schema_reset_to_open => GraphMeta {
                graph_id: expected_graph_id,
                generation: 0,
                next_node_id: 1,
                next_edge_id: 1,
                bound_type: None,
            },
            Some(meta) => {
                if meta.graph_id != expected_graph_id {
                    return Err(crate::GraphError::Provider(inconsistent(format!(
                        "CORE/META declares {} but caller asserted {} during recovery; \
                         refusing to silently reconstruct under the wrong identity",
                        meta.graph_id, expected_graph_id,
                    ))));
                }
                let snapshot_bound_type = match meta.bound_type_index {
                    Some(index) => {
                        Some(self.graph_types.get(&index).cloned().ok_or_else(|| {
                            crate::GraphError::Provider(inconsistent(format!(
                                "CORE/META references missing CORE/GTYP index {index}"
                            )))
                        })?)
                    }
                    None => None,
                };
                // Reconcile snapshot binding with caller's assertion. Either
                // side disagreeing is closed/open drift the user must surface.
                let mut bound_type = match (&snapshot_bound_type, &expected_bound_type) {
                    (Some(snap), Some(caller)) if snap.as_ref() != caller.as_ref() => {
                        return Err(crate::GraphError::Provider(inconsistent(
                            "CORE/META bound_type disagrees with caller-supplied \
                             bound_type during recovery; refusing to reconstruct \
                             under the wrong type",
                        )));
                    }
                    (Some(snap), _) => Some(snap.clone()),
                    (None, Some(_)) => {
                        return Err(crate::GraphError::Provider(inconsistent(
                            "caller supplied bound_type but CORE/META declares no \
                             binding; refusing to reconstruct under the wrong shape",
                        )));
                    }
                    (None, None) => None,
                };
                schema_replay::replay_schema_changes(
                    &mut bound_type,
                    &self.pending_schema_changes,
                )?;
                GraphMeta {
                    graph_id: meta.graph_id,
                    generation: meta.generation,
                    next_node_id: meta.next_node_id,
                    next_edge_id: meta.next_edge_id,
                    bound_type,
                }
            }
            // F2: WAL-only recovery preserves the caller's binding so a
            // closed-graph crash before the first snapshot does not silently
            // downgrade to open and skip GG02 validation forever after.
            None => {
                let mut bound_type = expected_bound_type.clone();
                schema_replay::replay_schema_changes(
                    &mut bound_type,
                    &self.pending_schema_changes,
                )?;
                GraphMeta {
                    graph_id: expected_graph_id,
                    generation: 0,
                    next_node_id: 1,
                    next_edge_id: 1,
                    bound_type,
                }
            }
        };
        let mut graph = SeleneGraph::new(meta.graph_id);
        graph.meta = meta;

        // BRIEF-Item-4a STEP 9 / BRIEF-Item-4c: materialize each row at its true
        // row index. Snapshot rows use their decoded position
        // (`RecoveredNodeRow::snapshot_position`); a WAL-created id absent from the snapshot
        // appends at the dense end (the live-append slot). Snapshot row ids are
        // carried in decoded-position order, so a compacted snapshot whose ids
        // are not row-sorted still lands each row at its recorded position.
        // WAL-created rows append afterward in id order. The hole slots between
        // recorded positions are padded with `NodeId::TOMBSTONE` and stay out
        // of the id->row map.
        let mut nodes = self.nodes;
        let mut node_alive = RoaringBitmap::new();
        let mut next_node_id = graph.meta.next_node_id.max(1);
        for id in self.snapshot_node_order {
            let Some(recovered) = nodes.remove(&id) else {
                return Err(crate::GraphError::Provider(inconsistent(format!(
                    "snapshot node order referenced missing node {id}"
                ))));
            };
            next_node_id = next_node_id.max(id.get().saturating_add(1));
            let row_index = match recovered.snapshot_position {
                Some(position) => PhysicalNodeRow::new(position),
                None => {
                    let len = graph.node_store.len();
                    // u32::MAX is reserved as RowIndex::TOMBSTONE; the last real
                    // row is u32::MAX - 1, so a live row never aliases the sentinel.
                    if !u32::try_from(len).is_ok_and(|row| row != u32::MAX) {
                        return Err(crate::GraphError::Provider(invalid_payload(format!(
                            "WAL-created node id {id} exceeds the u32 row space"
                        ))));
                    }
                    PhysicalNodeRow::new(len as u32)
                }
            };
            if recovered.row.alive {
                node_alive.insert(row_index.get());
            }
            insert_node_row(&mut graph, id, recovered.row, row_index)?;
        }
        let mut wal_node_order = self.wal_node_order;
        wal_node_order.sort_unstable();
        for id in wal_node_order {
            let Some(recovered) = nodes.remove(&id) else {
                return Err(crate::GraphError::Provider(inconsistent(format!(
                    "WAL node order referenced missing node {id}"
                ))));
            };
            next_node_id = next_node_id.max(id.get().saturating_add(1));
            // BRIEF-Item-4c: WAL-created ids (absent from the snapshot) APPEND
            // at the dense end, not `id - 1`. After a compacted snapshot loads
            // (dense rows, sparse high-water ids) a post-compaction
            // `NodeCreated` would otherwise re-pad the reclaimed holes on reload.
            // By the time a WAL-created row is reached every snapshot row is
            // placed and `len()` is the next dense slot, matching the live append
            // create path.
            let len = graph.node_store.len();
            // u32::MAX is reserved as RowIndex::TOMBSTONE; the last real row is
            // u32::MAX - 1, so a live row never aliases the sentinel.
            if !u32::try_from(len).is_ok_and(|row| row != u32::MAX) {
                return Err(crate::GraphError::Provider(invalid_payload(format!(
                    "WAL-created node id {id} exceeds the u32 row space"
                ))));
            }
            if recovered.row.alive {
                node_alive.insert(len as u32);
            }
            insert_node_row(
                &mut graph,
                id,
                recovered.row,
                PhysicalNodeRow::new(len as u32),
            )?;
        }
        if !nodes.is_empty() {
            return Err(crate::GraphError::Provider(inconsistent(format!(
                "{} recovered node rows were not materialized",
                nodes.len()
            ))));
        }
        graph.node_store.alive = Arc::new(node_alive);
        graph.meta.next_node_id = next_node_id;

        let mut edges = self.edges;
        let mut edge_alive = RoaringBitmap::new();
        let mut next_edge_id = graph.meta.next_edge_id.max(1);
        for id in self.snapshot_edge_order {
            let Some(recovered) = edges.remove(&id) else {
                return Err(crate::GraphError::Provider(inconsistent(format!(
                    "snapshot edge order referenced missing edge {id}"
                ))));
            };
            next_edge_id = next_edge_id.max(id.get().saturating_add(1));
            // BRIEF-Item-4c: WAL-created edge ids APPEND at the dense end (see the
            // node arm above).
            let row_index = match recovered.snapshot_position {
                Some(position) => PhysicalEdgeRow::new(position),
                None => {
                    let len = graph.edge_store.len();
                    // u32::MAX is reserved as RowIndex::TOMBSTONE (see the node arm).
                    if !u32::try_from(len).is_ok_and(|row| row != u32::MAX) {
                        return Err(crate::GraphError::Provider(invalid_payload(format!(
                            "WAL-created edge id {id} exceeds the u32 row space"
                        ))));
                    }
                    PhysicalEdgeRow::new(len as u32)
                }
            };
            if recovered.row.alive {
                edge_alive.insert(row_index.get());
            }
            insert_edge_row(&mut graph, id, recovered.row, row_index)?;
        }
        let mut wal_edge_order = self.wal_edge_order;
        wal_edge_order.sort_unstable();
        for id in wal_edge_order {
            let Some(recovered) = edges.remove(&id) else {
                return Err(crate::GraphError::Provider(inconsistent(format!(
                    "WAL edge order referenced missing edge {id}"
                ))));
            };
            next_edge_id = next_edge_id.max(id.get().saturating_add(1));
            // BRIEF-Item-4c: WAL-created edge ids append at the dense end for
            // the same reason as WAL-created node ids.
            let len = graph.edge_store.len();
            // u32::MAX is reserved as RowIndex::TOMBSTONE (see the node arm).
            if !u32::try_from(len).is_ok_and(|row| row != u32::MAX) {
                return Err(crate::GraphError::Provider(invalid_payload(format!(
                    "WAL-created edge id {id} exceeds the u32 row space"
                ))));
            }
            if recovered.row.alive {
                edge_alive.insert(len as u32);
            }
            insert_edge_row(
                &mut graph,
                id,
                recovered.row,
                PhysicalEdgeRow::new(len as u32),
            )?;
        }
        if !edges.is_empty() {
            return Err(crate::GraphError::Provider(inconsistent(format!(
                "{} recovered edge rows were not materialized",
                edges.len()
            ))));
        }
        graph.edge_store.alive = Arc::new(edge_alive);
        graph.meta.next_edge_id = next_edge_id;

        // Re-register property indexes from SCMA. The empty TypedIndex placeholders
        // are filled by the downstream rebuild passes (`try_from_graph`) so the
        // registration sets survive restart even though entry contents are
        // derived from primary state.
        for (key, entry) in self.schemas {
            let target = match key.entity {
                SchemaEntityKind::Node => &mut graph.property_index,
                SchemaEntityKind::Edge => &mut graph.edge_property_index,
            };
            target.insert(
                (key.label, key.property),
                PropertyIndexEntry::new(TypedIndex::new(entry.kind), entry.name),
            );
        }
        for (key, entry) in self.composite_schemas {
            let properties = SmallVec::from_iter(key.properties);
            let kinds = SmallVec::from_iter(entry.kinds);
            let canonical_key = crate::graph::composite_property_key(&properties);
            graph.composite_property_index.insert(
                (key.label, canonical_key),
                CompositePropertyIndexEntry::new(
                    crate::CompositeTypedIndex::new(kinds),
                    properties,
                    entry.name,
                ),
            );
        }
        for (key, entry) in self.vector_schemas {
            graph.vector_index.insert(
                (key.label, key.property),
                VectorIndexEntry::new(
                    crate::VectorIndex::new_with_configs(
                        entry.kind,
                        entry.dimension,
                        entry.hnsw_config,
                        entry.ivf_config,
                    )?,
                    entry.name,
                ),
            );
        }
        for (key, entry) in self.text_schemas {
            graph.text_index.insert(
                (key.label.clone(), key.property.clone()),
                TextIndexEntry::new(crate::TextIndex::empty(key.label, key.property), entry.name),
            );
        }
        // Apply the post-snapshot WAL index intents to the registration set
        // only. `into_graph` deliberately does NOT rebuild index contents or
        // re-validate the closed-graph state here: the single rebuild + validate
        // site is `SharedGraph::from_graph_parts_and_snapshot`, which every
        // recovered graph flows through next (GRAPH-06 dedup). Doing it twice was
        // pure redundant work on the cold startup path.
        replay_property_index_changes(&mut graph, &self.pending_property_index_changes)?;
        replay_composite_property_index_changes(
            &mut graph,
            &self.pending_composite_property_index_changes,
        )?;
        replay_vector_index_changes(&mut graph, &self.pending_vector_index_changes)?;
        replay_text_index_changes(&mut graph, &self.pending_text_index_changes)?;
        Ok(graph)
    }
}

#[cfg(test)]
mod tests;
