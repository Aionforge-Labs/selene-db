//! Ephemeral typed candidate sets bound to one immutable graph layout.

use std::marker::PhantomData;
use std::sync::Arc;

use selene_core::{EdgeId, GraphId, NodeId};

use crate::error::{CandidateSetError, CandidateSetResult, GraphError, GraphResult};
use crate::graph::SeleneGraph;
use crate::store::{EdgeRow, NodeRow};
use crate::validated_candidates::{
    ValidatedCandidateEdge, ValidatedCandidateNode, ValidatedEdgeCandidates,
    ValidatedNodeCandidates,
};
use crate::vector_search::VectorCandidateSet;

mod sealed {
    pub trait Sealed {}
}

/// A sealed element kind supported by [`CandidateSet`].
///
/// The graph crate implements this trait only for [`Node`] and [`Edge`], so
/// candidate kinds cannot be extended with an untrusted physical-row meaning.
pub trait CandidateKind: sealed::Sealed + 'static {
    /// Stable public identifier observed when iterating this candidate kind.
    type Id: Copy + std::fmt::Debug + Eq + Ord;
}

/// Marker kind for node candidate sets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Node {}

impl sealed::Sealed for Node {}

impl CandidateKind for Node {
    type Id = NodeId;
}

/// Marker kind for edge candidate sets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edge {}

impl sealed::Sealed for Edge {}

impl CandidateKind for Edge {
    type Id = EdgeId;
}

#[derive(Debug)]
pub(crate) struct LayoutToken;

#[derive(Clone, Debug)]
pub(crate) struct SnapshotLayout {
    physical: Arc<LayoutToken>,
    workspace_binding: Arc<LayoutToken>,
}

impl SnapshotLayout {
    pub(crate) fn new() -> Self {
        let physical = Arc::new(LayoutToken);
        Self {
            workspace_binding: Arc::clone(&physical),
            physical,
        }
    }

    fn retained_physical(&self) -> Arc<LayoutToken> {
        Arc::clone(&self.physical)
    }

    fn retained_workspace_binding(&self) -> Arc<LayoutToken> {
        Arc::clone(&self.workspace_binding)
    }

    fn matches(&self, physical: &Arc<LayoutToken>, workspace_binding: &Arc<LayoutToken>) -> bool {
        Arc::ptr_eq(&self.physical, physical)
            && Arc::ptr_eq(&self.workspace_binding, workspace_binding)
    }

    fn remint_candidate_binding(&mut self) {
        self.workspace_binding = Arc::new(LayoutToken);
    }

    #[cfg(test)]
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.physical, &other.physical)
    }

    #[cfg(test)]
    fn physical_matches(&self, token: &Arc<LayoutToken>) -> bool {
        Arc::ptr_eq(&self.physical, token)
    }

    #[cfg(test)]
    fn workspace_binding_matches(&self, token: &Arc<LayoutToken>) -> bool {
        Arc::ptr_eq(&self.workspace_binding, token)
    }
}

struct CandidateRow<K> {
    raw: u32,
    kind: PhantomData<fn() -> K>,
}

impl<K> CandidateRow<K> {
    const fn new(raw: u32) -> Self {
        Self {
            raw,
            kind: PhantomData,
        }
    }
}

impl<K> Clone for CandidateRow<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for CandidateRow<K> {}

impl<K> std::fmt::Debug for CandidateRow<K> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<private physical row>")
    }
}

/// An immutable, non-serialized set of candidates from one graph snapshot.
///
/// A set is bound to the producing lower graph identity, immutable generation,
/// private physical snapshot-layout allocation, and private mutable-workspace
/// binding. Graph-owned algebra rejects any identity mismatch or stale
/// stable-ID/typed-row pairing rather than translating physical rows. Public
/// iteration is deterministic in ascending stable-ID order and never exposes
/// physical rows.
///
/// Node and edge sets cannot be mixed:
///
/// ```compile_fail,E0308
/// use selene_core::GraphId;
/// use selene_graph::SeleneGraph;
///
/// let graph = SeleneGraph::new(GraphId::new(1));
/// let nodes = graph.live_node_candidates().unwrap();
/// let edges = graph.live_edge_candidates().unwrap();
/// let _ = graph.union_candidates(&nodes, &edges);
/// ```
#[must_use]
pub struct CandidateSet<K: CandidateKind> {
    graph_id: GraphId,
    generation: u64,
    physical_layout: Arc<LayoutToken>,
    workspace_binding: Arc<LayoutToken>,
    entries: Arc<[(K::Id, CandidateRow<K>)]>,
    entry_is_current: fn(&SeleneGraph, K::Id, &CandidateRow<K>) -> bool,
}

