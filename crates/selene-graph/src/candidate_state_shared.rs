//! Graph-owned typed candidate producers and maintained-state shared helpers.

use std::ops::RangeBounds;

use roaring::RoaringBitmap;
use selene_core::{DbString, Value};

use crate::store::{EdgeRow, NodeRow};
use crate::{
    CANDIDATE_STATE_PROVIDER_TAG, CandidateSet, Edge, GraphError, GraphResult, Node, ProviderError,
    ProviderTag, SeleneGraph, SharedGraph, VectorCandidateSet, VectorCandidateStateInfo,
};

impl SeleneGraph {
    /// Resolve a provider result against this pinned snapshot, including empty sets.
    /// Invalid graph/generation/layout/workspace identity is an explicit failure,
    /// never converted to raw IDs and rebound to a different snapshot.
    #[doc(hidden)]
    pub fn maintained_node_candidates(
        &self,
        provider: &dyn crate::IndexProvider,
        name: &DbString,
    ) -> Result<Option<CandidateSet<Node>>, ProviderError> {
        let candidates = provider.node_candidate_set(name, self)?;
        if let Some(candidates) = &candidates {
            candidates.validate_identity_for(self).map_err(|error| {
                ProviderError::Inconsistent {
                    reason: format!("maintained candidate identity mismatch: {error}"),
                }
            })?;
        }
        Ok(candidates)
    }

    /// Build candidates for every node alive in this immutable snapshot.
    pub fn live_node_candidates(&self) -> GraphResult<CandidateSet<Node>> {
        self.node_candidates_from_rows(&self.node_store.alive, "live node store")
    }

    /// Build candidates for every edge alive in this immutable snapshot.
    pub fn live_edge_candidates(&self) -> GraphResult<CandidateSet<Edge>> {
        self.edge_candidates_from_rows(&self.edge_store.alive, "live edge store")
    }

    /// Build typed candidates for live nodes carrying `label`.
    pub fn node_candidates_with_label(&self, label: &DbString) -> GraphResult<CandidateSet<Node>> {
        match self.nodes_with_label(label) {
            Some(rows) => self.node_candidates_from_rows(rows, "node label index"),
            None => Ok(CandidateSet::from_node_rows(self, [])),
        }
    }

    /// Build typed candidates for live edges carrying `label`.
    pub fn edge_candidates_with_label(&self, label: &DbString) -> GraphResult<CandidateSet<Edge>> {
        match self.edges_with_label(label) {
            Some(rows) => self.edge_candidates_from_rows(rows, "edge label index"),
            None => Ok(CandidateSet::from_edge_rows(self, [])),
        }
    }

    /// Build typed node candidates matching an equality property-index probe.
    pub fn node_candidates_with_property_eq(
        &self,
        label: &DbString,
        property: &DbString,
        value: &Value,
    ) -> GraphResult<Option<CandidateSet<Node>>> {
        self.nodes_with_property_eq(label, property, value)
            .map(|rows| self.node_candidates_from_rows(rows.as_ref(), "node property index"))
            .transpose()
    }

    /// Build typed node candidates matching any equality property-index probe.
    pub fn node_candidates_with_property_any(
        &self,
        label: &DbString,
        property: &DbString,
        values: &[Value],
    ) -> GraphResult<Option<CandidateSet<Node>>> {
        self.nodes_with_property_any(label, property, values)
            .as_ref()
            .map(|rows| self.node_candidates_from_rows(rows, "node property index"))
            .transpose()
    }

    /// Build typed edge candidates matching an equality property-index probe.
    pub fn edge_candidates_with_property_eq(
        &self,
        label: &DbString,
        property: &DbString,
        value: &Value,
    ) -> GraphResult<Option<CandidateSet<Edge>>> {
        self.edges_with_property_eq(label, property, value)
            .map(|rows| self.edge_candidates_from_rows(rows.as_ref(), "edge property index"))
            .transpose()
    }

    /// Build typed edge candidates matching any equality property-index probe.
    pub fn edge_candidates_with_property_any(
        &self,
        label: &DbString,
        property: &DbString,
        values: &[Value],
    ) -> GraphResult<Option<CandidateSet<Edge>>> {
        self.edges_with_property_any(label, property, values)
            .as_ref()
            .map(|rows| self.edge_candidates_from_rows(rows, "edge property index"))
            .transpose()
    }

    /// Build typed node candidates matching a property-index range probe.
    pub fn node_candidates_with_property_range<R>(
        &self,
        label: &DbString,
        property: &DbString,
        range: R,
    ) -> GraphResult<Option<CandidateSet<Node>>>
    where
        R: RangeBounds<Value>,
    {
        self.nodes_with_property_range(label, property, range)
            .as_ref()
            .map(|rows| self.node_candidates_from_rows(rows, "node property index"))
            .transpose()
    }

    /// Build typed edge candidates matching a property-index range probe.
    pub fn edge_candidates_with_property_range<R>(
        &self,
        label: &DbString,
        property: &DbString,
        range: R,
    ) -> GraphResult<Option<CandidateSet<Edge>>>
    where
        R: RangeBounds<Value>,
    {
        self.edges_with_property_range(label, property, range)
            .as_ref()
            .map(|rows| self.edge_candidates_from_rows(rows, "edge property index"))
            .transpose()
    }

