//! Metadata, node, and edge snapshot sections for the core graph provider.

use std::collections::HashSet;
use std::sync::Arc;

use rkyv::{
    Place,
    rancor::Fallible,
    ser::{Allocator, Writer},
    vec::{ArchivedVec, VecResolver},
    with::{ArchiveWith, DeserializeWith, SerializeWith},
};
use selene_core::{DbString, EdgeId, LabelSet, NodeId, PropertyMap};
use serde::{Deserialize, Serialize};

use crate::{
    core_provider::inconsistent,
    graph::{GraphMeta, SeleneGraph},
};

use super::codec::{
    decode_properties_blob, decode_rkyv, encode_properties_blob, encode_rkyv, validate_id_unique,
};

struct ArcBytes;

impl ArchiveWith<Arc<[u8]>> for ArcBytes {
    type Archived = ArchivedVec<u8>;
    type Resolver = VecResolver;

    fn resolve_with(field: &Arc<[u8]>, resolver: Self::Resolver, out: Place<Self::Archived>) {
        ArchivedVec::resolve_from_slice(field.as_ref(), resolver, out);
    }
}

impl<S> SerializeWith<Arc<[u8]>, S> for ArcBytes
where
    S: Fallible + Allocator + Writer + ?Sized,
{
    fn serialize_with(field: &Arc<[u8]>, serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        ArchivedVec::serialize_from_slice(field.as_ref(), serializer)
    }
}

impl<D> DeserializeWith<ArchivedVec<u8>, Arc<[u8]>, D> for ArcBytes
where
    D: Fallible + ?Sized,
{
    fn deserialize_with(
        field: &ArchivedVec<u8>,
        _deserializer: &mut D,
    ) -> Result<Arc<[u8]>, D::Error> {
        Ok(Arc::from(field.as_slice()))
    }
}

/// Graph metadata section payload.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
pub struct MetaPayload {
    /// Graph identifier.
    pub graph_id: selene_core::GraphId,
    /// Published generation counter.
    pub generation: u64,
    /// Next node ID to allocate.
    pub next_node_id: u64,
    /// Next edge ID to allocate.
    pub next_edge_id: u64,
    /// Index into the `CORE/GTYP` table for closed graphs.
    pub bound_type_index: Option<u32>,
    /// Persistence sequence associated with the metadata payload.
    pub sequence: u64,
}

/// Serialized node-store row.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NodeRow {
    /// Node labels stored in the row.
    pub labels: LabelSet,
    /// Node properties stored in the row.
    pub properties: PropertyMap,
    /// Whether the row is live.
    pub alive: bool,
}

/// Serialized edge-store row.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EdgeRow {
    /// Intrinsic directionality of this edge, independent of row order.
    pub directionality: selene_core::EdgeDirectionality,
    /// Edge label.
    pub label: DbString,
    /// Source node ID, or first canonical undirected endpoint.
    pub source: NodeId,
    /// Target node ID, or second canonical undirected endpoint.
    pub target: NodeId,
    /// Edge properties.
    pub properties: PropertyMap,
    /// Whether the row is live.
    pub alive: bool,
}

#[derive(rkyv::Archive, rkyv::Deserialize, rkyv::Serialize)]
struct NodeArchiveRow {
    labels: LabelSet,
    #[rkyv(with = ArcBytes)]
    properties_blob: Arc<[u8]>,
    alive: bool,
}

impl NodeArchiveRow {
    fn from_parts(
        labels: LabelSet,
        properties: &PropertyMap,
        alive: bool,
        section: &'static str,
    ) -> Result<Self, crate::ProviderError> {
        Ok(Self {
            labels,
            properties_blob: encode_properties_blob(properties, section)?,
            alive,
        })
    }

    fn into_runtime(self, section: &'static str) -> Result<NodeRow, crate::ProviderError> {
        Ok(NodeRow {
            labels: self.labels,
            properties: decode_properties_blob(&self.properties_blob, section)?,
            alive: self.alive,
        })
    }
}

#[derive(rkyv::Archive, rkyv::Deserialize, rkyv::Serialize)]
struct EdgeArchiveRow {
    directionality: selene_core::EdgeDirectionality,
    label: DbString,
    source: NodeId,
    target: NodeId,
    #[rkyv(with = ArcBytes)]
    properties_blob: Arc<[u8]>,
    alive: bool,
}

impl EdgeArchiveRow {
    fn from_parts(
        label: DbString,
        directionality: selene_core::EdgeDirectionality,
        source: NodeId,
        target: NodeId,
        properties: &PropertyMap,
        alive: bool,
        section: &'static str,
    ) -> Result<Self, crate::ProviderError> {
        Ok(Self {
            label,
            directionality,
            source,
            target,
            properties_blob: encode_properties_blob(properties, section)?,
            alive,
        })
    }

    fn into_runtime(self, section: &'static str) -> Result<EdgeRow, crate::ProviderError> {
        Ok(EdgeRow {
            directionality: self.directionality,
            label: self.label,
            source: self.source,
            target: self.target,
            properties: decode_properties_blob(&self.properties_blob, section)?,
            alive: self.alive,
        })
    }
}

pub(in crate::core_provider) fn encode_meta(
    meta: &GraphMeta,
    sequence: u64,
) -> Result<Vec<u8>, crate::ProviderError> {
    encode_rkyv(
        &MetaPayload {
            graph_id: meta.graph_id,
            generation: meta.generation,
            next_node_id: meta.next_node_id,
            next_edge_id: meta.next_edge_id,
            bound_type_index: meta.bound_type.as_ref().map(|_| 0),
            sequence,
        },
        "CORE/META",
    )
}

