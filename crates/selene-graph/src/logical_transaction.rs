//! Complete format-2 logical transaction composition and isolated replay.
//!
//! This is not a live publication authority or durable commit. Isolated replay
//! candidates require eager runtime materialization and the facade's frozen native
//! declaration admission before a recovered database is opened.

use crate::SeleneGraph;
use selene_catalog::{
    CatalogLogicalRecords, CatalogObjectId, CatalogParent, CatalogPayload, GraphTypeId,
    codec::CatalogDelta,
};
use selene_core::{
    DbString, GraphId, NodeId, Value,
    logical::{
        Budget, CodecError as E, CodecResult, Decoder, Encoder, GraphDefinition, GraphDelta, Limits,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

mod checkpoint;
mod named_types;
mod producer;
mod runtime;
mod schema;
pub use checkpoint::encode_checkpoint;
pub use producer::graph_delta;
pub use runtime::ReconstructedRuntime;
pub use schema::definition;

/// Failure of complete frame verification or semantic transaction validation.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// Framing, lineage, compression, or sealed/interior corruption.
    #[error(transparent)]
    Frame(#[from] selene_persist::logical_frame::FrameError),
    /// Logical representation, resource, or owning semantic validation failure.
    #[error(transparent)]
    Payload(#[from] E),
}

/// Whole-frame result. An incomplete suffix returns no candidate and performs no repair.
pub enum FrameCandidate {
    /// A complete transaction was validated against the prior isolated state.
    Complete {
        /// Entire catalog and graph candidate, never partially published.
        state: ReplayState,
        /// Digest to require in the next frame's lineage context.
        digest: [u8; 32],
    },
    /// An explicitly unsealed final input is incomplete.
    Incomplete {
        /// Bytes needed to continue frame validation.
        needed: usize,
    },
}

/// A named catalog graph-type creation/replacement/removal, paired with catalog revisions.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeDelta {
    /// Stable catalog type identity, never a positional type index.
    pub id: GraphTypeId,
    /// Complete resulting definition; absent only on catalog drop.
    pub definition: Option<GraphDefinition>,
}

/// One atomic unit: catalog changes, named types and every touched graph.
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalTransaction {
    /// Complete catalog revision delta and allocator high water.
    pub catalog: CatalogDelta,
    /// Touched named graph types, in strictly increasing identity order.
    pub graph_types: Vec<TypeDelta>,
    /// Touched graphs, in strictly increasing identity order.
    pub graphs: Vec<GraphDelta>,
}

impl LogicalTransaction {
    /// Encode version-one semantic body for a format-2 frame. No legacy enum serde.
    pub fn encode(&self, limits: Limits) -> CodecResult<Vec<u8>> {
        let mut e = Encoder::new(limits)?;
        e.u32(1)?;
        self.catalog.encode(&mut e)?;
        e.count(self.graph_types.len())?;
        if self.graph_types.windows(2).any(|p| p[0].id >= p[1].id) {
            return Err(E::Invalid("type identity order"));
        }
        for ty in &self.graph_types {
            e.u64(ty.id.get())?;
            e.boolean(ty.definition.is_some())?;
            if let Some(def) = &ty.definition {
                e.graph_definition(def)?;
            }
        }
        if self.graphs.windows(2).any(|p| p[0].id >= p[1].id) {
            return Err(E::Invalid("graph identity order"));
        }
        e.count(self.graphs.len())?;
        for graph in &self.graphs {
            graph.encode(&mut e)?;
        }
        Ok(e.finish())
    }

    /// Decode the entire body, including final fields and trailing-byte checks, before apply.
    pub fn decode(bytes: &[u8], limits: Limits) -> CodecResult<Self> {
        let mut budget = Budget::new(limits)?;
        Self::decode_budget(bytes, &mut budget)
    }
    /// Decode with conservative cumulative allocation bytes and semantic item charges.
    /// These are enforced accounting units, not allocator callback measurements.
    pub fn decode_accounted(bytes: &[u8], limits: Limits) -> CodecResult<(Self, usize, usize)> {
        let mut budget = Budget::new(limits)?;
        let transaction = Self::decode_budget(bytes, &mut budget)?;
        Ok((
            transaction,
            budget.allocation_charge(),
            budget.item_charge(),
        ))
    }
    fn decode_budget(bytes: &[u8], budget: &mut Budget) -> CodecResult<Self> {
        let mut d = Decoder::new(bytes, budget)?;
        if d.u32()? != 1 {
            return Err(E::Unsupported("logical body version"));
        }
        let catalog = CatalogDelta::decode(&mut d)?;
        let count = d.count()?;
        let mut graph_types: Vec<TypeDelta> = Vec::with_capacity(count);
        for _ in 0..count {
            let id = GraphTypeId::new(d.u64()?).map_err(|_| E::Semantic)?;
            if graph_types.last().is_some_and(|p| p.id >= id) {
                return Err(E::Invalid("type identity order"));
            }
            graph_types.push(TypeDelta {
                id,
                definition: if d.boolean()? {
                    Some(d.graph_definition()?)
                } else {
                    None
                },
            });
        }
        let count = d.count()?;
        let mut graphs: Vec<GraphDelta> = Vec::with_capacity(count);
        for _ in 0..count {
            let delta = GraphDelta::decode(&mut d)?;
            if graphs.last().is_some_and(|p| p.id >= delta.id) {
                return Err(E::Invalid("graph identity order"));
            }
            graphs.push(delta);
        }
        d.finish()?;
        Ok(Self {
            catalog,
            graph_types,
            graphs,
        })
    }
}

/// Isolated validated logical state. Not query-ready, not a writer and not a durability proof.
#[derive(Clone)]
pub struct ReplayState {
    catalog: CatalogLogicalRecords,
    graph_types: BTreeMap<GraphTypeId, Arc<GraphDefinition>>,
    graphs: BTreeMap<GraphId, Arc<SeleneGraph>>,
    backing_indexes: BTreeMap<GraphId, Arc<[u64]>>,
}

impl ReplayState {
    /// Verify exactly one full frame before any logical decode/apply. Additional bytes
    /// are rejected: stream owners slice at the framing decoder's consumed boundary.
    pub fn apply_frame(
        &self,
        bytes: &[u8],
        context: selene_persist::logical_frame::Context,
        boundary: selene_persist::logical_frame::Boundary,
        limits: Limits,
    ) -> Result<FrameCandidate, ReplayError> {
        use selene_persist::logical_frame::{self, Decoded};
        limits.validate()?;
        match logical_frame::decode(bytes, context, boundary, limits.bytes)? {
            Decoded::Incomplete { needed } => Ok(FrameCandidate::Incomplete { needed }),
            Decoded::Complete {
                body,
                digest,
                consumed,
            } => {
                if consumed != bytes.len() {
                    return Err(E::Invalid("trailing frame bytes").into());
                }
                let state = self.apply_body(&body, limits)?;
                Ok(FrameCandidate::Complete { state, digest })
            }
        }
    }
    /// Start from a catalog seed with no graphs/types. Native declarations remain metadata.
    pub fn seed(catalog: CatalogLogicalRecords) -> CodecResult<Self> {
        selene_catalog::codec::account_records(&catalog, &mut Budget::new(Limits::default())?)?;
        let state = Self {
            catalog,
            graph_types: BTreeMap::new(),
            graphs: BTreeMap::new(),
            backing_indexes: BTreeMap::new(),
        };
        state.validate_coverage()?;
        Ok(state)
    }
    /// Borrow validated catalog metadata, without runtime-activation claims.
    pub fn catalog(&self) -> &CatalogLogicalRecords {
        &self.catalog
    }
    /// Inspect an isolated graph's logical counts and allocation high water.
    pub fn graph_summary(&self, id: GraphId) -> Option<(usize, usize, u64, u64)> {
        self.graphs.get(&id).map(|g| {
            (
                g.node_count(),
                g.edge_count(),
                g.meta.next_node_id,
                g.meta.next_edge_id,
            )
        })
    }
    /// Inspect a primary node property without invoking a derived index.
    pub fn node_property(&self, graph: GraphId, node: NodeId, key: &DbString) -> Option<&Value> {
        self.graphs.get(&graph)?.node_properties(node)?.get(key)
    }
    /// Decode and validate all catalog/types/graphs, then return a new isolated candidate.
    /// `self` is never mutated, including when the final graph or field fails.
    pub fn apply_body(&self, bytes: &[u8], limits: Limits) -> CodecResult<Self> {
        let mut budget = Budget::new(limits)?;
        let transaction = LogicalTransaction::decode_budget(bytes, &mut budget)?;
        self.apply(&transaction, &mut budget)
    }

    fn apply(&self, transaction: &LogicalTransaction, budget: &mut Budget) -> CodecResult<Self> {
        selene_catalog::codec::account_records(&self.catalog, budget)?;
        let retained = self
            .graphs
            .len()
            .checked_add(self.graph_types.len())
            .ok_or(E::Limit)?;
        budget.charge(retained, retained.checked_mul(512).ok_or(E::Limit)?)?;
        let catalog = transaction.catalog.apply(&self.catalog)?;
        let snapshot = catalog.reconstruct().map_err(|_| E::Semantic)?;
        let old = self.catalog.reconstruct().map_err(|_| E::Semantic)?;
        named_types::require_changed_bodies(transaction, budget)?;
        let mut named_runtime = BTreeMap::new();
        let mut candidate = Self {
            catalog,
            graph_types: self.graph_types.clone(),
            graphs: self.graphs.clone(),
            backing_indexes: self.backing_indexes.clone(),
        };
        for ty in &transaction.graph_types {
            let descriptor = snapshot.descriptor(CatalogObjectId::GraphType(ty.id));
            match (&ty.definition, descriptor) {
                (Some(definition), Some(descriptor)) => {
                    named_types::check_name(definition, descriptor)?;
                    named_runtime.insert(ty.id, named_types::materialize(definition, budget)?);
                    if old.descriptor(CatalogObjectId::GraphType(ty.id)) == Some(descriptor)
                        && self.graph_types.get(&ty.id).map(AsRef::as_ref) != Some(definition)
                    {
                        return Err(E::Admission("type change without catalog revision"));
                    }
                    candidate
                        .graph_types
                        .insert(ty.id, Arc::new(definition.clone()));
                }
                (None, None) if candidate.graph_types.remove(&ty.id).is_some() => {}
                _ => return Err(E::Admission("type/catalog disagreement")),
            }
        }
        let touched: BTreeSet<_> = transaction.graphs.iter().map(|g| g.id).collect();
        for descriptor in old.descriptors() {
            if let CatalogObjectId::Graph(id) = descriptor.id()
                && snapshot.descriptor(descriptor.id()).is_none()
            {
                let id = GraphId::new(id.get());
                let graph = candidate.graphs.remove(&id).ok_or(E::Semantic)?;
                candidate.backing_indexes.remove(&id);
                if graph.node_count() != 0 || graph.edge_count() != 0 || touched.contains(&id) {
                    return Err(E::Admission("nonempty graph drop"));
                }
            }
        }
        for delta in &transaction.graphs {
            let catalog_id =
                selene_catalog::GraphId::new(delta.id.get()).map_err(|_| E::Semantic)?;
            if snapshot
                .descriptor(CatalogObjectId::Graph(catalog_id))
                .is_none()
            {
                return Err(E::Admission("missing graph owner"));
            }
            let original = self.graphs.get(&delta.id);
            if original.map(|g| g.meta.generation) != delta.previous
                || delta
                    .previous
                    .is_some_and(|previous| delta.generation <= previous)
            {
                return Err(E::Admission("graph generation"));
            }
            let bound = delta
                .definition
                .as_ref()
                .map(schema::materialize)
                .transpose()?
                .map(Arc::new);
            let graph =
                graph_apply::logical_graph(original.map(AsRef::as_ref), delta, bound, budget)?;
            graph
                .validate_logical_catalog(&snapshot, &delta.backing_indexes)
                .map_err(|_| E::Semantic)?;
            candidate
                .backing_indexes
                .insert(delta.id, delta.backing_indexes.clone().into());
            candidate.graphs.insert(delta.id, Arc::new(graph));
        }
        // Changed declarations require their owner graph in this same atomic unit.
        for descriptor in old.descriptors().chain(snapshot.descriptors()) {
            if descriptor.payload().declaration_metadata().is_some()
                && old.descriptor(descriptor.id()) != snapshot.descriptor(descriptor.id())
                && let CatalogParent::Graph(id) = descriptor.parent()
                && candidate.graphs.contains_key(&GraphId::new(id.get()))
                && !touched.contains(&GraphId::new(id.get()))
            {
                return Err(E::Admission("declaration without graph payload"));
            }
        }
        candidate.validate_coverage()?;
        named_types::validate_bindings(
            &candidate,
            &old,
            &snapshot,
            transaction,
            &mut named_runtime,
            budget,
        )?;
        Ok(candidate)
    }

    fn validate_coverage(&self) -> CodecResult<()> {
        let snapshot = self.catalog.reconstruct().map_err(|_| E::Semantic)?;
        let graph_ids: BTreeSet<_> = snapshot
            .descriptors()
            .filter_map(|d| match d.id() {
                CatalogObjectId::Graph(id) => Some(GraphId::new(id.get())),
                _ => None,
            })
            .collect();
        let type_ids: BTreeSet<_> = snapshot
            .descriptors()
            .filter_map(|d| match d.id() {
                CatalogObjectId::GraphType(id) => Some(id),
                _ => None,
            })
            .collect();
        if graph_ids != self.graphs.keys().copied().collect()
            || graph_ids != self.backing_indexes.keys().copied().collect()
            || type_ids != self.graph_types.keys().copied().collect()
        {
            return Err(E::Admission("missing or extra catalog graph/type payload"));
        }
        for descriptor in snapshot.descriptors() {
            if let CatalogPayload::Graph {
                graph_type: Some(id),
            } = descriptor.payload()
                && !self.graph_types.contains_key(id)
            {
                return Err(E::Admission("missing constraining type"));
            }
        }
        Ok(())
    }
}

pub(crate) mod graph_apply;

#[cfg(test)]
mod tests;
