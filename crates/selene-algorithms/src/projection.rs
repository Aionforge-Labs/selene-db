//! Filtered subgraph view with cached CSR adjacency for algorithm computation.
//!
//! A [`GraphProjection`] is a frozen view of the graph at a given generation,
//! defined by:
//! - A node bitmap (row-indexed, AND-intersected with optional scope).
//! - An edge-label filter (which edge types appear in the CSR).
//! - An optional weight property (numeric values project to `f64`; missing /
//!   non-numeric / null values default to `1.0` per spec 16 §E04).
//! - Cached out-direction and in-direction CSR adjacency.
//!
//! Projections are immutable once built. When the underlying graph mutates and
//! its `meta.generation` advances, the projection is logically stale. The
//! `ProjectionCatalog` rebuilds projections from stored configs when staleness
//! is detected.

mod csr;
mod row_index;

use selene_core::{DbString, NodeId};
use selene_graph::{CandidateSet, Node, SeleneGraph};

pub use csr::ProjNeighbor;
use csr::{ProjCsr, build_csr_out, transpose_csr_in};
pub(crate) use row_index::RowIndex;

use crate::error::AlgorithmsError;

/// Configuration for creating a graph projection.
///
/// `ProjectionConfig` is the user-facing input surface; literal construction
/// via struct expression is part of the ergonomic contract. Fields added later
/// land via a future builder pattern rather than via `#[non_exhaustive]`.
#[derive(Debug, Clone)]
pub struct ProjectionConfig {
    /// Stable name used by the projection catalog. Projection names are
    /// user-facing and arbitrary; `String` avoids forcing projection-catalog
    /// names through the graph identifier string type.
    pub name: String,
    /// Node labels to include. Empty = all alive nodes (intersected with
    /// `scope` at build time).
    pub node_labels: Vec<DbString>,
    /// Edge labels to include. Empty = all edge types.
    pub edge_labels: Vec<DbString>,
    /// Property key projecting numeric edge weights to `f64`. `None` =
    /// unweighted (all weights = `1.0`).
    ///
    /// Edges lacking this property, or carrying a non-numeric value
    /// (including `Value::Null`), default to weight `1.0` (the same as the
    /// unweighted case). For strict weight validation, preprocess at write
    /// time.
    pub weight_property: Option<DbString>,
}

/// A named subgraph view with cached CSR adjacency for fast algorithm
/// traversal.
///
/// The projection is immutable once created. When the underlying graph mutates
/// (generation changes), the projection is logically stale; the projection
/// catalog invalidates and rebuilds on staleness detection.
#[derive(Debug)]
pub struct GraphProjection {
    name: String,
    /// Typed candidate node set included in this projection (post label
    /// filter, post scope intersection).
    nodes: CandidateSet<Node>,
    edge_labels: Vec<DbString>,
    weight_property: Option<DbString>,
    /// Cached dense `dense_index ↔ external NodeId` remap over the frozen `nodes`
    /// set. Built once at construction and shared by reference with every
    /// algorithm via [`GraphProjection::row_index`].
    row_index: RowIndex,
    out_csr: ProjCsr,
    in_csr: ProjCsr,
    generation: u64,
}

