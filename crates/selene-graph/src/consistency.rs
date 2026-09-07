//! Debug-only structural consistency net for built-in graph indexes.
//!
//! Every selene-graph derived index — the label / edge-label bitmaps, the
//! per-`(label, property)` typed indexes, the composite typed indexes, and the
//! in/out adjacency maps — is maintained incrementally on the commit path
//! (`crate::mutator`) and rebuilt wholesale on the snapshot-load /
//! recovery path (`crate::shared::rebuild_derived_state` +
//! `crate::property_index::rebuild_property_indexes` +
//! `crate::property_index::rebuild_edge_property_indexes` +
//! `crate::composite_property_index::rebuild_composite_property_indexes` +
//! `crate::vector_index::rebuild_vector_indexes` +
//! `crate::text_index::rebuild_text_indexes`). A
//! bug in either path corrupts query results silently — the engine keeps
//! answering, just with wrong rows.
//!
//! [`SeleneGraph::assert_indexes_consistent`] re-derives every index from the
//! authoritative columns (`node_store`, `edge_store`) and compares it against
//! the maintained state, returning a descriptive [`Err`] on the first
//! mismatch. It is wired behind `#[cfg(debug_assertions)]` at the two graph
//! publication seams (the write-transaction commit and the snapshot-load
//! constructor) so every debug/test build self-checks each published snapshot
//! at zero release cost.

use immutable_chunkmap::map::MapM;
use roaring::RoaringBitmap;

use selene_core::{DbString, EdgeId, NodeId};

use crate::adjacency::AdjacencyEdge;
use crate::graph::SeleneGraph;
use crate::id_map::{EngineIdMap, engine_id_map, get_or_insert_default};
use crate::store::{EdgeRow, NodeRow};
use crate::vector_index::VectorIndexConfig;

impl SeleneGraph {
    /// Re-derive every built-in index from the authoritative node/edge columns
    /// and assert it matches the maintained index state.
    ///
    /// Returns `Ok(())` when the snapshot is internally consistent, or
    /// `Err(message)` describing the first detected drift. Intended for
    /// debug assertions and tests — it allocates fresh reference indexes and
    /// is `O(nodes * indexes + edges)` per call.
    ///
    /// # Checked invariant families
    ///
    /// 1. **Label / edge-label bitmaps** match a re-derivation from alive node
    ///    label sets and alive edge labels; no bucket is present-but-empty.
    /// 2. **Typed property indexes** match a fresh lenient re-build
    ///    (`build_property_index_lenient`). The lenient policy is reused so
    ///    open-graph kind drift and NaN — which the commit path legitimately
    ///    skips — do not false-positive.
    /// 3. **Composite typed indexes** match a fresh lenient re-build, same
    ///    skip-aware policy.
    /// 4. **Vector row-set indexes** match a fresh lenient re-build, same
    ///    skip-aware policy.
    /// 5. **Text BM25 indexes** match a fresh re-build from string properties.
    /// 6. **Store integrity / alive-set parity**: per-store columns share one
    ///    length and every alive row index is in range. Dead rows are
    ///    permitted holes (D11) and are only asserted absent from derived
    ///    state, never from the columns. The snapshot's `meta.next_*_id`
    ///    allocator high-water marks are intentionally **not** checked here:
    ///    the published snapshot's `meta` is allowed to carry stale allocator
    ///    fields after a `from_graph` / recovery load (the real allocator
    ///    floor is enforced separately by `IdAllocator::from_meta_with_floors`),
    ///    so they are not a derived-index invariant.
    /// 7. **Adjacency** matches a re-derivation from alive edges in both
    ///    directions, with no present-but-empty entry.
    ///
    /// # Errors
    ///
    /// Returns the first mismatch as a human-readable `String`.
    pub fn assert_indexes_consistent(&self) -> Result<(), String> {
        self.check_store_integrity()?;
        self.check_typed_id_maps()?;
        self.check_label_index()?;
        self.check_edge_label_index()?;
        self.check_property_indexes()?;
        self.check_edge_property_indexes()?;
        self.check_composite_property_indexes()?;
        self.check_vector_indexes()?;
        self.check_text_indexes()?;
        self.check_adjacency()?;
        Ok(())
    }

