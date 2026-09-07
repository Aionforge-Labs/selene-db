//! Compressed-sparse-row adjacency storage for `GraphProjection`.
//!
//! CSR is keyed by **dense index**, not sparse row. Every value admitted to a
//! projection's `nodes` bitmap is a sparse *row index* sourced from
//! `snapshot.live_nodes()` or `snapshot.nodes_with_label()`; the projection's
//! cached [`RowIndex`] maps each sparse row to a dense index `0..live_count`.
//! Offsets are therefore sized by the live-node count (not `node_store.len()`),
//! so a 100-node projection over a 1M-row store allocates ~100 offsets, not ~1M
//! (review-discovered memory invariant). A neighbor's membership in the
//! projection is tested via the cached [`RowIndex::dense_of_node`] (the map-backed
//! external-id lookup, BRIEF-Item-4a) rather than `id - 1` arithmetic; a row's
//! own `NodeId` is recovered via [`RowIndex::node_id_of`].
//!
//! CSR neighbor/offset count bound: `offsets: Vec<u32>` and the per-bucket
//! offsets are u32-indexed, so total projected neighbor count is bounded by
//! `u32::MAX` (~4.3 B), aligning with selene-graph's row addressing. Production
//! graphs approaching this bound are out of scope for the current in-memory
//! algorithm surface — distributed algorithms handle them.

use selene_core::{DbString, EdgeId, NodeId, Value};
use selene_graph::SeleneGraph;

use super::RowIndex;

/// One neighbor entry in a projection's CSR adjacency.
#[derive(Debug, Clone, Copy)]
pub struct ProjNeighbor {
    /// Edge ID underlying this projected neighbor link.
    pub edge_id: EdgeId,
    /// Neighbor node ID (target for outgoing CSR, source for incoming CSR).
    pub node_id: NodeId,
    /// Edge weight projected from the source graph. Defaults to `1.0` when
    /// `weight_property` is `None` or when the property is missing /
    /// non-numeric / `Value::Null` per spec 16 §3 E04 (matches the donor's
    /// permissive behavior).
    pub weight: f64,
    /// Dense index of [`Self::node_id`] within the projection's row index
    /// (ALGO-01). The CSR only ever stores projected neighbors (`build_csr`
    /// pass-1 and pass-2 each skip any neighbor outside the projection), and the
    /// row index is immutable after projection build, so this equals
    /// `row_index.dense_of_node(node_id).unwrap()` for the lifetime of the
    /// projection. Caching it lets the centrality / community / structural
    /// algorithms read the dense index directly from the contiguous CSR slice
    /// instead of re-probing the `node_to_dense` hash map on every edge visit.
    pub dense: u32,
}

/// CSR adjacency for one direction within a projection.
///
/// `offsets[dense]..offsets[dense+1]` is the neighbor slice for the node at that
/// dense index (assigned by the projection's [`RowIndex`]). `offsets` is sized
/// `live_count + 1`; the trailing slot is the running total sentinel.
#[derive(Debug)]
pub(crate) struct ProjCsr {
    offsets: Vec<u32>,
    neighbors: Vec<ProjNeighbor>,
}

impl ProjCsr {
    /// Neighbors of the node at dense index `dense`. Empty slice when the index
    /// is out of range or when the node has no qualifying neighbors.
    pub(crate) fn neighbors_of_dense(&self, dense: u32) -> &[ProjNeighbor] {
        let dense = dense as usize;
        if dense + 1 >= self.offsets.len() {
            return &[];
        }
        let start = self.offsets[dense] as usize;
        let end = self.offsets[dense + 1] as usize;
        &self.neighbors[start..end]
    }

    /// Total neighbor count across all rows.
    pub(crate) fn total_neighbors(&self) -> usize {
        self.neighbors.len()
    }

    /// Length of the offsets vector (`live_count + 1`). Test-only accessor that
    /// proves dense (not sparse) offset sizing.
    #[cfg(test)]
    pub(crate) fn offsets_len(&self) -> usize {
        self.offsets.len()
    }
}

