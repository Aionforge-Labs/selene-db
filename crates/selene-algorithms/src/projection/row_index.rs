//! Dense-index remap layer for algorithm state arrays.
//!
//! Why: algorithms size state arrays by `row_index.len()` and use dense
//! indices `0..live_count` internally, recovering external [`NodeId`] via
//! [`RowIndex::node_id_of`].
//!
//! The map is constructed once when the [`GraphProjection`](super::GraphProjection)
//! is built from `CandidateSet<Node>`. Because candidate set iteration yields
//! `NodeId` in ascending order, dense indices are assigned ASC-by-NodeId
//! per spec 16 §E03.

use rustc_hash::FxHashMap as HashMap;
use selene_core::NodeId;
use selene_graph::{CandidateSet, Node};

/// Bidirectional remap for a projection's live nodes: `dense_index ↔ external NodeId`.
#[derive(Debug)]
pub(crate) struct RowIndex {
    /// `dense_index → external NodeId`.
    node_ids: Vec<NodeId>,
    /// `external NodeId → dense_index`, the reverse of [`Self::node_ids`].
    node_to_dense: HashMap<NodeId, u32>,
}

impl RowIndex {
    /// Build the dense remap from a candidate node set.
    ///
    /// Dense indices are assigned in **ASC-by-NodeId** order per spec 16 §E03.
    pub(crate) fn from_candidates(candidates: &CandidateSet<Node>) -> Self {
        let node_ids: Vec<NodeId> = candidates.iter().collect();
        let node_to_dense: HashMap<NodeId, u32> = node_ids
            .iter()
            .enumerate()
            .map(|(dense_idx, id)| (*id, dense_idx as u32))
            .collect();
        Self {
            node_ids,
            node_to_dense,
        }
    }

    /// Number of live nodes in the projection.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.node_ids.len()
    }

    /// Returns `true` when there are no live nodes.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.node_ids.is_empty()
    }

    /// `external NodeId → dense_index`, or `None` when the node is not in the
    /// projection.
    #[inline]
    pub(crate) fn dense_of_node(&self, node: NodeId) -> Option<u32> {
        self.node_to_dense.get(&node).copied()
    }

    /// `dense_index → external NodeId`. Panics on out-of-range dense indices,
    /// which would indicate an algorithm bug rather than a data issue.
    #[inline]
    pub(crate) fn node_id_of(&self, dense_idx: u32) -> NodeId {
        self.node_ids[dense_idx as usize]
    }

    /// Iterate the projection's external `NodeId`s in **ascending NodeId order**
    /// per spec 16 §E03.
    #[inline]
    pub(crate) fn iter_node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.node_ids.iter().copied()
    }
}