    /// Family (4): column lengths agree, alive rows are in range, allocator
    /// floors clear the highest alive row.
    fn check_store_integrity(&self) -> Result<(), String> {
        let node_len = self.node_store.labels.len();
        if self.node_store.properties.len() != node_len
            || self.node_store.row_to_id.len() != node_len
        {
            return Err(format!(
                "node store column length mismatch: labels={node_len} properties={} row_to_id={}",
                self.node_store.properties.len(),
                self.node_store.row_to_id.len()
            ));
        }
        let edge_len = self.edge_store.label.len();
        if self.edge_store.source.len() != edge_len
            || self.edge_store.target.len() != edge_len
            || self.edge_store.properties.len() != edge_len
            || self.edge_store.row_to_id.len() != edge_len
        {
            return Err(format!(
                "edge store column length mismatch: label={edge_len} source={} target={} \
                 properties={} row_to_id={}",
                self.edge_store.source.len(),
                self.edge_store.target.len(),
                self.edge_store.properties.len(),
                self.edge_store.row_to_id.len()
            ));
        }

        for row in self.node_store.alive_rows() {
            if row.index() >= node_len {
                return Err(format!(
                    "node alive bitmap references out-of-range row {} (node store len {node_len})",
                    row.get()
                ));
            }
        }
        for row in self.edge_store.alive_rows() {
            if row.index() >= edge_len {
                return Err(format!(
                    "edge alive bitmap references out-of-range row {} (edge store len {edge_len})",
                    row.get()
                ));
            }
        }
        Ok(())
    }

    fn check_typed_id_maps(&self) -> Result<(), String> {
        for (id, row) in &self.node_rows {
            if row.index() >= self.node_store.len() || self.node_id_for_node_row(*row) != Some(*id)
            {
                return Err(format!(
                    "typed node inverse map has invalid binding {id} -> {}",
                    row.get()
                ));
            }
        }
        for (id, row) in &self.edge_rows {
            if row.index() >= self.edge_store.len() || self.edge_id_for_edge_row(*row) != Some(*id)
            {
                return Err(format!(
                    "typed edge inverse map has invalid binding {id} -> {}",
                    row.get()
                ));
            }
        }
        for (index, id) in self.node_store.row_to_id.iter().enumerate() {
            if *id != NodeId::TOMBSTONE
                && self.node_rows.get(id).copied() != Some(NodeRow::new(index as u32))
            {
                return Err(format!(
                    "node row {index} has no typed inverse binding for {id}"
                ));
            }
        }
        for (index, id) in self.edge_store.row_to_id.iter().enumerate() {
            if *id != EdgeId::TOMBSTONE
                && self.edge_rows.get(id).copied() != Some(EdgeRow::new(index as u32))
            {
                return Err(format!(
                    "edge row {index} has no typed inverse binding for {id}"
                ));
            }
        }
        Ok(())
    }

    /// Family (1a): node label bitmaps.
    fn check_label_index(&self) -> Result<(), String> {
        let mut reference: MapM<DbString, RoaringBitmap> = MapM::new();
        for row in self.node_store.alive_rows() {
            let Some(labels) = self.node_store.labels.get(row.index()) else {
                return Err(format!(
                    "alive node row {} has no label column entry",
                    row.get()
                ));
            };
            for label in labels.iter() {
                get_or_insert_default(&mut reference, label.clone()).insert(row.get());
            }
        }
        compare_bitmap_index("node label index", &self.idx_label, &reference)
    }

    /// Family (1b): edge label bitmaps.
    fn check_edge_label_index(&self) -> Result<(), String> {
        let mut reference: MapM<DbString, RoaringBitmap> = MapM::new();
        for row in self.edge_store.alive_rows() {
            let Some(label) = self.edge_store.label.get(row.index()) else {
                return Err(format!(
                    "alive edge row {} has no label column entry",
                    row.get()
                ));
            };
            get_or_insert_default(&mut reference, label.clone()).insert(row.get());
        }
        compare_bitmap_index("edge label index", &self.idx_edge_label, &reference)
    }

    /// Family (2): per-`(label, property)` typed indexes.
    fn check_property_indexes(&self) -> Result<(), String> {
        for ((label, property), entry) in &self.property_index {
            if entry.index.has_empty_bucket() {
                return Err(format!(
                    "property index ({label}, {property}) holds a present-but-empty bucket"
                ));
            }
            let reference = crate::property_index::build_property_index_lenient(
                self,
                label.clone(),
                property.clone(),
                entry.kind(),
            )
            .map_err(|err| {
                format!("failed to re-derive property index ({label}, {property}): {err}")
            })?;
            if entry.drifted_rows != reference.drifted_rows {
                return Err(format!(
                    "property index ({label}, {property}) records {} unkeyable rows but a fresh \
                     re-derivation found {}; an over-count disables the index for good and an \
                     under-count lets it answer while rows are missing",
                    entry.drifted_rows, reference.drifted_rows,
                ));
            }
            if !entry.index.buckets_eq(&reference.index) {
                return Err(format!(
                    "property index ({label}, {property}) drifted from a fresh re-derivation \
                     (maintained cardinality {}, reference cardinality {})",
                    entry.index.cardinality(),
                    reference.index.cardinality(),
                ));
            }
        }
        Ok(())
    }