impl<K: CandidateKind> Clone for CandidateSet<K> {
    fn clone(&self) -> Self {
        Self {
            graph_id: self.graph_id,
            generation: self.generation,
            physical_layout: Arc::clone(&self.physical_layout),
            workspace_binding: Arc::clone(&self.workspace_binding),
            entries: Arc::clone(&self.entries),
            entry_is_current: self.entry_is_current,
        }
    }
}

impl<K: CandidateKind> std::fmt::Debug for CandidateSet<K> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CandidateSet")
            .field("ids", &self.iter().collect::<Vec<_>>())
            .finish()
    }
}

impl<K: CandidateKind> CandidateSet<K> {
    /// Return the number of stable element IDs in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return true when the set contains no stable element IDs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return true when the set contains `id`.
    #[must_use]
    pub fn contains(&self, id: K::Id) -> bool {
        self.entries
            .binary_search_by_key(&id, |(candidate, _)| *candidate)
            .is_ok()
    }

    /// Iterate stable IDs in deterministic ascending order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = K::Id> + '_ {
        self.entries.iter().map(|(id, _)| *id)
    }

    fn from_sorted_entries(
        graph_id: GraphId,
        generation: u64,
        layout: &SnapshotLayout,
        entries: Vec<(K::Id, CandidateRow<K>)>,
        entry_is_current: fn(&SeleneGraph, K::Id, &CandidateRow<K>) -> bool,
    ) -> Self {
        debug_assert!(entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
        Self {
            graph_id,
            generation,
            physical_layout: layout.retained_physical(),
            workspace_binding: layout.retained_workspace_binding(),
            entries: entries.into(),
            entry_is_current,
        }
    }

    pub(crate) fn validate_identity_for(&self, graph: &SeleneGraph) -> CandidateSetResult<()> {
        if self.graph_id != graph.meta.graph_id {
            return Err(CandidateSetError::GraphMismatch {
                expected: graph.meta.graph_id,
                actual: self.graph_id,
            });
        }
        if self.generation != graph.meta.generation {
            return Err(CandidateSetError::GenerationMismatch {
                expected: graph.meta.generation,
                actual: self.generation,
            });
        }
        if !graph
            .layout
            .matches(&self.physical_layout, &self.workspace_binding)
        {
            return Err(CandidateSetError::LayoutMismatch);
        }
        Ok(())
    }

    pub(crate) fn validate_for(&self, graph: &SeleneGraph) -> CandidateSetResult<()> {
        self.validate_identity_for(graph)?;
        if !self
            .entries
            .iter()
            .all(|(id, row)| (self.entry_is_current)(graph, *id, row))
        {
            return Err(CandidateSetError::StaleEntry);
        }
        Ok(())
    }

    fn union(&self, other: &Self) -> Self {
        let mut entries = Vec::with_capacity(self.len().saturating_add(other.len()));
        merge_entries(&self.entries, &other.entries, |left, right| {
            match (left, right) {
                (Some(entry), None) | (None, Some(entry)) | (Some(entry), Some(_)) => {
                    entries.push(entry)
                }
                (None, None) => {}
            }
        });
        self.with_entries(entries)
    }

    fn intersection(&self, other: &Self) -> Self {
        let mut entries = Vec::with_capacity(self.len().min(other.len()));
        merge_entries(&self.entries, &other.entries, |left, right| {
            if let (Some(entry), Some(_)) = (left, right) {
                entries.push(entry);
            }
        });
        self.with_entries(entries)
    }

    fn difference(&self, other: &Self) -> Self {
        let mut entries = Vec::with_capacity(self.len());
        merge_entries(&self.entries, &other.entries, |left, right| {
            if let (Some(entry), None) = (left, right) {
                entries.push(entry);
            }
        });
        self.with_entries(entries)
    }

    fn with_entries(&self, entries: Vec<(K::Id, CandidateRow<K>)>) -> Self {
        Self {
            graph_id: self.graph_id,
            generation: self.generation,
            physical_layout: Arc::clone(&self.physical_layout),
            workspace_binding: Arc::clone(&self.workspace_binding),
            entries: entries.into(),
            entry_is_current: self.entry_is_current,
        }
    }

    #[cfg(test)]
    pub(crate) fn shares_physical_layout_with(&self, graph: &SeleneGraph) -> bool {
        graph.layout.physical_matches(&self.physical_layout)
    }

    #[cfg(test)]
    pub(crate) fn shares_workspace_binding_with(&self, graph: &SeleneGraph) -> bool {
        graph
            .layout
            .workspace_binding_matches(&self.workspace_binding)
    }
}