pub(in crate::core_provider) fn decode_meta(
    bytes: &[u8],
) -> Result<MetaPayload, crate::ProviderError> {
    decode_rkyv(bytes, "CORE/META")
}

pub(in crate::core_provider) fn encode_nodes(
    graph: &SeleneGraph,
) -> Result<Vec<u8>, crate::ProviderError> {
    let mut rows = Vec::with_capacity(graph.node_store.len());
    for row_index in 0..graph.node_store.len() {
        let row = u32::try_from(row_index).map_err(|_| {
            inconsistent(format!(
                "node row index {row_index} exceeds u32::MAX; core snapshot sections use v1 row indexes"
            ))
        })?;
        let labels =
            graph.node_store.labels.get(row_index).ok_or_else(|| {
                inconsistent(format!("node labels column missing row {row_index}"))
            })?;
        let properties = graph.node_store.properties.get(row_index).ok_or_else(|| {
            inconsistent(format!("node properties column missing row {row_index}"))
        })?;
        // BRIEF-Item-4a STEP 9: persist the EXPLICIT external id from the
        // row_to_id column rather than synthesizing `row + 1`. Committed rows
        // (alive or deleted-but-kept under Option B) carry their real `NodeId`;
        // never-committed aborted-tx hole rows carry `NodeId::TOMBSTONE`, which
        // recovery skips (-> the id resolves NotFound, matching the live path).
        // This is the format change the SLSN minor-version bump guards: a future
        // 4b-compacted snapshot whose ids != row+1 round-trips because recovery
        // places rows by their stored position, not by `id - 1` arithmetic.
        let id = graph
            .node_store
            .row_to_id
            .get(row_index)
            .copied()
            .ok_or_else(|| {
                inconsistent(format!("node row_to_id column missing row {row_index}"))
            })?;
        rows.push((
            id,
            NodeArchiveRow::from_parts(
                labels.clone(),
                properties,
                graph.node_store.is_alive(row),
                "CORE/NODE",
            )?,
        ));
    }
    encode_rkyv(&rows, "CORE/NODE")
}

pub(in crate::core_provider) fn decode_nodes(
    bytes: &[u8],
) -> Result<Vec<(NodeId, NodeRow)>, crate::ProviderError> {
    let rows: Vec<(NodeId, NodeArchiveRow)> = decode_rkyv(bytes, "CORE/NODE")?;
    // BRIEF-Item-4a STEP 9: rows are no longer guaranteed sorted-ascending by id
    // (a 4b-compacted snapshot may store ids in any row order) and multiple
    // aborted-tx hole rows legitimately share `NodeId::TOMBSTONE`. Validate that
    // every *real* (non-tombstone) id is unique; row order is positional.
    let mut seen = HashSet::new();
    let mut runtime_rows = Vec::with_capacity(rows.len());
    for (id, row) in rows {
        validate_id_unique(&mut seen, id, NodeId::TOMBSTONE, "CORE/NODE")?;
        runtime_rows.push((id, row.into_runtime("CORE/NODE")?));
    }
    Ok(runtime_rows)
}

pub(in crate::core_provider) fn encode_edges(
    graph: &SeleneGraph,
) -> Result<Vec<u8>, crate::ProviderError> {
    let mut rows = Vec::with_capacity(graph.edge_store.len());
    for row_index in 0..graph.edge_store.len() {
        let row = u32::try_from(row_index).map_err(|_| {
            inconsistent(format!(
                "edge row index {row_index} exceeds u32::MAX; core snapshot sections use v1 row indexes"
            ))
        })?;
        let label =
            graph.edge_store.label.get(row_index).ok_or_else(|| {
                inconsistent(format!("edge label column missing row {row_index}"))
            })?;
        let source =
            graph.edge_store.source.get(row_index).ok_or_else(|| {
                inconsistent(format!("edge source column missing row {row_index}"))
            })?;
        let target =
            graph.edge_store.target.get(row_index).ok_or_else(|| {
                inconsistent(format!("edge target column missing row {row_index}"))
            })?;
        let properties = graph.edge_store.properties.get(row_index).ok_or_else(|| {
            inconsistent(format!("edge properties column missing row {row_index}"))
        })?;
        // BRIEF-Item-4a STEP 9: persist the explicit external id from the
        // row_to_id column (real `EdgeId`, or `EdgeId::TOMBSTONE` for a
        // never-committed hole row). See `encode_nodes` for the rationale.
        let id = graph
            .edge_store
            .row_to_id
            .get(row_index)
            .copied()
            .ok_or_else(|| {
                inconsistent(format!("edge row_to_id column missing row {row_index}"))
            })?;
        rows.push((
            id,
            EdgeArchiveRow::from_parts(
                label.clone(),
                graph
                    .edge_store
                    .directionality
                    .get(row_index)
                    .copied()
                    .ok_or_else(|| {
                        inconsistent(format!(
                            "edge directionality column missing row {row_index}"
                        ))
                    })?,
                *source,
                *target,
                properties,
                graph.edge_store.is_alive(row),
                "CORE/EDGE",
            )?,
        ));
    }
    encode_rkyv(&rows, "CORE/EDGE")
}

pub(in crate::core_provider) fn decode_edges(
    bytes: &[u8],
) -> Result<Vec<(EdgeId, EdgeRow)>, crate::ProviderError> {
    let rows: Vec<(EdgeId, EdgeArchiveRow)> = decode_rkyv(bytes, "CORE/EDGE")?;
    let mut seen = HashSet::new();
    let mut runtime_rows = Vec::with_capacity(rows.len());
    for (id, row) in rows {
        validate_id_unique(&mut seen, id, EdgeId::TOMBSTONE, "CORE/EDGE")?;
        runtime_rows.push((id, row.into_runtime("CORE/EDGE")?));
    }
    Ok(runtime_rows)
}
