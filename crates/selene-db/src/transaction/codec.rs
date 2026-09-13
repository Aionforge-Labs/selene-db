//! Pure codec preparation used before the authoritative format-2 append.

use super::{DatabaseDraft, DatabaseState};
use selene_catalog::{
    CatalogLogicalRecords, CatalogObjectId, CatalogObjectKind as K, codec::CatalogDelta,
};
use selene_core::logical::{CodecError as E, CodecResult};
use selene_graph::logical_transaction::{LogicalTransaction, TypeDelta, definition, graph_delta};
use std::sync::Arc;

impl DatabaseDraft {
    /// Build the complete transaction from real detached inputs without publishing,
    /// appending, syncing, notifying providers, or changing the facade outcome contract.
    pub(crate) fn logical_transaction(
        &self,
        base: &Arc<DatabaseState>,
    ) -> CodecResult<LogicalTransaction> {
        if !self.matches_base(base) {
            return Err(E::Invalid("database draft base"));
        }
        let water = self.high_water;
        let records = CatalogLogicalRecords::new(
            self.catalog.generation(),
            std::collections::BTreeMap::from([
                (K::Catalog, self.catalog.catalog_id().get()),
                (K::Directory, self.catalog.root_directory_id().get()),
                (K::Schema, water.schema),
                (K::Graph, water.graph),
                (K::GraphType, water.graph_type),
                (K::Index, water.index),
                (K::Constraint, water.constraint),
                (K::Procedure, water.procedure),
            ]),
            self.catalog.descriptors().cloned().collect(),
        )
        .map_err(|_| E::Semantic)?;
        let catalog = CatalogDelta::between(&base.catalog, &records)?;
        let mut graph_types = Vec::new();
        for (id, old) in &base.graph_types {
            if !self.graph_types.contains_key(id) {
                graph_types.push(TypeDelta {
                    id: *id,
                    definition: None,
                });
            } else if self.graph_types.get(id).is_some_and(|next| next != old)
                || self.catalog.descriptor(CatalogObjectId::GraphType(*id))
                    != base.catalog.descriptor(CatalogObjectId::GraphType(*id))
            {
                graph_types.push(TypeDelta {
                    id: *id,
                    definition: Some(definition(&self.graph_types[id])?),
                });
            }
        }
        for (id, next) in &self.graph_types {
            if !base.graph_types.contains_key(id) {
                graph_types.push(TypeDelta {
                    id: *id,
                    definition: Some(definition(next)?),
                });
            }
        }
        graph_types.sort_by_key(|ty| ty.id);
        let mut graphs = Vec::new();
        for (id, replacement) in &self.graph_replacements {
            // Replacements are bound during staging and rechecked by the outer
            // authority before encoding. Do not rebuild complete backing for a
            // data-only transaction just to extract its logical identities.
            let next = replacement.snapshot();
            let original = base.graphs.get(id).map(|instance| instance.graph.read());
            let changes = self
                .logical_changes
                .get(id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            // Snapshot-only catalog replacements currently preserve data; new graph
            // creation is empty. Reject an unrepresented future import path, not data.
            if changes.is_empty()
                && original.is_none()
                && (next.node_count() != 0 || next.edge_count() != 0)
            {
                return Err(E::Invalid("new graph data requires logical changes"));
            }
            graphs.push(graph_delta(original.as_deref(), next, changes)?);
        }
        Ok(LogicalTransaction {
            catalog,
            graph_types,
            graphs,
        })
    }
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
