//! Opaque process-local database identity and facade reference handles.

use std::sync::atomic::{AtomicU64, Ordering};

use selene_catalog::GraphId as LowerGraphId;

use crate::{
    CatalogReadSnapshot, Database, Error, ErrorKind, GraphDescriptor, GraphId, Result, Session,
};

static NEXT_DATABASE_ID: AtomicU64 = AtomicU64::new(1);

/// Opaque identity of one facade [`Database`] instance in this process.
///
/// This identity is neither persisted nor globally routable. Clones of one
/// database retain it, while each independent successful build receives a new
/// value. There is no public raw constructor or numeric accessor; debug output
/// is diagnostic and not a parsing contract.
///
/// ```compile_fail
/// let forged = selene_db::DatabaseId(7);
/// # let _ = forged;
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DatabaseId(u64);

impl DatabaseId {
    pub(crate) fn allocate() -> Self {
        let raw = NEXT_DATABASE_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .expect("process-local facade database identity space exhausted");
        Self(raw)
    }

    #[cfg(test)]
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Generation of one published graph state.
///
/// A graph generation is a cache and state token. It is not part of
/// [`GraphRef`], [`NodeRef`], or [`EdgeRef`] identity or validity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphGeneration(u64);

impl GraphGeneration {
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the generation number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the next generation, or `None` when the domain is exhausted.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Opaque, non-durable reference to one graph in a facade database instance.
///
/// Equality and hashing use only [`DatabaseId`] and stable [`GraphId`]. The
/// handle has no public raw constructor and intentionally implements no
/// serialization contract.
///
/// ```compile_fail
/// fn forge(database: selene_db::DatabaseId, graph: selene_db::GraphId) -> selene_db::GraphRef {
///     selene_db::GraphRef { database, graph }
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphRef {
    database: DatabaseId,
    graph: GraphId,
}

impl GraphRef {
    pub(crate) const fn new(database: DatabaseId, graph: GraphId) -> Self {
        Self { database, graph }
    }

    /// Return the owning process-local database identity.
    #[must_use]
    pub const fn database_id(self) -> DatabaseId {
        self.database
    }

    /// Return the stable graph identity carried by this reference.
    #[must_use]
    pub const fn graph_id(self) -> GraphId {
        self.graph
    }
}

/// Opaque, non-durable reference to one node in a facade database instance.
///
/// Equality and hashing use only database, graph, and stable node identity.
/// Generation is deliberately absent. Construction is limited to
/// [`Session::node_reference`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeRef {
    database: DatabaseId,
    graph: GraphId,
    node: crate::NodeId,
}

impl NodeRef {
    pub(crate) const fn new(database: DatabaseId, graph: GraphId, node: crate::NodeId) -> Self {
        Self {
            database,
            graph,
            node,
        }
    }

    /// Return the owning process-local database identity.
    #[must_use]
    pub const fn database_id(self) -> DatabaseId {
        self.database
    }

    /// Return the stable graph identity carried by this reference.
    #[must_use]
    pub const fn graph_id(self) -> GraphId {
        self.graph
    }

    /// Return the stable node identity carried by this reference.
    #[must_use]
    pub const fn node_id(self) -> crate::NodeId {
        self.node
    }
}

/// Opaque, non-durable reference to one edge in a facade database instance.
///
/// Equality and hashing use only database, graph, and stable edge identity.
/// Generation is deliberately absent. Construction is limited to
/// [`Session::edge_reference`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EdgeRef {
    database: DatabaseId,
    graph: GraphId,
    edge: crate::EdgeId,
}

impl EdgeRef {
    pub(crate) const fn new(database: DatabaseId, graph: GraphId, edge: crate::EdgeId) -> Self {
        Self {
            database,
            graph,
            edge,
        }
    }

    /// Return the owning process-local database identity.
    #[must_use]
    pub const fn database_id(self) -> DatabaseId {
        self.database
    }

    /// Return the stable graph identity carried by this reference.
    #[must_use]
    pub const fn graph_id(self) -> GraphId {
        self.graph
    }

    /// Return the stable edge identity carried by this reference.
    #[must_use]
    pub const fn edge_id(self) -> crate::EdgeId {
        self.edge
    }
}

impl Database {
    /// Return this facade instance's opaque process-local identity.
    #[must_use]
    pub fn id(&self) -> DatabaseId {
        self.inner.database_id
    }

    /// Issue a graph reference for the live graph at `path`.
    ///
    /// # Errors
    ///
    /// Returns a catalog diagnostic when `path` is not a live graph. A graph
    /// concurrently dropped after path resolution is rejected as runtime
    /// invalid-reference `42002`.
    pub fn graph_reference(&self, path: &crate::ObjectPath) -> Result<GraphRef> {
        let descriptor = self.catalog().snapshot().resolve_graph(path)?;
        let reference = GraphRef::new(self.id(), descriptor.id);
        self.resolve_graph_reference(reference)?;
        Ok(reference)
    }

    /// Validate and resolve a database-owned graph reference.
    ///
    /// # Errors
    ///
    /// Returns runtime invalid-reference `42002` when the reference belongs to
    /// another database instance or its graph is no longer live.
    pub fn resolve_graph_reference(&self, reference: GraphRef) -> Result<GraphDescriptor> {
        self.inner.resolve_graph_reference(reference)
    }
}

impl Session {
    /// Return the owning facade database's process-local identity.
    #[must_use]
    pub fn database_id(&self) -> DatabaseId {
        self.inner.database_id
    }

