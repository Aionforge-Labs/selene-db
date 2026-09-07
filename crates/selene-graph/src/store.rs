//! Structure-of-arrays node and edge stores per spec 03 section 3.1.
//!
//! Physical kind rows are not repository-public APIs:
//!
//! ```compile_fail
//! use selene_graph::store::{EdgeRow, NodeRow};
//!
//! fn cannot_cross_or_mix(edge: EdgeRow) {
//!     let _: NodeRow = edge;
//! }
//! ```
//!
//! Physical row indices are not repository-public APIs:
//!
//! ```compile_fail
//! use selene_graph::RowIndex;
//! ```
//!
//! Candidates of different kinds cannot be mixed:
//!
//! ```compile_fail
//! use selene_graph::{CandidateSet, Edge, Node};
//!
//! fn cannot_mix(nodes: CandidateSet<Node>) {
//!     let _: CandidateSet<Edge> = nodes;
//! }
//! ```

use std::sync::Arc;

use roaring::RoaringBitmap;

use selene_core::{DbString, EdgeId, LabelSet, NodeId, PropertyMap};

use crate::chunked_vec::ChunkedVec;

/// Physical node-store position. Deliberately private to the graph crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct NodeRow(u32);

impl NodeRow {
    /// Sentinel for "no row" (`u32::MAX`, never a valid dense row position).
    #[allow(dead_code)]
    pub(crate) const TOMBSTONE: NodeRow = NodeRow(u32::MAX);

    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }

    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Physical edge-store position. Deliberately private to the graph crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct EdgeRow(u32);

impl EdgeRow {
    /// Sentinel for "no row" (`u32::MAX`, never a valid dense row position).
    #[allow(dead_code)]
    pub(crate) const TOMBSTONE: EdgeRow = EdgeRow(u32::MAX);

    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }

    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Node columns plus liveness bitmap.
#[derive(Clone, Debug)]
pub struct NodeStore {
    /// Per-row node label sets.
    pub labels: ChunkedVec<LabelSet>,
    /// Per-row node property maps.
    pub properties: ChunkedVec<PropertyMap>,
    /// Per-row external node id (`RowIndex -> NodeId`). Dead / hole rows hold
    /// [`NodeId::TOMBSTONE`]. Parallel with [`labels`](Self::labels): the stable
    /// id is read here, never synthesized as `row + 1`, so compaction can
    /// renumber rows under stable ids.
    pub row_to_id: ChunkedVec<NodeId>,
    /// Alive row indexes, shared copy-on-write across snapshots (B1): cloning
    /// the store bumps a refcount; mutate only through
    /// [`alive_mut`](Self::alive_mut) so a pre-clone snapshot never observes
    /// the change.
    pub alive: Arc<RoaringBitmap>,
}

impl NodeStore {
    /// Construct an empty node store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            labels: ChunkedVec::new(),
            properties: ChunkedVec::new(),
            row_to_id: ChunkedVec::new(),
            alive: Arc::new(RoaringBitmap::new()),
        }
    }

    /// Mutable access to the alive bitmap via `Arc::make_mut` (B1 COW): the
    /// first mutation after a snapshot clone pays one bitmap clone; while the
    /// Arc is unique this is a plain `&mut` borrow.
    pub fn alive_mut(&mut self) -> &mut RoaringBitmap {
        Arc::make_mut(&mut self.alive)
    }

    /// Number of allocated node rows, including dead holes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.labels.len()
    }

    /// Return true when no node rows exist.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return true when `index` is alive.
    #[must_use]
    pub fn is_alive(&self, index: u32) -> bool {
        self.alive.contains(index)
    }

    pub(crate) fn is_alive_row(&self, row: NodeRow) -> bool {
        self.alive.contains(row.get())
    }

    pub(crate) fn alive_rows(&self) -> impl Iterator<Item = NodeRow> + '_ {
        self.alive.iter().map(NodeRow::new)
    }

    pub(crate) fn mark_alive(&mut self, row: NodeRow) {
        self.alive_mut().insert(row.get());
    }

    pub(crate) fn mark_dead(&mut self, row: NodeRow) {
        self.alive_mut().remove(row.get());
    }
}

impl Default for NodeStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Edge columns plus liveness bitmap.
///
/// Stored edges are directed by construction: every live row has exactly one
/// source node and one target node. Undirected query patterns are a matching
/// convenience and do not add an undirected storage bit.
#[derive(Clone, Debug)]
pub struct EdgeStore {
    /// Per-row edge label.
    pub label: ChunkedVec<DbString>,
    /// Per-row edge source node.
    pub source: ChunkedVec<NodeId>,
    /// Per-row edge target node.
    pub target: ChunkedVec<NodeId>,
    /// Per-row edge property maps.
    pub properties: ChunkedVec<PropertyMap>,
    /// Per-row external edge id (`RowIndex -> EdgeId`). Dead / hole rows hold
    /// [`EdgeId::TOMBSTONE`]. Parallel with [`label`](Self::label): the stable id
    /// is read here, never synthesized as `row + 1`.
    pub row_to_id: ChunkedVec<EdgeId>,
    /// Alive row indexes, shared copy-on-write across snapshots (B1): cloning
    /// the store bumps a refcount; mutate only through
    /// [`alive_mut`](Self::alive_mut) so a pre-clone snapshot never observes
    /// the change.
    pub alive: Arc<RoaringBitmap>,
}

