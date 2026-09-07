//! Internal validated candidate views bound to one immutable graph snapshot.
//!
//! A validated candidate view cannot outlive its producing snapshot (lifetime
//! `'a` bound to [`SeleneGraph`]), cannot expose raw physical row positions or
//! storage row indices to callers, and guarantees that every entry has been
//! validated for graph ID, generation, physical snapshot-layout allocation,
//! workspace binding, and live forward/reverse ID pairing.

use std::cmp::Ordering;
use std::sync::Arc;

use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;
use selene_core::{
    CancellationChecker, DbString, EdgeId, JsonValue, LabelSet, NodeId, PropertyMap, VectorMetric,
    VectorTopK, VectorValue,
};

use crate::error::GraphError;
use crate::graph::SeleneGraph;
use crate::store::{EdgeRow, NodeRow};
use crate::vector_index::VectorIndexSearchHit;
use crate::vector_search::{VECTOR_SEARCH_CANCEL_STRIDE, VectorNodeSearchHit, VectorSearchError};

/// A single validated candidate node bound to an immutable graph snapshot.
///
/// Physical row storage positions are strictly private and never exposed.
/// Direct property access methods use the verified row position for O(1)
/// store lookup without requiring secondary hash map probing.
#[derive(Clone, Copy)]
pub(crate) struct ValidatedCandidateNode<'a> {
    graph: &'a SeleneGraph,
    node_id: NodeId,
    row: NodeRow,
}

impl<'a> std::fmt::Debug for ValidatedCandidateNode<'a> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidatedCandidateNode")
            .field("node_id", &self.node_id)
            .finish()
    }
}

impl<'a> ValidatedCandidateNode<'a> {
    pub(crate) fn new(graph: &'a SeleneGraph, node_id: NodeId, row: NodeRow) -> Self {
        Self {
            graph,
            node_id,
            row,
        }
    }

    /// Return the stable node ID.
    pub(crate) fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// Return the internal physical node row.
    pub(crate) const fn row(&self) -> NodeRow {
        self.row
    }

    /// Return node properties for this validated node.
    pub(crate) fn properties(&self) -> Result<&'a PropertyMap, GraphError> {
        self.graph
            .node_store
            .properties
            .get(self.row.index())
            .ok_or_else(|| GraphError::Inconsistent {
                reason: format!("node row {} has no property row", self.row.get()),
            })
    }

    /// Return the vector property value if present and vector-typed.
    pub(crate) fn vector_property(
        &self,
        property: &DbString,
    ) -> Result<Option<&'a VectorValue>, GraphError> {
        let properties = self.properties()?;
        Ok(match properties.get(property) {
            Some(selene_core::Value::Vector(vector)) => Some(vector),
            _ => None,
        })
    }

    /// Return the string property value if present and string-typed.
    pub(crate) fn string_property(
        &self,
        property: &DbString,
    ) -> Result<Option<&'a DbString>, GraphError> {
        let properties = self.properties()?;
        Ok(match properties.get(property) {
            Some(selene_core::Value::String(text)) => Some(text),
            _ => None,
        })
    }

    /// Return the JSON property value if present and JSON-typed.
    pub(crate) fn json_property(
        &self,
        property: &DbString,
    ) -> Result<Option<&'a JsonValue>, GraphError> {
        let properties = self.properties()?;
        Ok(match properties.get(property) {
            Some(selene_core::Value::Json(value)) => Some(value),
            _ => None,
        })
    }

    /// Return the label set for this validated node.
    pub(crate) fn labels(&self) -> Result<&'a LabelSet, GraphError> {
        self.graph
            .node_store
            .labels
            .get(self.row.index())
            .ok_or_else(|| GraphError::Inconsistent {
                reason: format!("node row {} has no label row", self.row.get()),
            })
    }

    /// Return whether this validated node has the specified label.
    pub(crate) fn has_label(&self, label: &DbString) -> Result<bool, GraphError> {
        self.labels().map(|labels| labels.contains(label))
    }

    /// Insert this validated candidate node into a text index builder.
    pub(crate) fn insert_into_text_index(
        &self,
        index: &mut crate::text_index::TextIndexBuilder,
        text: &str,
    ) {
        index.insert_document(self.row.get(), self.node_id, text);
    }
}