impl CandidateSet<Node> {
    pub(crate) fn from_node_rows(
        graph: &SeleneGraph,
        entries: impl IntoIterator<Item = (NodeId, NodeRow)>,
    ) -> Self {
        let mut entries = entries
            .into_iter()
            .map(|(id, row)| (id, CandidateRow::new(row.get())))
            .collect::<Vec<_>>();
        canonicalize_entries(&mut entries);
        Self::from_sorted_entries(
            graph.meta.graph_id,
            graph.meta.generation,
            &graph.layout,
            entries,
            node_entry_is_current,
        )
    }

    #[cfg(test)]
    pub(crate) fn physical_layout_weak(&self) -> std::sync::Weak<LayoutToken> {
        Arc::downgrade(&self.physical_layout)
    }

    #[cfg(test)]
    pub(crate) fn workspace_binding_weak(&self) -> std::sync::Weak<LayoutToken> {
        Arc::downgrade(&self.workspace_binding)
    }
}

impl CandidateSet<Edge> {
    pub(crate) fn from_edge_rows(
        graph: &SeleneGraph,
        entries: impl IntoIterator<Item = (EdgeId, EdgeRow)>,
    ) -> Self {
        let mut entries = entries
            .into_iter()
            .map(|(id, row)| (id, CandidateRow::new(row.get())))
            .collect::<Vec<_>>();
        canonicalize_entries(&mut entries);
        Self::from_sorted_entries(
            graph.meta.graph_id,
            graph.meta.generation,
            &graph.layout,
            entries,
            edge_entry_is_current,
        )
    }
}

fn node_entry_is_current(graph: &SeleneGraph, id: NodeId, candidate: &CandidateRow<Node>) -> bool {
    let row = NodeRow::new(candidate.raw);
    graph.node_row_for_id(id) == Some(row)
        && graph.node_id_for_node_row(row) == Some(id)
        && graph.node_store.is_alive_row(row)
}

fn edge_entry_is_current(graph: &SeleneGraph, id: EdgeId, candidate: &CandidateRow<Edge>) -> bool {
    let row = EdgeRow::new(candidate.raw);
    graph.edge_row_for_id(id) == Some(row)
        && graph.edge_id_for_edge_row(row) == Some(id)
        && graph.edge_store.is_alive_row(row)
}

impl SeleneGraph {
    /// Bind arbitrary stable node IDs to this immutable snapshot using liveness only.
    ///
    /// Inputs are sorted and deduplicated when needed. Tombstones, absent
    /// mappings, and dead rows are filtered without error; labels, properties,
    /// indexes, vector dimensions, and values are intentionally not inspected.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] when a mapped live row does not have
    /// the matching reverse stable-ID pairing.
    pub fn bind_node_candidates(
        &self,
        ids: impl IntoIterator<Item = NodeId>,
    ) -> GraphResult<CandidateSet<Node>> {
        let mut ids = ids.into_iter().collect::<Vec<_>>();
        canonicalize_ids(&mut ids);
        let mut entries = Vec::with_capacity(ids.len());
        for id in ids {
            if id == NodeId::TOMBSTONE {
                continue;
            }
            let Some(row) = self.node_row_for_id(id) else {
                continue;
            };
            if !self.node_store.is_alive_row(row) {
                continue;
            }
            self.ensure_live_node_pair(id, row, "stable node candidate")?;
            entries.push((id, row));
        }
        Ok(CandidateSet::from_node_rows(self, entries))
    }

