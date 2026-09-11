//! Enforce the named catalog constraint without changing instance-local declarations.

use super::*;
use selene_catalog::{CatalogObjectId, CatalogPayload};
use selene_core::Change;

impl DatabaseDraft {
    pub(super) fn validate_named_graph(
        &self,
        graph: &SeleneGraph,
        changes: &[Change],
    ) -> Result<()> {
        let id = GraphId::new(graph.graph_id().get()).map_err(Error::from_catalog_invariant)?;
        let descriptor = self
            .catalog
            .descriptor(CatalogObjectId::Graph(id))
            .ok_or_else(|| Error::catalog_invariant("named validation graph owner missing"))?;
        let CatalogPayload::Graph {
            graph_type: Some(type_id),
        } = descriptor.payload()
        else {
            return Ok(());
        };
        let type_descriptor = self
            .catalog
            .descriptor(CatalogObjectId::GraphType(*type_id))
            .ok_or_else(|| Error::catalog_invariant("named graph type descriptor missing"))?;
        let named = self
            .graph_types
            .get(type_id)
            .ok_or_else(|| Error::catalog_invariant("named graph type body missing"))?;
        if descriptor.parent() != type_descriptor.parent()
            || named.name.as_str() != type_descriptor.name().display()
            || graph
                .meta
                .bound_type
                .as_ref()
                .is_none_or(|instance| instance.name != named.name)
        {
            return Err(Error::catalog_invariant(
                "named graph type binding identity mismatch",
            ));
        }
        // Deliberately a full scan of this affected graph, NOT O(delta). The same
        // owning validators are used by isolated replay. Unused local types remain.
        selene_graph::type_validator::validate_entity_state(graph, named)
            .map_err(Error::named_type_violation)?;
        for change in changes {
            selene_graph::type_validator::validate_change(change, graph, named)
                .map_err(Error::named_type_violation)?;
        }
        Ok(())
    }

    pub(super) fn validate_named_replacements(&self, base: &DatabaseState) -> Result<()> {
        for descriptor in self.catalog.descriptors() {
            let CatalogObjectId::Graph(id) = descriptor.id() else {
                continue;
            };
            let changed_type = match descriptor.payload() {
                CatalogPayload::Graph {
                    graph_type: Some(ty),
                } => {
                    base.graph_types.get(ty) != self.graph_types.get(ty)
                        || base.catalog.descriptor(CatalogObjectId::GraphType(*ty))
                            != self.catalog.descriptor(CatalogObjectId::GraphType(*ty))
                }
                _ => false,
            };
            if let Some(replacement) = self.graph_replacements.get(&id) {
                self.validate_named_graph(
                    replacement.snapshot(),
                    self.logical_changes.get(&id).map_or(&[], Vec::as_slice),
                )?;
            } else if changed_type || base.catalog.descriptor(descriptor.id()) != Some(descriptor) {
                let instance = base.graphs.get(&id).ok_or_else(|| {
                    Error::catalog_invariant("changed graph binding lacks runtime")
                })?;
                self.validate_named_graph(&instance.graph.read(), &[])?;
            }
            // Unchanged immutable graph/type/binding triples were already proved.
        }
        Ok(())
    }
}