/// A validated collection of candidate nodes bound to an immutable graph snapshot.
///
/// Cannot outlive the validating snapshot (lifetime `'a` bound to [`SeleneGraph`]).
/// Exposes stable [`NodeId`] identifiers and direct property access; never exposes
/// internal physical row positions.
#[derive(Clone)]
pub(crate) struct ValidatedNodeCandidates<'a> {
    _graph: &'a SeleneGraph,
    items: Arc<[ValidatedCandidateNode<'a>]>,
}

impl<'a> std::fmt::Debug for ValidatedNodeCandidates<'a> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidatedNodeCandidates")
            .field("len", &self.items.len())
            .finish()
    }
}

impl<'a> ValidatedNodeCandidates<'a> {
    pub(crate) fn new(graph: &'a SeleneGraph, items: Arc<[ValidatedCandidateNode<'a>]>) -> Self {
        Self {
            _graph: graph,
            items,
        }
    }

    /// Return the number of validated candidate nodes.
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// Return true when there are no candidate nodes.
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Return a slice of the validated candidate nodes.
    pub(crate) fn as_slice(&self) -> &[ValidatedCandidateNode<'a>] {
        &self.items
    }

    /// Iterate stable node IDs in deterministic ascending order.
    pub(crate) fn node_ids(&self) -> impl ExactSizeIterator<Item = NodeId> + '_ {
        self.items.iter().map(|item| item.node_id)
    }

    /// Return whether two validated candidate sets share the same backing allocation.
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.items, &other.items)
    }

    /// Filter a TurboQuant index bitmap to only include rows present in this validated candidate set.
    pub(crate) fn filter_index_rows(
        &self,
        index_rows: &RoaringBitmap,
        checker: &CancellationChecker<'_>,
    ) -> Result<RoaringBitmap, VectorSearchError> {
        let mut rows = RoaringBitmap::new();
        let mut candidates_since_check = 0usize;
        for item in self.items.iter() {
            candidates_since_check += 1;
            if candidates_since_check >= VECTOR_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(candidates_since_check)?;
                candidates_since_check = 0;
            }
            let raw_row = item.row.get();
            if index_rows.contains(raw_row) {
                rows.insert(raw_row);
            }
        }
        if candidates_since_check > 0 {
            checker.note_nodes_scanned(candidates_since_check)?;
        }
        Ok(rows)
    }

    /// Map approximate nearest-neighbor index hit rows to validated candidate node search hits.
    pub(crate) fn resolve_ann_row_hits(
        &self,
        row_hits: Vec<VectorIndexSearchHit>,
        checker: &CancellationChecker<'_>,
    ) -> Result<Vec<VectorNodeSearchHit>, VectorSearchError> {
        let mut lookup = FxHashMap::with_capacity_and_hasher(self.items.len(), Default::default());
        for item in self.items.iter() {
            lookup.insert(item.row.get(), item.node_id);
        }
        let mut hits = Vec::with_capacity(row_hits.len());
        let mut needs_sort = false;
        let mut rows_since_check = 0usize;
        for hit in row_hits {
            rows_since_check += 1;
            if rows_since_check >= VECTOR_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(rows_since_check)?;
                rows_since_check = 0;
            }
            let Some(&node_id) = lookup.get(&hit.row) else {
                continue;
            };
            let node_hit = VectorNodeSearchHit {
                node_id,
                distance: hit.distance,
            };
            needs_sort |= hits
                .last()
                .is_some_and(|previous| compare_node_search_hit(previous, &node_hit).is_gt());
            hits.push(node_hit);
        }
        if rows_since_check > 0 {
            checker.note_nodes_scanned(rows_since_check)?;
        }
        if needs_sort {
            hits.sort_by(compare_node_search_hit);
        }
        Ok(hits)
    }

    /// Rerank approximate nearest-neighbor index hit rows against query vectors using exact distances.
    pub(crate) fn rerank_ann_row_hits(
        &self,
        property: &DbString,
        query: &VectorValue,
        metric: VectorMetric,
        k: usize,
        row_hits: Vec<VectorIndexSearchHit>,
        checker: &CancellationChecker<'_>,
    ) -> Result<Vec<VectorNodeSearchHit>, VectorSearchError> {
        let mut lookup = FxHashMap::with_capacity_and_hasher(self.items.len(), Default::default());
        for item in self.items.iter() {
            lookup.insert(item.row.get(), *item);
        }
        let scorer = metric.bind_query(query).map_err(GraphError::from)?;
        let mut top_k = VectorTopK::new(k);
        let mut rows_since_check = 0usize;
        for hit in row_hits {
            rows_since_check += 1;
            if rows_since_check >= VECTOR_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(rows_since_check)?;
                rows_since_check = 0;
            }
            let Some(item) = lookup.get(&hit.row) else {
                continue;
            };
            let Some(vector) = item.vector_property(property)? else {
                continue;
            };
            let distance = scorer.distance(vector).map_err(GraphError::from)?;
            top_k.push_distance(item.node_id(), distance);
        }
        if rows_since_check > 0 {
            checker.note_nodes_scanned(rows_since_check)?;
        }
        Ok(top_k
            .into_hits()
            .into_iter()
            .map(|hit| VectorNodeSearchHit {
                node_id: hit.key,
                distance: hit.distance,
            })
            .collect())
    }
}

