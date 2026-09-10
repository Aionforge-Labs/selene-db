//! Allocation lifetime independent of a detached snapshot or its committer.

use crate::{GraphError, GraphResult, IdAllocator, SeleneGraph, SharedGraph};
use parking_lot::Mutex;
use selene_core::GraphId;
use std::sync::Arc;

/// Opaque, graph-scoped allocation authority shared by detached transactions.
///
/// Retains only monotonic counters, never graph data, a lifecycle lock, a WAL or
/// a committer. Allocated node/edge identities remain consumed after rollback.
/// This process-local capability is not serializable or a persisted identity.
#[doc(hidden)]
#[derive(Clone)]
pub struct GraphAllocationAuthority {
    graph: GraphId,
    allocator: Arc<Mutex<IdAllocator>>,
}

impl SharedGraph {
    /// Retain this graph's monotonic allocation authority across detached work.
    #[doc(hidden)]
    #[must_use]
    pub fn allocation_authority(&self) -> GraphAllocationAuthority {
        GraphAllocationAuthority {
            graph: self.read().graph_id(),
            allocator: Arc::clone(&self.allocator),
        }
    }

    /// Build an in-memory snapshot runtime sharing the supplied allocation domain.
    ///
    /// Counters advance to the snapshot's validated metadata/storage floors and
    /// never retreat. The fresh runtime has no writers before its allocator is
    /// attached; committers do not own or copy this allocation state.
    ///
    /// # Errors
    /// Returns an inconsistency for a different graph identity, or any ordinary
    /// snapshot validation error from [`Self::try_from_graph`].
    #[doc(hidden)]
    pub fn try_from_graph_with_allocation(
        graph: SeleneGraph,
        authority: &GraphAllocationAuthority,
    ) -> GraphResult<Self> {
        if graph.graph_id() != authority.graph {
            return Err(GraphError::Inconsistent {
                reason: "allocation authority belongs to another graph".into(),
            });
        }
        let mut shared = Self::try_from_graph(graph)?;
        let floor = shared.allocator.lock().clone();
        authority.allocator.lock().raise_to(&floor);
        shared.allocator = Arc::clone(&authority.allocator);
        Ok(shared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selene_core::{LabelSet, PropertyMap};

    #[test]
    fn detached_authority_retains_burned_ids_and_raises_snapshot_floors() {
        let graph = SharedGraph::new(GraphId::new(51));
        let authority = graph.allocation_authority();
        let mut snapshot = graph.read().as_ref().clone();
        snapshot.meta.next_node_id = 100;
        snapshot.meta.next_edge_id = 200;
        let detached =
            SharedGraph::try_from_graph_with_allocation(snapshot.clone(), &authority).unwrap();
        let allocate = |runtime: &SharedGraph| {
            let mut tx = runtime.begin_write();
            let mut mutation = tx.mutator();
            let node = mutation
                .create_node(LabelSet::new(), PropertyMap::new())
                .unwrap();
            let edge = mutation
                .create_edge(
                    selene_core::db_string("E").unwrap(),
                    node,
                    node,
                    PropertyMap::new(),
                )
                .unwrap();
            (node.get(), edge.get())
        };
        assert_eq!(allocate(&detached), (100, 200));
        assert_eq!(allocate(&graph), (101, 201));
        drop(detached);
        drop(graph);
        let reopened = SharedGraph::try_from_graph_with_allocation(snapshot, &authority).unwrap();
        assert_eq!(allocate(&reopened), (102, 202));
        assert_eq!(reopened.read().node_count(), 0);
    }

    #[test]
    fn detached_authority_rejects_another_graph() {
        let graph = SharedGraph::new(GraphId::new(52));
        assert!(
            SharedGraph::try_from_graph_with_allocation(
                SeleneGraph::new(GraphId::new(53)),
                &graph.allocation_authority()
            )
            .is_err()
        );
    }
}
