//! Per-graph ID allocator per D11.
//!
//! The allocator is owned by `SharedGraph`, not by individual transactions.
//! Advancing a counter is permanent even when the transaction later rolls back,
//! which preserves spec 02 section 4's no-reuse identity rule.

use selene_core::{EdgeId, NodeId};

use crate::graph::GraphMeta;
use crate::{GraphError, GraphResult};

/// Per-graph node and edge ID allocator.
#[derive(Clone, Debug)]
pub struct IdAllocator {
    next_node_id: u64,
    next_edge_id: u64,
}

impl IdAllocator {
    /// Construct an allocator at the v1.0 initial checkpoint.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_node_id: 1,
            next_edge_id: 1,
        }
    }

    /// Restore allocator counters from graph metadata, never falling below the
    /// supplied storage floors.
    ///
    /// Recovery uses `(node_store_len + 1, edge_store_len + 1)` as the floors so
    /// that stale metadata cannot allow ID reuse over already-populated rows.
    /// Identity invariants in spec 02 §4 require allocators to monotonically
    /// advance past every row that ever existed.
    #[must_use]
    pub fn from_meta_with_floors(meta: &GraphMeta, node_floor: u64, edge_floor: u64) -> Self {
        Self {
            next_node_id: meta.next_node_id.max(node_floor).max(1),
            next_edge_id: meta.next_edge_id.max(edge_floor).max(1),
        }
    }

    /// Allocate a node ID and advance the permanent high-water mark.
    ///
    /// # Errors
    /// Returns [`GraphError::CounterExhausted`] before the high-water mark can wrap.
    pub fn allocate_node(&mut self) -> GraphResult<NodeId> {
        let id = self.next_node_id;
        self.next_node_id =
            self.next_node_id
                .checked_add(1)
                .ok_or(GraphError::CounterExhausted {
                    kind: "node identity",
                })?;
        Ok(NodeId::new(id))
    }

    /// Allocate an edge ID and advance the permanent high-water mark.
    ///
    /// # Errors
    /// Returns [`GraphError::CounterExhausted`] before the high-water mark can wrap.
    pub fn allocate_edge(&mut self) -> GraphResult<EdgeId> {
        let id = self.next_edge_id;
        self.next_edge_id =
            self.next_edge_id
                .checked_add(1)
                .ok_or(GraphError::CounterExhausted {
                    kind: "edge identity",
                })?;
        Ok(EdgeId::new(id))
    }

    /// Return the next node ID without allocating it.
    #[must_use]
    pub const fn peek_next_node(&self) -> u64 {
        self.next_node_id
    }

    /// Return the next edge ID without allocating it.
    #[must_use]
    pub const fn peek_next_edge(&self) -> u64 {
        self.next_edge_id
    }
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selene_core::GraphId;

    #[test]
    fn allocate_node_advances_counter() {
        let mut allocator = IdAllocator::new();
        assert_eq!(allocator.allocate_node().unwrap(), NodeId::new(1));
        assert_eq!(allocator.peek_next_node(), 2);
    }

    #[test]
    fn allocate_edge_advances_counter() {
        let mut allocator = IdAllocator::new();
        assert_eq!(allocator.allocate_edge().unwrap(), EdgeId::new(1));
        assert_eq!(allocator.peek_next_edge(), 2);
    }

    #[test]
    fn exhausted_counters_fail_repeatedly_without_wrapping_or_reusing() {
        let mut allocator = IdAllocator {
            next_node_id: u64::MAX - 1,
            next_edge_id: u64::MAX - 1,
        };
        assert_eq!(
            allocator.allocate_node().unwrap(),
            NodeId::new(u64::MAX - 1)
        );
        assert_eq!(
            allocator.allocate_edge().unwrap(),
            EdgeId::new(u64::MAX - 1)
        );
        for _ in 0..2 {
            assert!(matches!(
                allocator.allocate_node(),
                Err(GraphError::CounterExhausted {
                    kind: "node identity"
                })
            ));
            assert!(matches!(
                allocator.allocate_edge(),
                Err(GraphError::CounterExhausted {
                    kind: "edge identity"
                })
            ));
            assert_eq!(allocator.peek_next_node(), u64::MAX);
            assert_eq!(allocator.peek_next_edge(), u64::MAX);
        }
    }

    #[test]
    fn from_meta_with_floors_restores_counters_with_zero_floors() {
        let meta = GraphMeta {
            graph_id: GraphId::new(1),
            generation: 7,
            next_node_id: 42,
            next_edge_id: 99,
            bound_type: None,
        };
        let allocator = IdAllocator::from_meta_with_floors(&meta, 0, 0);
        assert_eq!(allocator.peek_next_node(), 42);
        assert_eq!(allocator.peek_next_edge(), 99);
    }

    #[test]
    fn from_meta_with_floors_takes_max_of_meta_and_storage() {
        let meta = GraphMeta {
            graph_id: GraphId::new(1),
            generation: 0,
            next_node_id: 5,
            next_edge_id: 50,
            bound_type: None,
        };
        let allocator = IdAllocator::from_meta_with_floors(&meta, 10, 30);
        assert_eq!(
            allocator.peek_next_node(),
            10,
            "storage floor wins for nodes"
        );
        assert_eq!(allocator.peek_next_edge(), 50, "meta wins for edges");
    }

    #[test]
    fn from_meta_with_floors_uses_meta_when_higher() {
        let meta = GraphMeta {
            graph_id: GraphId::new(1),
            generation: 0,
            next_node_id: 100,
            next_edge_id: 200,
            bound_type: None,
        };
        let allocator = IdAllocator::from_meta_with_floors(&meta, 1, 1);
        assert_eq!(allocator.peek_next_node(), 100);
        assert_eq!(allocator.peek_next_edge(), 200);
    }
}