    /// Bind arbitrary stable edge IDs to this immutable snapshot using liveness only.
    ///
    /// Inputs are sorted and deduplicated when needed. Tombstones, absent
    /// mappings, and dead rows are filtered without error; labels, properties,
    /// and indexes are intentionally not inspected.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] when a mapped live row does not have
    /// the matching reverse stable-ID pairing.
    pub fn bind_edge_candidates(
        &self,
        ids: impl IntoIterator<Item = EdgeId>,
    ) -> GraphResult<CandidateSet<Edge>> {
        let mut ids = ids.into_iter().collect::<Vec<_>>();
        canonicalize_ids(&mut ids);
        let mut entries = Vec::with_capacity(ids.len());
        for id in ids {
            if id == EdgeId::TOMBSTONE {
                continue;
            }
            let Some(row) = self.edge_row_for_id(id) else {
                continue;
            };
            if !self.edge_store.is_alive_row(row) {
                continue;
            }
            self.ensure_live_edge_pair(id, row, "stable edge candidate")?;
            entries.push((id, row));
        }
        Ok(CandidateSet::from_edge_rows(self, entries))
    }

    /// Bind an unbound stable-ID vector candidate set to this immutable snapshot.
    ///
    /// Canonical vector inputs avoid another sorting pass. Binding is
    /// liveness-only and does not require a vector property or index membership.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] for an inconsistent mapped live row.
    pub fn bind_vector_candidate_set(
        &self,
        candidates: &VectorCandidateSet,
    ) -> GraphResult<CandidateSet<Node>> {
        self.bind_node_candidates(candidates.as_nodes().iter().copied())
    }

    /// Return the union of two candidates bound to this exact snapshot identity.
    pub fn union_candidates<K: CandidateKind>(
        &self,
        left: &CandidateSet<K>,
        right: &CandidateSet<K>,
    ) -> CandidateSetResult<CandidateSet<K>> {
        left.validate_for(self)?;
        right.validate_for(self)?;
        Ok(left.union(right))
    }

    /// Return the intersection of candidates bound to this exact snapshot identity.
    pub fn intersect_candidates<K: CandidateKind>(
        &self,
        left: &CandidateSet<K>,
        right: &CandidateSet<K>,
    ) -> CandidateSetResult<CandidateSet<K>> {
        left.validate_for(self)?;
        right.validate_for(self)?;
        Ok(left.intersection(right))
    }

    /// Return IDs from `left` absent from `right`, requiring this exact snapshot identity.
    pub fn difference_candidates<K: CandidateKind>(
        &self,
        left: &CandidateSet<K>,
        right: &CandidateSet<K>,
    ) -> CandidateSetResult<CandidateSet<K>> {
        left.validate_for(self)?;
        right.validate_for(self)?;
        Ok(left.difference(right))
    }