impl GraphProjection {
    /// Build a projection from a frozen graph snapshot.
    ///
    /// Returns `Ok(projection)` even when the filtered node set is empty — an
    /// empty subgraph is a legitimate algorithm input (e.g., "PageRank over a
    /// graph with zero `Person` nodes returns empty"). Algorithms requiring
    /// non-empty input check `node_count() > 0` after build and may raise
    /// [`AlgorithmsError::EmptyProjection`] from their own surface.
    pub fn build(
        snapshot: &SeleneGraph,
        config: &ProjectionConfig,
        scope: Option<&CandidateSet<Node>>,
    ) -> Result<Self, AlgorithmsError> {
        let mut nodes = if config.node_labels.is_empty() {
            snapshot.live_node_candidates()?
        } else {
            let mut set: Option<CandidateSet<Node>> = None;
            for label in &config.node_labels {
                let labeled = snapshot.node_candidates_with_label(label)?;
                set = match set {
                    Some(existing) => Some(snapshot.union_candidates(&existing, &labeled)?),
                    None => Some(labeled),
                };
            }
            let unioned = set.unwrap_or_else(|| snapshot.bind_node_candidates([]).unwrap());
            let live = snapshot.live_node_candidates()?;
            snapshot.intersect_candidates(&unioned, &live)?
        };
        if let Some(scope_set) = scope {
            nodes = snapshot.intersect_candidates(&nodes, scope_set)?;
        }

        let row_index = RowIndex::from_candidates(&nodes);

        let out_csr = build_csr_out(
            snapshot,
            &row_index,
            &config.edge_labels,
            config.weight_property.as_ref(),
        );
        let in_csr = transpose_csr_in(&out_csr, &row_index);
        #[cfg(debug_assertions)]
        assert_csr_transpose(&row_index, &out_csr, &in_csr);

        Ok(Self {
            name: config.name.clone(),
            nodes,
            edge_labels: config.edge_labels.clone(),
            weight_property: config.weight_property.clone(),
            row_index,
            out_csr,
            in_csr,
            generation: snapshot.meta.generation,
        })
    }

    /// Cached dense `sparse_row ↔ dense_index` remap for this projection's live
    /// nodes.
    ///
    /// Built once at construction over the frozen node set; algorithms borrow it
    /// instead of rebuilding per call. Dense indices are assigned ASC by NodeId
    /// (spec 16 §E03), so tie-break ordering is identical to the previous
    /// per-call construction.
    #[must_use]
    pub(crate) fn row_index(&self) -> &RowIndex {
        &self.row_index
    }

    /// Length of the out-direction CSR offsets vector (`live_count + 1`).
    /// Test-only: proves OPT-9 dense (not sparse) offset sizing.
    #[cfg(test)]
    pub(crate) fn out_csr_offsets_len(&self) -> usize {
        self.out_csr.offsets_len()
    }

    /// Length of the in-direction CSR offsets vector (`live_count + 1`).
    /// Test-only: proves OPT-9 dense (not sparse) offset sizing.
    #[cfg(test)]
    pub(crate) fn in_csr_offsets_len(&self) -> usize {
        self.in_csr.offsets_len()
    }

    /// Projection name (from `ProjectionConfig::name`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of nodes in the projection.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of **outgoing** edges in this projection.
    ///
    /// For directed graphs this is the total edge count; for algorithms
    /// treating the projection as undirected (e.g., WCC, label propagation),
    /// iterate `out_neighbors()` ∪ `in_neighbors()` per node to avoid
    /// double-counting — do NOT sum `out_degree() + in_degree()` over all
    /// nodes.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.out_csr.total_neighbors()
    }

    /// Graph generation pinned at build time. The projection catalog compares
    /// this against `snapshot.meta.generation` for staleness detection.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns true when `node` is part of this projection.
    #[must_use]
    pub fn contains(&self, node: NodeId) -> bool {
        self.row_index.dense_of_node(node).is_some()
    }

    /// Out-neighbors of `node`, sorted ASC by `node_id` per spec 16 §E03.
    ///
    /// Returns an empty slice when `node` is not in this projection or has no
    /// qualifying outgoing edges.
    #[must_use]
    pub fn out_neighbors(&self, node: NodeId) -> &[ProjNeighbor] {
        let Some(dense) = self.row_index.dense_of_node(node) else {
            return &[];
        };
        self.out_csr.neighbors_of_dense(dense)
    }

    /// Out-neighbors of a dense projection row.
    #[must_use]
    pub(crate) fn out_neighbors_dense(&self, dense: u32) -> &[ProjNeighbor] {
        self.out_csr.neighbors_of_dense(dense)
    }

    /// In-neighbors of a dense projection row.
    #[must_use]
    pub(crate) fn in_neighbors_dense(&self, dense: u32) -> &[ProjNeighbor] {
        self.in_csr.neighbors_of_dense(dense)
    }

    /// In-neighbors of `node`, sorted ASC by `node_id` per spec 16 §E03.
    ///
    /// Returns an empty slice when `node` is not in this projection or has no
    /// qualifying incoming edges.
    #[must_use]
    pub fn in_neighbors(&self, node: NodeId) -> &[ProjNeighbor] {
        let Some(dense) = self.row_index.dense_of_node(node) else {
            return &[];
        };
        self.in_csr.neighbors_of_dense(dense)
    }

    /// Out-degree of `node` within this projection.
    #[must_use]
    pub fn out_degree(&self, node: NodeId) -> usize {
        self.out_neighbors(node).len()
    }

    /// In-degree of `node` within this projection.
    #[must_use]
    pub fn in_degree(&self, node: NodeId) -> usize {
        self.in_neighbors(node).len()
    }

    /// Iterate node IDs in the projection in **ascending order** (inherited
    /// from `RoaringBitmap` iteration).
    ///
    /// Use for deterministic traversal in tests and snapshot goldens; algorithm
    /// correctness on ties (BFS visit order, SCC enumeration order, Louvain
    /// community-id assignment on equal modularity) depends on this stability.
    pub fn iter_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.row_index.iter_node_ids()
    }

    /// Returns true when this projection carries weighted edges.
    #[must_use]
    pub fn is_weighted(&self) -> bool {
        self.weight_property.is_some()
    }

    /// Edge-label filter declared at projection build time. Empty slice means
    /// "all edge types were admitted" (no filter).
    #[must_use]
    pub fn edge_labels(&self) -> &[DbString] {
        &self.edge_labels
    }
}