    /// Family (2b): edge typed property indexes.
    fn check_edge_property_indexes(&self) -> Result<(), String> {
        for ((label, property), entry) in &self.edge_property_index {
            if entry.index.has_empty_bucket() {
                return Err(format!(
                    "edge property index ({label}, {property}) holds a present-but-empty bucket"
                ));
            }
            let reference = crate::property_index::build_edge_property_index_lenient(
                self,
                label.clone(),
                property.clone(),
                entry.kind(),
            )
            .map_err(|err| {
                format!("failed to re-derive edge property index ({label}, {property}): {err}")
            })?;
            if entry.drifted_rows != reference.drifted_rows {
                return Err(format!(
                    "edge property index ({label}, {property}) records {} unkeyable rows but a \
                     fresh re-derivation found {}",
                    entry.drifted_rows, reference.drifted_rows,
                ));
            }
            if !entry.index.buckets_eq(&reference.index) {
                return Err(format!(
                    "edge property index ({label}, {property}) drifted from a fresh \
                     re-derivation (maintained cardinality {}, reference cardinality {})",
                    entry.index.cardinality(),
                    reference.index.cardinality(),
                ));
            }
        }
        Ok(())
    }

    /// Family (3): composite typed indexes.
    fn check_composite_property_indexes(&self) -> Result<(), String> {
        for ((label, _key), entry) in &self.composite_property_index {
            if entry.index.has_empty_bucket() {
                return Err(format!(
                    "composite index ({label}, {:?}) holds a present-but-empty bucket",
                    entry.declared_properties
                ));
            }
            let reference =
                crate::composite_property_index::build_composite_property_index_lenient(
                    self,
                    label.clone(),
                    entry.declared_properties.clone(),
                    entry.kinds(),
                )
                .map_err(|err| {
                    format!(
                        "failed to re-derive composite index ({label}, {:?}): {err}",
                        entry.declared_properties
                    )
                })?;
            if entry.drifted_rows != reference.drifted_rows {
                return Err(format!(
                    "composite index ({label}, {:?}) records {} unkeyable rows but a fresh \
                     re-derivation found {}",
                    entry.declared_properties, entry.drifted_rows, reference.drifted_rows,
                ));
            }
            if !entry.index.buckets_eq(&reference.index) {
                return Err(format!(
                    "composite index ({label}, {:?}) drifted from a fresh re-derivation \
                     (maintained cardinality {}, reference cardinality {})",
                    entry.declared_properties,
                    entry.index.cardinality(),
                    reference.index.cardinality(),
                ));
            }
        }
        Ok(())
    }

    /// Family (4): vector row-set indexes.
    fn check_vector_indexes(&self) -> Result<(), String> {
        for ((label, property), entry) in &self.vector_index {
            let reference = crate::vector_index::build_vector_index_lenient_with_configs(
                self,
                label.clone(),
                property.clone(),
                entry.kind(),
                entry.dimension(),
                VectorIndexConfig::new(entry.hnsw_config(), entry.ivf_config()),
            )
            .map_err(|err| {
                format!("failed to re-derive vector index ({label}, {property}): {err}")
            })?;
            if !entry.index.rows_eq(&reference) {
                return Err(format!(
                    "vector index ({label}, {property}) drifted from a fresh re-derivation \
                     (maintained cardinality {}, reference cardinality {})",
                    entry.index.cardinality(),
                    reference.cardinality(),
                ));
            }
        }
        Ok(())
    }

    /// Family (5): BM25 text indexes.
    fn check_text_indexes(&self) -> Result<(), String> {
        for ((label, property), entry) in &self.text_index {
            let reference = crate::TextIndex::build(self, label.clone(), property.clone())
                .map_err(|err| {
                    format!("failed to re-derive text index ({label}, {property}): {err}")
                })?;
            if !entry.index.rows_eq(&reference) {
                return Err(format!(
                    "text index ({label}, {property}) drifted from a fresh re-derivation \
                     (maintained documents {}, reference documents {})",
                    entry.index.document_count(),
                    reference.document_count(),
                ));
            }
        }
        Ok(())
    }

