//! Memory-only shared graph construction helpers.

use crate::{GraphError, GraphResult, GraphTypeDef, IndexProvider, SeleneGraph, SharedGraph};
use selene_core::GraphId;
use std::sync::Arc;

/// Builder for an in-memory [`SharedGraph`] and its fixed observer registry.
/// Durable database construction belongs to the `selene-db` facade.
pub struct SharedGraphBuilder {
    graph: SeleneGraph,
    providers: Vec<Arc<dyn IndexProvider>>,
}
impl SharedGraphBuilder {
    pub(super) fn new(graph_id: GraphId) -> Self {
        Self {
            graph: SeleneGraph::new(graph_id),
            providers: Vec::new(),
        }
    }
    /// Register an observer, retaining registration order for committed delivery.
    #[must_use]
    pub fn with_provider(mut self, provider: Arc<dyn IndexProvider>) -> Self {
        self.providers.push(provider);
        self
    }
    /// Bind the graph to a validated closed type.
    pub fn bound_to(mut self, type_def: GraphTypeDef) -> GraphResult<Self> {
        if self.graph.meta.bound_type.is_some() {
            return Err(GraphError::Inconsistent {
                reason: "graph builder is already bound to a graph type".into(),
            });
        }
        self.graph.meta.bound_type = Some(Arc::new(type_def.validate()?));
        Ok(self)
    }
    /// Build shared graph state, validating data, indexes and unique observer tags.
    pub fn build(self) -> GraphResult<SharedGraph> {
        SharedGraph::from_graph_with_providers(self.graph, self.providers)
    }
}
