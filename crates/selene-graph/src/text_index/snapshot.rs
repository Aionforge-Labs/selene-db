//! Snapshot-owned construction and candidate scoring.

use crate::{
    CandidateSet, GraphError, GraphResult, Node, SeleneGraph, SharedGraph, TextIndex,
    TextSearchError, TextSearchHit,
};
use selene_core::{CancellationChecker, DbString};

impl SeleneGraph {
    /// Build a reusable BM25 index from this snapshot's authoritative values.
    /// Returns a graph inconsistency if a live label row cannot be resolved.
    pub fn build_text_index(
        &self,
        label: &DbString,
        property: &DbString,
    ) -> GraphResult<TextIndex> {
        TextIndex::build(self, label.clone(), property.clone())
    }

    /// Build a transient postings index and search it. Intended for comparisons;
    /// production reads should use a maintained registration or the exact scan.
    pub fn indexed_text_search_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        query: &str,
        k: usize,
    ) -> GraphResult<Vec<TextSearchHit>> {
        Ok(self.build_text_index(label, property)?.search(query, k))
    }

    /// Score snapshot-bound candidates using the registered index's full corpus.
    /// Validates graph, generation, layout and workspace even for empty candidates
    /// or zero k. Never builds an index on a read path. Missing/ineligible indexes
    /// and foreign candidate identities return graph inconsistencies.
    pub fn score_text_candidates_checked(
        &self,
        label: &DbString,
        property: &DbString,
        query: &str,
        candidates: &CandidateSet<Node>,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<TextSearchHit>, TextSearchError> {
        candidates
            .validate_identity_for(self)
            .map_err(|error| GraphError::Inconsistent {
                reason: format!("invalid text candidates: {error}"),
            })?;
        let index =
            self.text_index_for(label, property)
                .ok_or_else(|| GraphError::Inconsistent {
                    reason: format!("no usable text index for {label}.{property}"),
                })?;
        let nodes: Vec<_> = candidates.iter().collect();
        index.search_candidates_checked(query, &nodes, k, checker)
    }
}

impl SharedGraph {
    /// Build a reusable BM25 index from the current pinned snapshot.
    pub fn build_text_index(
        &self,
        label: &DbString,
        property: &DbString,
    ) -> GraphResult<TextIndex> {
        self.read().build_text_index(label, property)
    }

    /// Build a transient index from the current snapshot and search it.
    pub fn indexed_text_search_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        query: &str,
        k: usize,
    ) -> GraphResult<Vec<TextSearchHit>> {
        self.read()
            .indexed_text_search_nodes(label, property, query, k)
    }
}