/// Build the outgoing-direction CSR for a projection.
pub(crate) fn build_csr_out(
    snapshot: &SeleneGraph,
    row_index: &RowIndex,
    edge_labels: &[DbString],
    weight_property: Option<&DbString>,
) -> ProjCsr {
    let dense_n = row_index.len();
    let mut offsets = vec![0u32; dense_n + 1];

    // selene-graph adjacency entries inline label + neighbor + edge_id; no
    // per-edge id → data lookup is needed during the filter pass (cf. donor
    // aether-db-algorithms projection.rs:248 which calls graph.get_edge per
    // edge id). Only weight extraction performs an additional lookup, gated
    // by `weight_property.is_some()`.

    // Pass 1: count qualifying neighbors per dense index.
    for (dense, nid) in row_index.iter_node_ids().enumerate() {
        let Some(entry) = snapshot.outgoing_edges(nid) else {
            continue;
        };
        for adj in entry.iter() {
            if row_index.dense_of_node(adj.neighbor).is_none() {
                continue;
            }
            if !edge_labels.is_empty() && !edge_labels.contains(&adj.label) {
                continue;
            }
            offsets[dense] += 1;
        }
    }

    // Convert per-row counts into cumulative offsets (prefix sum). The final
    // slot holds the total neighbor count.
    let mut cumulative: u32 = 0;
    for slot in offsets.iter_mut() {
        let count = *slot;
        *slot = cumulative;
        cumulative = cumulative.saturating_add(count);
    }

    // Pass 2: fill the neighbor array using a per-dense-index write cursor.
    let total = cumulative as usize;
    let mut neighbors: Vec<ProjNeighbor> = vec![
        ProjNeighbor {
            edge_id: EdgeId::new(0),
            node_id: NodeId::new(0),
            weight: 0.0,
            dense: 0,
        };
        total
    ];
    // Why: the per-dense write cursor must start at each row's base offset, but
    // `offsets` is still read immutably here (and is then moved into `ProjCsr`
    // below as the canonical row-base array). A clone gives the cursor an
    // independent mutable copy without aliasing the offsets we keep. This is a
    // build-time-only allocation, trivial next to the `neighbors` Vec.
    let mut cursor = offsets.clone();

    for (dense, nid) in row_index.iter_node_ids().enumerate() {
        let Some(entry) = snapshot.outgoing_edges(nid) else {
            continue;
        };
        for adj in entry.iter() {
            // Capture the neighbor's dense index here (ALGO-01): this pass is
            // already resolving it for the projection-membership skip, so caching
            // it costs one extra u32 write and zero additional map probes.
            let Some(neighbor_dense) = row_index.dense_of_node(adj.neighbor) else {
                continue;
            };
            if !edge_labels.is_empty() && !edge_labels.contains(&adj.label) {
                continue;
            }
            let weight = extract_weight(snapshot, adj.edge_id, weight_property);
            let pos = cursor[dense] as usize;
            neighbors[pos] = ProjNeighbor {
                edge_id: adj.edge_id,
                node_id: adj.neighbor,
                weight,
                dense: neighbor_dense,
            };
            cursor[dense] += 1;
        }
    }

    // Per spec 16 §E03: sort each row's bucket by node_id ASC. selene-graph's
    // AdjacencyEntry is sorted by (label, neighbor, edge_id); after filtering
    // by edge_labels the per-row order is not pure ASC-by-neighbor, so we
    // re-sort explicitly to guarantee deterministic algorithm tie-breaks and
    // snapshot-harness goldens.
    for d in 0..dense_n {
        let start = offsets[d] as usize;
        let end = offsets[d + 1] as usize;
        sort_neighbors_by_node_id(&mut neighbors[start..end]);
    }

    ProjCsr { offsets, neighbors }
}

/// Build the incoming-direction CSR by transposing an already-filtered out CSR.
pub(crate) fn transpose_csr_in(out_csr: &ProjCsr, row_index: &RowIndex) -> ProjCsr {
    let dense_n = row_index.len();
    debug_assert_eq!(
        out_csr.offsets.len(),
        dense_n + 1,
        "out CSR offsets must match the projection row index"
    );
    let mut offsets = vec![0u32; dense_n + 1];

    for source_dense in 0..dense_n {
        let source_dense = u32::try_from(source_dense).expect("projection dense index fits u32");
        for neighbor in out_csr.neighbors_of_dense(source_dense) {
            offsets[neighbor.dense as usize] += 1;
        }
    }

    let mut cumulative: u32 = 0;
    for slot in &mut offsets {
        let count = *slot;
        *slot = cumulative;
        cumulative = cumulative.saturating_add(count);
    }

    let total = cumulative as usize;
    let mut neighbors: Vec<ProjNeighbor> = vec![
        ProjNeighbor {
            edge_id: EdgeId::new(0),
            node_id: NodeId::new(0),
            weight: 0.0,
            dense: 0,
        };
        total
    ];
    let mut cursor = offsets.clone();

    for source_dense in 0..dense_n {
        let source_dense = u32::try_from(source_dense).expect("projection dense index fits u32");
        let source = row_index.node_id_of(source_dense);
        for neighbor in out_csr.neighbors_of_dense(source_dense) {
            let target_dense = neighbor.dense as usize;
            let pos = cursor[target_dense] as usize;
            neighbors[pos] = ProjNeighbor {
                edge_id: neighbor.edge_id,
                node_id: source,
                weight: neighbor.weight,
                dense: source_dense,
            };
            cursor[target_dense] += 1;
        }
    }

    for d in 0..dense_n {
        let start = offsets[d] as usize;
        let end = offsets[d + 1] as usize;
        sort_neighbors_by_node_id(&mut neighbors[start..end]);
    }

    ProjCsr { offsets, neighbors }
}

fn sort_neighbors_by_node_id(neighbors: &mut [ProjNeighbor]) {
    if neighbors
        .windows(2)
        .all(|pair| pair[0].node_id <= pair[1].node_id)
    {
        return;
    }
    neighbors.sort_by_key(|n| n.node_id);
}

/// Extract the edge weight per spec 16 §E04 (permissive: missing / non-numeric
/// / null → `1.0`).
fn extract_weight(
    snapshot: &SeleneGraph,
    edge_id: EdgeId,
    weight_property: Option<&DbString>,
) -> f64 {
    let Some(prop) = weight_property else {
        return 1.0;
    };
    let Some(properties) = snapshot.edge_properties(edge_id) else {
        return 1.0;
    };
    match properties.get(prop) {
        Some(Value::Float(f)) => *f,
        Some(Value::Int(i)) => *i as f64,
        Some(Value::Uint(u)) => *u as f64,
        _ => 1.0,
    }
}