#[cfg(debug_assertions)]
fn assert_csr_transpose(row_index: &RowIndex, out_csr: &ProjCsr, in_csr: &ProjCsr) {
    let mut out_edges = Vec::with_capacity(out_csr.total_neighbors());
    for (dense, source) in row_index.iter_node_ids().enumerate() {
        let dense = dense as u32;
        for neighbor in out_csr.neighbors_of_dense(dense) {
            out_edges.push((neighbor.edge_id, source, neighbor.node_id));
        }
    }

    let mut in_edges = Vec::with_capacity(in_csr.total_neighbors());
    for (dense, target) in row_index.iter_node_ids().enumerate() {
        let dense = dense as u32;
        for neighbor in in_csr.neighbors_of_dense(dense) {
            in_edges.push((neighbor.edge_id, neighbor.node_id, target));
        }
    }

    out_edges.sort_unstable();
    in_edges.sort_unstable();
    debug_assert_eq!(
        out_edges, in_edges,
        "GraphProjection out/in CSR transpose invariant violated"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use selene_core::{GraphId, LabelSet, PropertyMap, Value};
    use selene_graph::SharedGraph;

    fn db_string(name: &str) -> DbString {
        selene_core::db_string(name).unwrap()
    }

    /// Build a graph with `total` nodes (all labeled `T`), keep only the nodes
    /// at the given sparse rows alive (delete the rest), and wire a ring of
    /// `link` edges among the survivors. Returns the shared graph plus the
    /// surviving NodeIds in insertion order.
    fn sparse_ring(total: u64, keep_rows: &[u32]) -> (SharedGraph, Vec<NodeId>) {
        let shared = SharedGraph::new(GraphId::new(7_700));
        let label = db_string("T");
        let link = db_string("link");

        let mut all = Vec::with_capacity(total as usize);
        {
            let mut txn = shared.begin_write();
            for _ in 0..total {
                let nid = txn
                    .mutator()
                    .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                    .unwrap();
                all.push(nid);
            }
            txn.commit().unwrap();
        }

        // Map keep_rows (sparse) to NodeIds via the fixture's own creation order
        // (all[r] is the id created for row r) — no row+1 assumption.
        let survivors: Vec<NodeId> = keep_rows.iter().map(|&r| all[r as usize]).collect();

        // Delete every node NOT in survivors, creating sparse holes.
        {
            let mut txn = shared.begin_write();
            for &nid in &all {
                if !survivors.contains(&nid) {
                    txn.mutator().delete_node(nid).unwrap();
                }
            }
            // Ring edges among survivors.
            for i in 0..survivors.len() {
                let a = survivors[i];
                let b = survivors[(i + 1) % survivors.len()];
                if a != b {
                    txn.mutator()
                        .create_edge(link.clone(), a, b, PropertyMap::new())
                        .unwrap();
                }
            }
            txn.commit().unwrap();
        }

        (shared, survivors)
    }

    fn config() -> ProjectionConfig {
        ProjectionConfig {
            name: "p".to_string(),
            node_labels: vec![db_string("T")],
            edge_labels: vec![],
            weight_property: None,
        }
    }

    /// The cached `row_index()` round-trips every live node through the captured
    /// external-id mapping: `dense_of_node` and `node_id_of` invert, and
    /// `dense_of(row)` (row read from the graph, not synthesized as `id - 1`)
    /// agrees with `dense_of_node`.
    #[test]
    fn cached_row_index_round_trips() {
        // Sparse: keep rows 10, 500, 999 out of 1000.
        let (shared, survivors) = sparse_ring(1000, &[10, 500, 999]);
        let snapshot = shared.read();
        let proj = GraphProjection::build(&snapshot, &config(), None).unwrap();

        let cached = proj.row_index();
        assert_eq!(cached.len(), survivors.len());

        // iter_nodes() must yield NodeIds in ASC order (spec 16 §E03), pinned
        // independent of the id↔row mapping so a 4b ordering regression is caught.
        let ids: Vec<NodeId> = proj.iter_nodes().collect();
        for w in ids.windows(2) {
            assert!(
                w[0].get() < w[1].get(),
                "iter_nodes must yield NodeIds ascending per spec 16 §E03"
            );
        }

        for &nid in &survivors {
            let dense = cached
                .dense_of_node(nid)
                .expect("survivor has a dense index");
            assert_eq!(cached.node_id_of(dense), nid);
        }

        // Deleted node 1 is outside the projection.
        assert_eq!(cached.dense_of_node(NodeId::new(1)), None);
    }

    /// OPT-9: CSR offsets are sized by live-node count, NOT node_store.len().
    /// A 3-node projection over a 1000-row store allocates 4 offsets, not 1001.
    #[test]
    fn csr_offsets_sized_by_live_count() {
        let (shared, survivors) = sparse_ring(1000, &[10, 500, 999]);
        let snapshot = shared.read();
        let proj = GraphProjection::build(&snapshot, &config(), None).unwrap();

        assert_eq!(proj.node_count(), survivors.len());
        assert_eq!(proj.out_csr_offsets_len(), survivors.len() + 1);
        assert_eq!(proj.in_csr_offsets_len(), survivors.len() + 1);
        // Emphatically NOT the sparse store length.
        assert!(proj.out_csr_offsets_len() < 100);
    }

    /// OPT-8/9 transparency: out/in neighbor sets over a sparse projection are
    /// identical whether the projected nodes sit at low rows {0,1,2} or high
    /// rows {10,500,999} (modulo NodeId labels). The ring structure is the same.
    #[test]
    fn neighbors_identical_low_vs_high_rows() {
        let (lo_shared, lo) = sparse_ring(1000, &[0, 1, 2]);
        let (hi_shared, hi) = sparse_ring(1000, &[10, 500, 999]);
        let lo_snap = lo_shared.read();
        let hi_snap = hi_shared.read();
        let lo_proj = GraphProjection::build(&lo_snap, &config(), None).unwrap();
        let hi_proj = GraphProjection::build(&hi_snap, &config(), None).unwrap();

        assert_eq!(lo_proj.node_count(), 3);
        assert_eq!(hi_proj.node_count(), 3);

        // Map each ring position to (out_degree, in_degree); must match by index.
        for i in 0..3 {
            assert_eq!(
                lo_proj.out_degree(lo[i]),
                hi_proj.out_degree(hi[i]),
                "out_degree mismatch at ring position {i}"
            );
            assert_eq!(
                lo_proj.in_degree(lo[i]),
                hi_proj.in_degree(hi[i]),
                "in_degree mismatch at ring position {i}"
            );
        }

        // Both rings: every node has exactly one out and one in neighbor.
        for &nid in &hi {
            assert_eq!(hi_proj.out_neighbors(nid).len(), 1);
            assert_eq!(hi_proj.in_neighbors(nid).len(), 1);
        }
    }

    #[test]
    fn incoming_csr_transpose_preserves_sources_dense_indices_and_weights() {
        let shared = SharedGraph::new(GraphId::new(7_703));
        let label = db_string("T");
        let link = db_string("link");
        let weight = db_string("weight");
        let (a, b, c) = {
            let mut txn = shared.begin_write();
            let a = txn
                .mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .unwrap();
            let b = txn
                .mutator()
                .create_node(LabelSet::single(label.clone()), PropertyMap::new())
                .unwrap();
            let c = txn
                .mutator()
                .create_node(LabelSet::single(label), PropertyMap::new())
                .unwrap();
            txn.mutator()
                .create_edge(
                    link.clone(),
                    c,
                    b,
                    PropertyMap::from_pairs([(weight.clone(), Value::Float(3.0))]).unwrap(),
                )
                .unwrap();
            txn.mutator()
                .create_edge(
                    link,
                    a,
                    b,
                    PropertyMap::from_pairs([(weight.clone(), Value::Float(1.0))]).unwrap(),
                )
                .unwrap();
            txn.commit().unwrap();
            (a, b, c)
        };

        let snapshot = shared.read();
        let cfg = ProjectionConfig {
            name: "weighted".to_string(),
            node_labels: vec![db_string("T")],
            edge_labels: vec![db_string("link")],
            weight_property: Some(weight),
        };
        let proj = GraphProjection::build(&snapshot, &cfg, None).unwrap();
        let incoming = proj.in_neighbors(b);

        assert_eq!(
            incoming.iter().map(|n| n.node_id).collect::<Vec<_>>(),
            vec![a, c]
        );
        assert_eq!(
            incoming.iter().map(|n| n.dense).collect::<Vec<_>>(),
            vec![
                proj.row_index().dense_of_node(a).unwrap(),
                proj.row_index().dense_of_node(c).unwrap()
            ]
        );
        assert_eq!(
            incoming.iter().map(|n| n.weight).collect::<Vec<_>>(),
            vec![1.0, 3.0]
        );
    }

    #[test]
    fn outgoing_csr_orders_mixed_label_neighbors_by_node_id() {
        let shared = SharedGraph::new(GraphId::new(7_704));
        let node_label = db_string("T");
        let early_label = db_string("A");
        let late_label = db_string("Z");
        let (source, low, high) = {
            let mut txn = shared.begin_write();
            let source = txn
                .mutator()
                .create_node(LabelSet::single(node_label.clone()), PropertyMap::new())
                .unwrap();
            let low = txn
                .mutator()
                .create_node(LabelSet::single(node_label.clone()), PropertyMap::new())
                .unwrap();
            let high = txn
                .mutator()
                .create_node(LabelSet::single(node_label), PropertyMap::new())
                .unwrap();
            txn.mutator()
                .create_edge(early_label, source, high, PropertyMap::new())
                .unwrap();
            txn.mutator()
                .create_edge(late_label, source, low, PropertyMap::new())
                .unwrap();
            txn.commit().unwrap();
            (source, low, high)
        };

        let snapshot = shared.read();
        let proj = GraphProjection::build(&snapshot, &config(), None).unwrap();

        assert_eq!(
            proj.out_neighbors(source)
                .iter()
                .map(|neighbor| neighbor.node_id)
                .collect::<Vec<_>>(),
            vec![low, high]
        );
    }

    /// Empty projection: cached row_index is empty and offsets are [0].
    #[test]
    fn empty_projection_dense_offsets() {
        let shared = SharedGraph::new(GraphId::new(7_701));
        let snapshot = shared.read();
        let cfg = ProjectionConfig {
            name: "empty".to_string(),
            node_labels: vec![db_string("Nonexistent")],
            edge_labels: vec![],
            weight_property: None,
        };
        let proj = GraphProjection::build(&snapshot, &cfg, None).unwrap();
        assert_eq!(proj.node_count(), 0);
        assert!(proj.row_index().is_empty());
        assert_eq!(proj.out_csr_offsets_len(), 1);
        assert_eq!(proj.in_csr_offsets_len(), 1);
        // Out-of-projection lookups return empty.
        assert!(proj.out_neighbors(NodeId::new(1)).is_empty());
    }

    /// Scope intersection still produces a dense map keyed by survivors.
    #[test]
    fn scoped_projection_dense_map() {
        let (shared, survivors) = sparse_ring(1000, &[10, 500, 999]);
        let snapshot = shared.read();
        // Scope to survivors {0, 2} only (drop 500).
        let scope = snapshot
            .bind_node_candidates([survivors[0], survivors[2]])
            .unwrap();
        let proj = GraphProjection::build(&snapshot, &config(), Some(&scope)).unwrap();
        assert_eq!(proj.node_count(), 2);
        assert_eq!(proj.out_csr_offsets_len(), 3);
        assert_eq!(proj.row_index().len(), 2);
        // The dropped node is not in the projection.
        assert_eq!(proj.row_index().dense_of_node(survivors[1]), None);
    }

    /// BRIEF-Item-4a Increment 6 — non-identity proof for the algorithms layer.
    /// A graph whose external ids are NOT `row + 1` (NodeId 5 @ row 0, NodeId 8 @
    /// row 1, EdgeId 3 @ row 0) must project + traverse by external id. The
    /// RowIndex capture and dense remap are exercised against a genuine
    /// non-identity mapping (the 4b shape), proving algorithms emit correct
    /// external NodeIds rather than row-derived ones.
    #[test]
    fn projection_over_non_identity_graph_emits_external_node_ids() {
        use selene_core::EdgeId;
        let label = db_string("T");
        let link = db_string("link");
        let mut built = SeleneGraph::new(GraphId::new(7_702));
        built
            .node_store
            .labels
            .push(LabelSet::single(label.clone()));
        built.node_store.properties.push(PropertyMap::new());
        built.node_store.row_to_id.push(NodeId::new(5));
        built.node_store.labels.push(LabelSet::single(label));
        built.node_store.properties.push(PropertyMap::new());
        built.node_store.row_to_id.push(NodeId::new(8));
        built.node_store.alive_mut().insert(0);
        built.node_store.alive_mut().insert(1);
        built.edge_store.label.push(link);
        built.edge_store.source.push(NodeId::new(5));
        built.edge_store.target.push(NodeId::new(8));
        built.edge_store.properties.push(PropertyMap::new());
        built.edge_store.row_to_id.push(EdgeId::new(3));
        built.edge_store.alive_mut().insert(0);
        built.meta.next_node_id = 9;
        built.meta.next_edge_id = 4;
        let shared = SharedGraph::from_graph(built);
        let snapshot = shared.read();
        let proj = GraphProjection::build(&snapshot, &config(), None).unwrap();

        // iter_nodes yields external ids (ASC), never row-derived ids.
        assert_eq!(
            proj.iter_nodes().collect::<Vec<_>>(),
            vec![NodeId::new(5), NodeId::new(8)]
        );
        assert!(proj.contains(NodeId::new(5)));
        // Row 0 carries external id 5, NOT 1 — the row+1 answer is wrong.
        assert!(!proj.contains(NodeId::new(1)));
        // Adjacency + degree resolve by external id.
        let outs: Vec<NodeId> = proj
            .out_neighbors(NodeId::new(5))
            .iter()
            .map(|n| n.node_id)
            .collect();
        assert_eq!(outs, vec![NodeId::new(8)]);
        assert_eq!(proj.out_degree(NodeId::new(5)), 1);
        assert_eq!(proj.in_degree(NodeId::new(8)), 1);
        assert_eq!(proj.out_degree(NodeId::new(8)), 0);
    }
}