    /// Family (7): in/out adjacency.
    fn check_adjacency(&self) -> Result<(), String> {
        let mut out_reference: EngineIdMap<NodeId, Vec<AdjacencyEdge>> = engine_id_map();
        let mut in_reference: EngineIdMap<NodeId, Vec<AdjacencyEdge>> = engine_id_map();
        for row in self.edge_store.alive_rows() {
            let Some(edge_id) = self.edge_id_for_edge_row(row) else {
                return Err(format!(
                    "alive edge row {} has no mapped external id",
                    row.get()
                ));
            };
            let Some(label) = self.edge_store.label.get(row.index()).cloned() else {
                return Err(format!(
                    "alive edge row {} has no label column entry",
                    row.get()
                ));
            };
            let Some(source) = self.edge_store.source.get(row.index()).copied() else {
                return Err(format!(
                    "alive edge row {} has no source column entry",
                    row.get()
                ));
            };
            let Some(target) = self.edge_store.target.get(row.index()).copied() else {
                return Err(format!(
                    "alive edge row {} has no target column entry",
                    row.get()
                ));
            };
            get_or_insert_default(&mut out_reference, source).push(AdjacencyEdge {
                label: label.clone(),
                neighbor: target,
                edge_id,
            });
            get_or_insert_default(&mut in_reference, target).push(AdjacencyEdge {
                label,
                neighbor: source,
                edge_id,
            });
        }
        compare_adjacency("outgoing", &self.adjacency_out, &out_reference)?;
        compare_adjacency("incoming", &self.adjacency_in, &in_reference)?;
        Ok(())
    }
}

/// Compare a maintained `DbString`-keyed bitmap index against a re-derivation,
/// failing on any key difference, bitmap difference, or empty maintained
/// bucket.
fn compare_bitmap_index(
    name: &str,
    maintained: &MapM<DbString, RoaringBitmap>,
    reference: &MapM<DbString, RoaringBitmap>,
) -> Result<(), String> {
    for (label, bitmap) in maintained {
        if bitmap.is_empty() {
            return Err(format!(
                "{name}: key {label} holds a present-but-empty bitmap"
            ));
        }
        match reference.get(label) {
            None => {
                return Err(format!(
                    "{name}: key {label} is maintained but not re-derived (stale rows)"
                ));
            }
            Some(expected) if expected != bitmap => {
                return Err(format!(
                    "{name}: key {label} bitmap drifted (maintained {:?}, reference {:?})",
                    bitmap.iter().collect::<Vec<_>>(),
                    expected.iter().collect::<Vec<_>>()
                ));
            }
            Some(_) => {}
        }
    }
    for (label, _) in reference {
        if maintained.get(label).is_none() {
            return Err(format!(
                "{name}: key {label} is re-derived but missing from the maintained index"
            ));
        }
    }
    Ok(())
}

/// Compare a maintained adjacency map against a re-derivation. The maintained
/// entry is sorted by `(label, neighbor, edge_id)`; the reference is sorted to
/// match before comparison so parallel edges and ordering are both checked.
fn compare_adjacency(
    direction: &str,
    maintained: &EngineIdMap<NodeId, crate::adjacency::AdjacencyEntry>,
    reference: &EngineIdMap<NodeId, Vec<AdjacencyEdge>>,
) -> Result<(), String> {
    for (node, entry) in maintained {
        if entry.is_empty() {
            return Err(format!(
                "{direction} adjacency: node {node} holds a present-but-empty entry"
            ));
        }
        let maintained_edges: Vec<AdjacencyEdge> = entry.iter().cloned().collect();
        match reference.get(node) {
            None => {
                return Err(format!(
                    "{direction} adjacency: node {node} is maintained but has no alive edges \
                     (dangling adjacency)"
                ));
            }
            Some(expected) => {
                let mut expected_sorted = expected.clone();
                expected_sorted.sort_by_key(adjacency_sort_key);
                if maintained_edges != expected_sorted {
                    return Err(format!(
                        "{direction} adjacency: node {node} drifted (maintained {maintained_edges:?}, \
                         reference {expected_sorted:?})"
                    ));
                }
            }
        }
    }
    for (node, _) in reference {
        if maintained.get(node).is_none() {
            return Err(format!(
                "{direction} adjacency: node {node} has alive edges but is missing from the \
                 maintained adjacency map"
            ));
        }
    }
    Ok(())
}

fn adjacency_sort_key(edge: &AdjacencyEdge) -> (DbString, NodeId, EdgeId) {
    (edge.label.clone(), edge.neighbor, edge.edge_id)
}

#[cfg(test)]
#[path = "consistency_tests.rs"]
mod tests;