impl EdgeStore {
    /// Construct an empty edge store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            label: ChunkedVec::new(),
            source: ChunkedVec::new(),
            target: ChunkedVec::new(),
            properties: ChunkedVec::new(),
            row_to_id: ChunkedVec::new(),
            alive: Arc::new(RoaringBitmap::new()),
        }
    }

    /// Mutable access to the alive bitmap via `Arc::make_mut` (B1 COW): the
    /// first mutation after a snapshot clone pays one bitmap clone; while the
    /// Arc is unique this is a plain `&mut` borrow.
    pub fn alive_mut(&mut self) -> &mut RoaringBitmap {
        Arc::make_mut(&mut self.alive)
    }

    /// Number of allocated edge rows, including dead holes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.label.len()
    }

    /// Return true when no edge rows exist.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return true when `index` is alive.
    #[must_use]
    pub fn is_alive(&self, index: u32) -> bool {
        self.alive.contains(index)
    }

    pub(crate) fn is_alive_row(&self, row: EdgeRow) -> bool {
        self.alive.contains(row.get())
    }

    pub(crate) fn alive_rows(&self) -> impl Iterator<Item = EdgeRow> + '_ {
        self.alive.iter().map(EdgeRow::new)
    }

    pub(crate) fn mark_alive(&mut self, row: EdgeRow) {
        self.alive_mut().insert(row.get());
    }

    pub(crate) fn mark_dead(&mut self, row: EdgeRow) {
        self.alive_mut().remove(row.get());
    }
}

impl Default for EdgeStore {
    fn default() -> Self {
        Self::new()
    }
}

// BRIEF-Item-4c retired `node_row_index_arith` / `edge_row_index_arith`: both the
// live create path (mutator) and the recovery WAL-replay path now allocate rows
// by APPEND (`row = store.len()`) rather than `id - 1`. After 4b compaction the
// monotonic high-water id far exceeds the dense row count, so id-arith would
// re-pad the reclaimed holes; append keeps stores dense. No production read path
// derives a row from an id anymore — the grep-gate (`check-no-rowid-arith.sh`)
// needs no binding-authority allowlist.

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use selene_core::db_string;

    #[test]
    fn node_store_new_is_empty() {
        let store = NodeStore::new();
        assert_eq!(store.len(), 0);
        assert!(store.alive.is_empty());
    }

    #[test]
    fn edge_store_new_is_empty() {
        let store = EdgeStore::new();
        assert_eq!(store.len(), 0);
        assert!(store.alive.is_empty());
    }

    #[test]
    fn is_alive_after_simulated_insert() {
        let mut store = NodeStore::new();
        store
            .labels
            .push(LabelSet::single(db_string("store.node").unwrap()));
        store.properties.push(PropertyMap::new());
        store.alive_mut().insert(0);
        assert!(store.is_alive(0));
        assert!(!store.is_alive(1));
    }

    #[test]
    fn clone_shares_alive_bitmap_until_mutation() {
        // B1: a read-only store clone shares the alive bitmap by refcount.
        let mut store = NodeStore::new();
        store.alive_mut().insert(0);
        store.alive_mut().insert(7);
        let cloned = store.clone();
        assert!(Arc::ptr_eq(&store.alive, &cloned.alive));
        assert_eq!(Arc::strong_count(&store.alive), 2);
    }

    #[test]
    fn alive_mutation_on_clone_isolates_snapshot() {
        // B1: delete (remove) and create (insert) on a clone must COW the
        // bitmap; the original snapshot keeps its pre-mutation liveness.
        let mut store = EdgeStore::new();
        store.alive_mut().insert(0);
        store.alive_mut().insert(1);
        let mut cloned = store.clone();
        cloned.alive_mut().remove(1);
        cloned.alive_mut().insert(9);
        assert!(store.is_alive(0));
        assert!(store.is_alive(1));
        assert!(!store.is_alive(9));
        assert!(cloned.is_alive(0));
        assert!(!cloned.is_alive(1));
        assert!(cloned.is_alive(9));
        assert!(!Arc::ptr_eq(&store.alive, &cloned.alive));
    }

    proptest! {
        #[test]
        fn alive_clone_isolation_across_create_delete_compact(
            seed_rows in proptest::collection::btree_set(0_u32..4096, 0..256),
            post_ops in proptest::collection::vec((any::<bool>(), 0_u32..4096), 1..256),
            compact in any::<bool>(),
        ) {
            // B1 (c): a snapshot clone of the alive bitmap is never disturbed
            // by later creates (insert), deletes (remove), or a
            // compaction-style dense rebuild on the live store.
            let mut live = NodeStore::new();
            for row in &seed_rows {
                live.alive_mut().insert(*row);
            }
            let snapshot = live.clone();
            let mut model: std::collections::BTreeSet<u32> = seed_rows.clone();
            for (insert, row) in post_ops {
                if insert {
                    live.alive_mut().insert(row);
                    model.insert(row);
                } else {
                    live.alive_mut().remove(row);
                    model.remove(&row);
                }
            }
            if compact {
                // compact_core-shaped rebuild: renumber survivors dense from a
                // fresh store, then overwrite the live store wholesale.
                let mut dense = NodeStore::new();
                for new_row in 0..model.len() as u32 {
                    dense.alive_mut().insert(new_row);
                }
                live = dense;
                model = (0..model.len() as u32).collect();
            }
            prop_assert_eq!(
                snapshot.alive.iter().collect::<Vec<_>>(),
                seed_rows.iter().copied().collect::<Vec<_>>()
            );
            prop_assert_eq!(
                live.alive.iter().collect::<Vec<_>>(),
                model.iter().copied().collect::<Vec<_>>()
            );
        }
    }
}
