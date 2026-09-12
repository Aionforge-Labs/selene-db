//! CORE graph compaction mechanism (BRIEF-Item-4b / 4c).
//!
//! [`compact_core`] is the low-level pure transform: given a [`SeleneGraph`], it
//! builds a fresh graph whose rows are dense (every dead or otherwise
//! unoccupied hole row dropped), preserving external `NodeId` / `EdgeId` and
//! the monotonic allocator high-water marks, then rebuilds all derived state
//! from the compacted columns via the shared derived-state rebuilders. It
//! performs no publication and no snapshot I/O. Live embedders normally call
//! [`crate::SharedGraph::compact`], which serializes the transform with writers
//! and atomically republishes the dense graph in the same total publication
//! order as commits. Snapshot I/O remains a separate, caller-driven maintenance
//! step owned by the database facade's checkpoint authority, so the newly
//! dense layout can be published at a fresh snapshot sequence without
//! requiring a dummy user mutation.
//!
//! Because 4a left edges + adjacency keyed by stable external `NodeId`, a row
//! renumber does not touch edge endpoints or adjacency — only the row-keyed
//! columns, the alive bitmaps, the row-indexed label/property indexes, and the
//! id↔row maps move, and all of those are rebuilt from the compacted columns.
//!
//! **Cross-epoch replay.** Compaction drops dead rows, so a *deleted* external
//! id that gets compacted away resolves `NotFound` afterwards. A
//! `Change::NodeDeleted` / `EdgeDeleted` written *before* a compaction must
//! never replay *after* the compacted snapshot loads: it would route through
//! the mutation funnel's `require_live_*` guards and hard-error on the
//! reclaimed id. This cannot happen in the normal flow: snapshot publication
//! is the facade-owned checkpoint authority, which advances the snapshot and
//! its recovery floor together — pre-compaction entries stay below the floor
//! and can never replay against a compacted snapshot. The only cross-epoch
//! replay is of *post*-snapshot entries, which resolve against the dense rows:
//! a post-compaction `NodeCreated` appends, and a post-compaction
//! `NodeDeleted` of a survivor finds it in the compacted snapshot. The
//! `require_live_*` hard-error is deliberately *retained* as
//! genuine-corruption detection — a "no-op the reclaimed-id delete"
//! alternative was rejected because it would mask a truly inconsistent log.

use rustc_hash::FxHashSet;
use selene_core::NodeId;

use crate::error::{GraphError, GraphResult};
use crate::graph::{
    CompositePropertyIndexEntry, PropertyIndexEntry, SeleneGraph, VectorIndexEntry,
};
use crate::store::{EdgeRow, EdgeStore, NodeRow, NodeStore};
use crate::typed_index::TypedIndex;

const BASIS_POINTS_DENOMINATOR: u64 = 10_000;

/// Minimum dead node+edge rows before compaction recommendation can fire.
///
/// Compaction rebuilds the whole live graph, so tiny amounts of row churn should
/// remain explicit caller policy rather than default maintenance advice.
pub const COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS: u64 = 1_024;

/// Minimum dead-row ratio, scaled by 10,000, for compaction advice.
pub const COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_BASIS_POINTS: u64 = 2_500;

/// What CORE reclaimed during a compaction pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompactionReport {
    /// Node rows (dead + hole) dropped.
    pub reclaimed_nodes: u64,
    /// Edge rows (dead + hole) dropped.
    pub reclaimed_edges: u64,
}

/// Cheap snapshot of row-space pressure before compaction.
///
/// Deletes clear heavyweight row payloads immediately, but the row slots and
/// stable id mappings remain until compaction. These counters let maintenance
/// policy decide whether a dense rebuild is worth scheduling without first
/// performing that rebuild.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompactionStats {
    /// Allocated node rows, including live rows and reclaimable dead rows.
    pub allocated_nodes: u64,
    /// Alive node rows.
    pub live_nodes: u64,
    /// Dead node rows that a compaction pass can reclaim.
    pub reclaimable_nodes: u64,
    /// Allocated edge rows, including live rows and reclaimable dead rows.
    pub allocated_edges: u64,
    /// Alive edge rows.
    pub live_edges: u64,
    /// Dead edge rows that a compaction pass can reclaim.
    pub reclaimable_edges: u64,
}

impl CompactionStats {
    /// Build compaction counters for `graph` without rebuilding any rows.
    #[must_use]
    pub fn from_graph(graph: &SeleneGraph) -> Self {
        let allocated_nodes = graph.node_store.len() as u64;
        let live_nodes = graph.node_count() as u64;
        let allocated_edges = graph.edge_store.len() as u64;
        let live_edges = graph.edge_count() as u64;
        Self {
            allocated_nodes,
            live_nodes,
            reclaimable_nodes: allocated_nodes.saturating_sub(live_nodes),
            allocated_edges,
            live_edges,
            reclaimable_edges: allocated_edges.saturating_sub(live_edges),
        }
    }

