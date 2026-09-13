//! Enforce the named catalog constraint without changing instance-local declarations.

use super::*;
use selene_catalog::{CatalogObjectId, CatalogPayload};
use selene_core::Change;

impl DatabaseDraft {
    fn named_type_for(&self, id: GraphId) -> Option<Arc<GraphTypeDef>> {
        let CatalogPayload::Graph {
            graph_type: Some(ty),
        } = self
            .catalog
            .descriptor(CatalogObjectId::Graph(id))?
            .payload()
        else {
            return None;
        };
        self.graph_types.get(ty).cloned()
    }

    pub(super) fn admit_named_prepared(&self, prepared: &mut PreparedGraphCommit) -> Result<()> {
        let id = GraphId::new(prepared.snapshot().graph_id().get())
            .map_err(Error::from_catalog_invariant)?;
        if let Some(named) = self.named_type_for(id)
            && !prepared.snapshot().named_constraints_match(&named)
        {
            prepared
                .admit_named_constraints(self.selected_graph()?, named)
                .map_err(named_error)?;
        }
        Ok(())
    }

    pub(super) fn admit_named_replacements(&mut self, base: &DatabaseState) -> Result<()> {
        let types: Vec<_> = self
            .graph_replacements
            .keys()
            .filter_map(|id| self.named_type_for(*id).map(|ty| (*id, ty)))
            .collect();
        for (id, named) in types {
            let replacement = self.graph_replacements.get_mut(&id).expect("replacement");
            if let DetachedGraphReplacement::Snapshot(graph) = replacement {
                let before = base.graphs.get(&id).map(|instance| instance.graph.read());
                graph
                    .admit_named_constraints(before.as_deref(), named, &[])
                    .map_err(named_error)?;
            }
        }
        Ok(())
    }
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
        if !graph.named_constraints_match(named) {
            selene_graph::type_validator::validate_entity_state(graph, named)
                .map_err(Error::named_type_violation)?;
        }
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

fn named_error(error: selene_graph::GraphError) -> Error {
    match error {
        selene_graph::GraphError::TypeViolation(violation) => {
            Error::named_type_violation(violation)
        }
        other => Error::invalid_graph_type_source(other),
    }
}