/// A single validated candidate edge bound to an immutable graph snapshot.
#[derive(Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ValidatedCandidateEdge<'a> {
    graph: &'a SeleneGraph,
    edge_id: EdgeId,
    row: EdgeRow,
}

impl<'a> std::fmt::Debug for ValidatedCandidateEdge<'a> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidatedCandidateEdge")
            .field("edge_id", &self.edge_id)
            .finish()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl<'a> ValidatedCandidateEdge<'a> {
    pub(crate) fn new(graph: &'a SeleneGraph, edge_id: EdgeId, row: EdgeRow) -> Self {
        Self {
            graph,
            edge_id,
            row,
        }
    }

    /// Return the stable edge ID.
    pub(crate) fn edge_id(&self) -> EdgeId {
        self.edge_id
    }

    /// Return the edge label.
    pub(crate) fn label(&self) -> Result<&'a DbString, GraphError> {
        self.graph
            .edge_store
            .label
            .get(self.row.index())
            .ok_or_else(|| GraphError::Inconsistent {
                reason: format!("edge row {} has no label row", self.row.get()),
            })
    }

    /// Return the source and target node endpoints.
    pub(crate) fn endpoints(&self) -> Result<(NodeId, NodeId), GraphError> {
        let source = self
            .graph
            .edge_store
            .source
            .get(self.row.index())
            .ok_or_else(|| GraphError::Inconsistent {
                reason: format!("edge row {} has no source endpoint", self.row.get()),
            })?;
        let target = self
            .graph
            .edge_store
            .target
            .get(self.row.index())
            .ok_or_else(|| GraphError::Inconsistent {
                reason: format!("edge row {} has no target endpoint", self.row.get()),
            })?;
        Ok((*source, *target))
    }

    /// Return edge properties for this validated edge.
    pub(crate) fn properties(&self) -> Result<&'a PropertyMap, GraphError> {
        self.graph
            .edge_store
            .properties
            .get(self.row.index())
            .ok_or_else(|| GraphError::Inconsistent {
                reason: format!("edge row {} has no property row", self.row.get()),
            })
    }
}

/// A validated collection of candidate edges bound to an immutable graph snapshot.
#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ValidatedEdgeCandidates<'a> {
    _graph: &'a SeleneGraph,
    items: Arc<[ValidatedCandidateEdge<'a>]>,
}

impl<'a> std::fmt::Debug for ValidatedEdgeCandidates<'a> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidatedEdgeCandidates")
            .field("len", &self.items.len())
            .finish()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl<'a> ValidatedEdgeCandidates<'a> {
    pub(crate) fn new(graph: &'a SeleneGraph, items: Arc<[ValidatedCandidateEdge<'a>]>) -> Self {
        Self {
            _graph: graph,
            items,
        }
    }

    /// Return the number of validated candidate edges.
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// Return true when there are no candidate edges.
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Return a slice of the validated candidate edges.
    pub(crate) fn as_slice(&self) -> &[ValidatedCandidateEdge<'a>] {
        &self.items
    }

    /// Iterate stable edge IDs in deterministic ascending order.
    pub(crate) fn edge_ids(&self) -> impl ExactSizeIterator<Item = EdgeId> + '_ {
        self.items.iter().map(|item| item.edge_id)
    }

    /// Return whether two validated candidate sets share the same backing allocation.
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.items, &other.items)
    }
}

fn compare_node_search_hit(lhs: &VectorNodeSearchHit, rhs: &VectorNodeSearchHit) -> Ordering {
    lhs.distance
        .total_cmp(&rhs.distance)
        .then_with(|| lhs.node_id.cmp(&rhs.node_id))
}