    /// Validate a candidate node set against this immutable graph snapshot.
    ///
    /// Verifies graph ID, generation, physical layout token, workspace
    /// binding token, and current liveness pairing for all entries.
    ///
    /// # Errors
    ///
    /// Returns [`CandidateSetError`] if validation fails.
    pub(crate) fn validate_node_candidates<'a>(
        &'a self,
        candidates: &CandidateSet<Node>,
    ) -> CandidateSetResult<ValidatedNodeCandidates<'a>> {
        candidates.validate_for(self)?;
        let items: Vec<ValidatedCandidateNode<'a>> = candidates
            .entries
            .iter()
            .map(|(id, row)| ValidatedCandidateNode::new(self, *id, NodeRow::new(row.raw)))
            .collect();
        Ok(ValidatedNodeCandidates::new(self, items.into()))
    }

    /// Validate a candidate edge set against this immutable graph snapshot.
    ///
    /// Verifies graph ID, generation, physical layout token, workspace
    /// binding token, and current liveness pairing for all entries.
    ///
    /// # Errors
    ///
    /// Returns [`CandidateSetError`] if validation fails.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn validate_edge_candidates<'a>(
        &'a self,
        candidates: &CandidateSet<Edge>,
    ) -> CandidateSetResult<ValidatedEdgeCandidates<'a>> {
        candidates.validate_for(self)?;
        let items: Vec<ValidatedCandidateEdge<'a>> = candidates
            .entries
            .iter()
            .map(|(id, row)| ValidatedCandidateEdge::new(self, *id, EdgeRow::new(row.raw)))
            .collect();
        Ok(ValidatedEdgeCandidates::new(self, items.into()))
    }

    pub(crate) fn remint_layout(&mut self) {
        self.layout = SnapshotLayout::new();
    }

    pub(crate) fn remint_candidate_binding(&mut self) {
        self.layout.remint_candidate_binding();
    }

    pub(crate) fn ensure_live_node_pair(
        &self,
        id: NodeId,
        row: NodeRow,
        source: &str,
    ) -> GraphResult<()> {
        if self.node_id_for_node_row(row) != Some(id) {
            return Err(GraphError::Inconsistent {
                reason: format!(
                    "{source} live node {id} row {} has a different reverse id",
                    row.get()
                ),
            });
        }
        match self.node_row_for_id(id) {
            Some(forward) if forward == row => Ok(()),
            Some(forward) => Err(GraphError::Inconsistent {
                reason: format!(
                    "{source} live node {id} row {} disagrees with row {}",
                    row.get(),
                    forward.get()
                ),
            }),
            None => Err(GraphError::Inconsistent {
                reason: format!(
                    "{source} live node {id} row {} has no forward mapping",
                    row.get()
                ),
            }),
        }
    }

    pub(crate) fn ensure_live_edge_pair(
        &self,
        id: EdgeId,
        row: EdgeRow,
        source: &str,
    ) -> GraphResult<()> {
        if self.edge_id_for_edge_row(row) != Some(id) {
            return Err(GraphError::Inconsistent {
                reason: format!(
                    "{source} live edge {id} row {} has a different reverse id",
                    row.get()
                ),
            });
        }
        match self.edge_row_for_id(id) {
            Some(forward) if forward == row => Ok(()),
            Some(forward) => Err(GraphError::Inconsistent {
                reason: format!(
                    "{source} live edge {id} row {} disagrees with row {}",
                    row.get(),
                    forward.get()
                ),
            }),
            None => Err(GraphError::Inconsistent {
                reason: format!(
                    "{source} live edge {id} row {} has no forward mapping",
                    row.get()
                ),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn shares_layout_with(&self, other: &Self) -> bool {
        self.layout.ptr_eq(&other.layout)
    }
}

fn canonicalize_ids<Id: Ord>(ids: &mut Vec<Id>) {
    if ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        ids.sort_unstable();
        ids.dedup();
    }
}

fn canonicalize_entries<Id: Ord, Row>(entries: &mut Vec<(Id, Row)>) {
    if entries.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        entries.dedup_by(|left, right| left.0 == right.0);
    }
}

fn merge_entries<K: CandidateKind>(
    left: &[(K::Id, CandidateRow<K>)],
    right: &[(K::Id, CandidateRow<K>)],
    mut emit: impl FnMut(Option<(K::Id, CandidateRow<K>)>, Option<(K::Id, CandidateRow<K>)>),
) {
    let (mut left_index, mut right_index) = (0, 0);
    while left_index < left.len() || right_index < right.len() {
        match (left.get(left_index), right.get(right_index)) {
            (Some(left_entry), Some(right_entry)) if left_entry.0 < right_entry.0 => {
                emit(Some(*left_entry), None);
                left_index += 1;
            }
            (Some(left_entry), Some(right_entry)) if right_entry.0 < left_entry.0 => {
                emit(None, Some(*right_entry));
                right_index += 1;
            }
            (Some(left_entry), Some(right_entry)) => {
                debug_assert_eq!(left_entry.1.raw, right_entry.1.raw);
                emit(Some(*left_entry), Some(*right_entry));
                left_index += 1;
                right_index += 1;
            }
            (Some(left_entry), None) => {
                emit(Some(*left_entry), None);
                left_index += 1;
            }
            (None, Some(right_entry)) => {
                emit(None, Some(*right_entry));
                right_index += 1;
            }
            (None, None) => break,
        }
    }
}