    /// Issue a reference to this session's live selected graph.
    ///
    /// # Errors
    ///
    /// Returns runtime invalid-reference `42002` if the selected graph was
    /// dropped or replaced.
    pub fn graph_reference(&self) -> Result<GraphRef> {
        let reference = GraphRef::new(self.database_id(), self.context.current_graph().id);
        self.resolve_graph_reference(reference)?;
        Ok(reference)
    }

    /// Validate and resolve a graph reference against this session selection.
    ///
    /// # Errors
    ///
    /// Returns runtime invalid-reference `42002` for a wrong database, wrong
    /// selected graph, dropped graph, or drop/recreate reference.
    pub fn resolve_graph_reference(&self, reference: GraphRef) -> Result<GraphDescriptor> {
        self.ensure_selected(reference.database, reference.graph)?;
        self.inner.resolve_graph_reference(reference)
    }

    /// Return the current state generation of this session's live selected graph.
    ///
    /// # Errors
    ///
    /// Returns runtime invalid-reference `42002` if the selected graph is no
    /// longer live.
    pub fn graph_generation(&self) -> Result<GraphGeneration> {
        let graph = self.context.current_graph();
        self.inner.with_reference_graph(graph.id, |runtime| {
            Ok(GraphGeneration::from_raw(runtime.read().meta.generation))
        })
    }

    /// Issue a reference to a committed live node in the selected graph.
    ///
    /// # Errors
    ///
    /// Returns `22G11` when `node` is deleted or absent, or ownership-domain
    /// invalid-reference `42002` when the selected graph is no longer live.
    pub fn node_reference(&self, node: crate::NodeId) -> Result<NodeRef> {
        let reference = NodeRef::new(self.database_id(), self.context.current_graph().id, node);
        self.resolve_node_reference(reference)?;
        Ok(reference)
    }

    /// Validate and resolve a node reference against this session selection.
    ///
    /// # Errors
    ///
    /// Returns `42002` for wrong ownership or a wrong selected graph, and
    /// `22G11` when the node referent is deleted or absent.
    pub fn resolve_node_reference(&self, reference: NodeRef) -> Result<crate::NodeId> {
        self.ensure_selected(reference.database, reference.graph)?;
        self.inner.with_reference_graph(reference.graph, |runtime| {
            if runtime.read().is_node_alive(reference.node) {
                Ok(reference.node)
            } else {
                Err(crate::value::deleted_reference())
            }
        })
    }

    /// Issue a reference to a committed live edge in the selected graph.
    ///
    /// # Errors
    ///
    /// Returns `22G11` when `edge` is deleted or absent, or ownership-domain
    /// invalid-reference `42002` when the selected graph is no longer live.
    pub fn edge_reference(&self, edge: crate::EdgeId) -> Result<EdgeRef> {
        let reference = EdgeRef::new(self.database_id(), self.context.current_graph().id, edge);
        self.resolve_edge_reference(reference)?;
        Ok(reference)
    }

    /// Validate and resolve an edge reference against this session selection.
    ///
    /// # Errors
    ///
    /// Returns `42002` for wrong ownership or a wrong selected graph, and
    /// `22G11` when the edge referent is deleted or absent.
    pub fn resolve_edge_reference(&self, reference: EdgeRef) -> Result<crate::EdgeId> {
        self.ensure_selected(reference.database, reference.graph)?;
        self.inner.with_reference_graph(reference.graph, |runtime| {
            if runtime.read().is_edge_alive(reference.edge) {
                Ok(reference.edge)
            } else {
                Err(crate::value::deleted_reference())
            }
        })
    }

    pub(crate) fn ensure_selected(&self, database: DatabaseId, graph: GraphId) -> Result<()> {
        if database != self.database_id() {
            return Err(Error::invalid_runtime_reference(
                "reference belongs to another database instance",
            ));
        }
        if graph != self.context.current_graph().id {
            return Err(Error::invalid_runtime_reference(
                "reference belongs to another graph",
            ));
        }
        Ok(())
    }
}

impl crate::database::DatabaseInner {
    fn resolve_graph_reference(&self, reference: GraphRef) -> Result<GraphDescriptor> {
        if reference.database != self.database_id {
            return Err(Error::invalid_runtime_reference(
                "reference belongs to another database instance",
            ));
        }
        self.with_reference_graph(reference.graph, |_| {
            let snapshot = CatalogReadSnapshot {
                state: self.state.load_full(),
            };
            snapshot
                .graph_by_id(reference.graph)?
                .ok_or_else(|| Error::catalog_invariant("live graph has no catalog descriptor"))
        })
    }

    pub(crate) fn with_reference_graph<T>(
        &self,
        graph: GraphId,
        inspect: impl FnOnce(&selene_graph::SharedGraph) -> Result<T>,
    ) -> Result<T> {
        let lower = LowerGraphId::new(graph.get()).map_err(|_| {
            Error::invalid_runtime_reference("reference carries an invalid graph identity")
        })?;
        self.with_graph_request(lower, &"runtime reference", inspect)
            .map_err(|error| {
                if error.kind() == ErrorKind::StaleSessionReference {
                    Error::invalid_runtime_reference("referenced graph is no longer live")
                } else {
                    error
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::GraphGeneration;

    #[test]
    fn graph_generation_transition_is_checked() {
        assert_eq!(
            GraphGeneration::from_raw(41).checked_next(),
            Some(GraphGeneration::from_raw(42))
        );
        assert_eq!(GraphGeneration::from_raw(u64::MAX).checked_next(), None);
    }
}