    /// Total allocated node and edge rows.
    #[must_use]
    pub const fn allocated_rows(self) -> u64 {
        self.allocated_nodes.saturating_add(self.allocated_edges)
    }

    /// Total alive node and edge rows.
    #[must_use]
    pub const fn live_rows(self) -> u64 {
        self.live_nodes.saturating_add(self.live_edges)
    }

    /// Total reclaimable dead node and edge rows.
    #[must_use]
    pub const fn reclaimable_rows(self) -> u64 {
        self.reclaimable_nodes
            .saturating_add(self.reclaimable_edges)
    }

    /// True when no dead rows remain to compact.
    #[must_use]
    pub const fn is_dense(self) -> bool {
        self.reclaimable_nodes == 0 && self.reclaimable_edges == 0
    }

    /// Return reclaimable rows divided by allocated rows, scaled by 10,000.
    #[must_use]
    pub fn reclaimable_row_basis_points(self) -> u64 {
        let allocated = self.allocated_rows();
        if allocated == 0 {
            return 0;
        }
        let scaled = u128::from(self.reclaimable_rows()) * u128::from(BASIS_POINTS_DENOMINATOR);
        (scaled / u128::from(allocated)) as u64
    }

    /// Return true when current row-space pressure merits maintenance compaction.
    ///
    /// The recommendation is diagnostic only: reads and writes never compact
    /// automatically, and callers still decide when to run maintenance.
    #[must_use]
    pub fn compaction_recommended(self) -> bool {
        self.reclaimable_rows() >= COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_ROWS
            && self.reclaimable_row_basis_points()
                >= COMPACTION_RECOMMENDATION_MIN_RECLAIMABLE_BASIS_POINTS
    }
}

/// The product of compacting the CORE store.
pub struct CompactedCore {
    /// The dense, fully-rebuilt compacted graph (no dead/hole rows).
    pub graph: SeleneGraph,
    /// What CORE reclaimed.
    pub report: CompactionReport,
}