    /// Build typed node candidates matching a string-prefix property-index probe.
    pub fn node_candidates_with_property_prefix(
        &self,
        label: &DbString,
        property: &DbString,
        prefix: &str,
    ) -> GraphResult<Option<CandidateSet<Node>>> {
        self.nodes_with_property_prefix(label, property, prefix)
            .as_ref()
            .map(|rows| self.node_candidates_from_rows(rows, "node property index"))
            .transpose()
    }

    /// Build typed node candidates matching a composite property index probe.
    pub fn node_candidates_with_composite_key(
        &self,
        label: &DbString,
        properties: &[DbString],
        values: &[Value],
    ) -> GraphResult<Option<CandidateSet<Node>>> {
        let Some(index) = self.composite_property_index_for(label, properties) else {
            return Ok(None);
        };
        let refs: Vec<&Value> = values.iter().collect();
        let Ok(key) = index.key_from_values(&refs) else {
            return Ok(None);
        };
        match index.lookup_key(&key) {
            Some(rows) => Ok(Some(
                self.node_candidates_from_rows(rows, "composite property index")?,
            )),
            None => Ok(Some(CandidateSet::from_node_rows(self, []))),
        }
    }

    pub(crate) fn node_candidates_from_rows(
        &self,
        rows: &RoaringBitmap,
        source: &str,
    ) -> GraphResult<CandidateSet<Node>> {
        let mut entries = Vec::with_capacity(usize::try_from(rows.len()).unwrap_or(usize::MAX));
        for raw_row in rows.iter() {
            let row = NodeRow::new(raw_row);
            if !self.node_store.is_alive_row(row) {
                continue;
            }
            let id = self
                .node_id_for_node_row(row)
                .ok_or_else(|| GraphError::Inconsistent {
                    reason: format!("{source} live node row {raw_row} has no stable id"),
                })?;
            self.ensure_live_node_pair(id, row, source)?;
            entries.push((id, row));
        }
        Ok(CandidateSet::from_node_rows(self, entries))
    }

    pub(crate) fn edge_candidates_from_rows(
        &self,
        rows: &RoaringBitmap,
        source: &str,
    ) -> GraphResult<CandidateSet<Edge>> {
        let mut entries = Vec::with_capacity(usize::try_from(rows.len()).unwrap_or(usize::MAX));
        for raw_row in rows.iter() {
            let row = EdgeRow::new(raw_row);
            if !self.edge_store.is_alive_row(row) {
                continue;
            }
            let id = self
                .edge_id_for_edge_row(row)
                .ok_or_else(|| GraphError::Inconsistent {
                    reason: format!("{source} live edge row {raw_row} has no stable id"),
                })?;
            self.ensure_live_edge_pair(id, row, source)?;
            entries.push((id, row));
        }
        Ok(CandidateSet::from_edge_rows(self, entries))
    }
}

impl SharedGraph {
    /// Look up a maintained node candidate set bound to one pinned snapshot.
    ///
    /// `Ok(None)` means no maintained candidate-state provider is registered or
    /// the provider has no set named `name`. The returned typed set is bound to
    /// the exact graph identity, generation, physical layout, and workspace
    /// identity of the single snapshot captured by this call.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when provider state is stale or cannot bind to
    /// the captured snapshot.
    pub fn node_candidate_set(
        &self,
        name: &DbString,
    ) -> Result<Option<CandidateSet<Node>>, ProviderError> {
        let snapshot = self.read();
        let Some(provider) = self.index_provider_by_tag(ProviderTag(CANDIDATE_STATE_PROVIDER_TAG))
        else {
            return Ok(None);
        };
        snapshot.maintained_node_candidates(provider.as_ref(), name)
    }

    /// Look up a generation-checked maintained vector candidate set by name.
    ///
    /// `Ok(None)` means no maintained candidate-state provider is registered or
    /// the provider has no set named `name`. The returned set is tied to the
    /// same immutable snapshot generation used for the lookup.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the candidate-state provider is present
    /// but cannot prove it has applied through the current graph generation.
    pub fn vector_candidate_set(
        &self,
        name: &DbString,
    ) -> Result<Option<VectorCandidateSet>, ProviderError> {
        let snapshot = self.read();
        let Some(provider) = self.index_provider_by_tag(ProviderTag(CANDIDATE_STATE_PROVIDER_TAG))
        else {
            return Ok(None);
        };
        provider.vector_candidate_set(name, snapshot.meta.generation)
    }

    /// Return generation-checked metadata for maintained vector candidate states.
    ///
    /// An empty vector means no maintained candidate-state provider is
    /// registered for this graph. The metadata is tied to the same immutable
    /// graph snapshot that supplies the generation value, so callers can use the
    /// returned names immediately with vector candidate-state scoring.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the candidate-state provider is present
    /// but cannot prove it has applied through the current graph generation.
    pub fn vector_candidate_state_infos(
        &self,
    ) -> Result<Vec<VectorCandidateStateInfo>, ProviderError> {
        let snapshot = self.read();
        let Some(provider) = self.index_provider_by_tag(ProviderTag(CANDIDATE_STATE_PROVIDER_TAG))
        else {
            return Ok(Vec::new());
        };
        provider.vector_candidate_state_infos(snapshot.meta.generation)
    }
}