/// Compact `graph`: drop every dead / hole row, renumber rows dense (ascending
/// old-row order), and rebuild all derived state. External ids and the
/// allocator high-water marks are preserved verbatim.
///
/// # Errors
///
/// Returns [`GraphError`] if an alive row has no mapped external id (a corrupt
/// id↔row mapping) or if the rebuilt graph fails its consistency check.
pub fn compact_core(graph: &SeleneGraph) -> GraphResult<CompactedCore> {
    let mut nodes = NodeStore::new();
    let mut live_nodes: FxHashSet<NodeId> = FxHashSet::with_capacity_and_hasher(
        graph.node_store.alive.len() as usize,
        Default::default(),
    );
    for old_row in graph.node_store.alive_rows() {
        let r = old_row.index();
        let id = graph
            .node_id_for_node_row(old_row)
            .ok_or_else(|| GraphError::Inconsistent {
                reason: format!(
                    "alive node row {} has no external id during compaction",
                    old_row.get()
                ),
            })?;
        let column_missing = |col: &str| GraphError::Inconsistent {
            reason: format!(
                "node {col} column missing live row {} during compaction",
                old_row.get()
            ),
        };
        // Strict (fail-loud) reads, symmetric with the edge columns below: a
        // misaligned column on an alive row is corruption, not an empty row.
        nodes.labels.push(
            graph
                .node_store
                .labels
                .get(r)
                .cloned()
                .ok_or_else(|| column_missing("labels"))?,
        );
        nodes.properties.push(
            graph
                .node_store
                .properties
                .get(r)
                .cloned()
                .ok_or_else(|| column_missing("properties"))?,
        );
        nodes.row_to_id.push(id);
        live_nodes.insert(id);
    }
    let node_len = nodes.labels.len() as u32;
    // B1: `alive_mut` is free here — the store is freshly built, so its Arc is
    // unique and `make_mut` never clones.
    for new_row in 0..node_len {
        nodes.mark_alive(NodeRow::new(new_row));
    }

    let mut edges = EdgeStore::new();
    for old_row in graph.edge_store.alive_rows() {
        let r = old_row.index();
        let id = graph
            .edge_id_for_edge_row(old_row)
            .ok_or_else(|| GraphError::Inconsistent {
                reason: format!(
                    "alive edge row {} has no external id during compaction",
                    old_row.get()
                ),
            })?;
        let column_missing = |col: &str| GraphError::Inconsistent {
            reason: format!(
                "edge {col} column missing live row {} during compaction",
                old_row.get()
            ),
        };
        let source = graph
            .edge_store
            .source
            .get(r)
            .copied()
            .ok_or_else(|| column_missing("source"))?;
        let target = graph
            .edge_store
            .target
            .get(r)
            .copied()
            .ok_or_else(|| column_missing("target"))?;
        // Defense-in-depth: a surviving edge must reference surviving nodes. The
        // mutator's delete_node cascade guarantees this for engine-produced
        // graphs; reject a dangling edge (only constructible by a raw column
        // build) rather than rebuild adjacency keyed by a vanished node.
        if !live_nodes.contains(&source) || !live_nodes.contains(&target) {
            return Err(GraphError::Inconsistent {
                reason: format!(
                    "edge {id} survives compaction but endpoint {source} or {target} does not"
                ),
            });
        }
        edges.label.push(
            graph
                .edge_store
                .label
                .get(r)
                .cloned()
                .ok_or_else(|| column_missing("label"))?,
        );
        edges.source.push(source);
        edges.directionality.push(
            graph
                .edge_store
                .directionality
                .get(r)
                .copied()
                .ok_or_else(|| column_missing("directionality"))?,
        );
        edges.target.push(target);
        edges.properties.push(
            graph
                .edge_store
                .properties
                .get(r)
                .cloned()
                .ok_or_else(|| column_missing("properties"))?,
        );
        edges.row_to_id.push(id);
    }
    let edge_len = edges.label.len() as u32;
    // B1: free `make_mut` on a freshly built store (see node loop above).
    for new_row in 0..edge_len {
        edges.mark_alive(EdgeRow::new(new_row));
    }

    let stats = CompactionStats::from_graph(graph);
    let report = CompactionReport {
        reclaimed_nodes: stats.reclaimable_nodes,
        reclaimed_edges: stats.reclaimable_edges,
    };

    // Assemble the dense graph. meta is preserved VERBATIM — the monotonic
    // next_node_id/next_edge_id high-water marks must NOT be recomputed from the
    // shrunken row count, or a compacted-away external id could be reused
    // (violating D11/D22). bound_type + generation carry through unchanged.
    let mut dense = SeleneGraph::new(graph.meta.graph_id);
    dense.meta = graph.meta.clone();
    dense.node_store = nodes;
    dense.edge_store = edges;

    // Carry over property-index *registrations* (which (label, property) are
    // indexed + their kind/name) with fresh empty indexes; the rebuilders below
    // refill the row bitmaps from the dense columns. Cloning the source entries
    // directly would copy stale OLD-row bitmaps.
    for ((label, property), entry) in &graph.property_index {
        dense.property_index.insert(
            (label.clone(), property.clone()),
            PropertyIndexEntry::new(TypedIndex::new(entry.kind()), entry.name.clone()),
        );
    }
    for ((label, property), entry) in &graph.edge_property_index {
        dense.edge_property_index.insert(
            (label.clone(), property.clone()),
            PropertyIndexEntry::new(TypedIndex::new(entry.kind()), entry.name.clone()),
        );
    }
    for (key, entry) in &graph.composite_property_index {
        dense.composite_property_index.insert(
            key.clone(),
            CompositePropertyIndexEntry::new(
                crate::CompositeTypedIndex::new(entry.kinds()),
                entry.declared_properties.clone(),
                entry.name.clone(),
            ),
        );
    }
    for ((label, property), entry) in &graph.vector_index {
        dense.vector_index.insert(
            (label.clone(), property.clone()),
            VectorIndexEntry::new(
                crate::VectorIndex::new_with_configs(
                    entry.kind(),
                    entry.dimension(),
                    entry.hnsw_config(),
                    entry.ivf_config(),
                )?,
                entry.name.clone(),
            ),
        );
    }
    for ((label, property), entry) in &graph.text_index {
        dense.text_index.insert(
            (label.clone(), property.clone()),
            crate::graph::TextIndexEntry::new(
                crate::TextIndex::empty(label.clone(), property.clone()),
                entry.name.clone(),
            ),
        );
    }

    // Rebuild every derived structure from the dense columns — the same chain
    // the logical snapshot-apply path uses when materializing a graph.
    crate::shared::rebuild_derived_state(&mut dense)?;
    crate::property_index::rebuild_property_indexes(&mut dense)?;
    crate::property_index::rebuild_edge_property_indexes(&mut dense)?;
    crate::composite_property_index::rebuild_composite_property_indexes(&mut dense)?;
    crate::vector_index::rebuild_vector_indexes(&mut dense)?;
    crate::text_index::rebuild_text_indexes(&mut dense)?;
    dense
        .rebind_catalog_after_rebuild(graph)
        .map_err(|error| GraphError::Inconsistent {
            reason: format!("compacted catalog binding failed validation: {error}"),
        })?;

    // Debug-only structural net (matches the snapshot-load publication seam):
    // re-derive every index from the compacted columns and confirm agreement.
    #[cfg(debug_assertions)]
    if let Err(reason) = dense.assert_indexes_consistent() {
        return Err(GraphError::Inconsistent {
            reason: format!("compacted graph failed consistency check: {reason}"),
        });
    }

    Ok(CompactedCore {
        graph: dense,
        report,
    })
}

#[cfg(test)]
mod schema_tests;
#[cfg(test)]
mod tests;
