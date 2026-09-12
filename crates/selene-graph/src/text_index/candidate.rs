//! Candidate-scoped BM25 scoring for maintained text indexes.
//!
//! The standalone index accepts stable IDs from its own corpus. Graph callers
//! use `SeleneGraph::score_text_candidates_checked` to validate snapshot identity.

use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use selene_core::{CancellationChecker, NodeId};

use super::{QueryDocumentFrequencies, QueryPostings, TextIndex, TextPosting};
use crate::text_search::{
    DocumentStats, TEXT_SEARCH_CANCEL_STRIDE, TextSearchError, TextSearchHit, TextTopK, bm25_score,
    unique_query_terms,
};

impl TextIndex {
    /// Rank explicit node candidates for `query` using this index's BM25 corpus stats.
    ///
    /// Candidate ids are deduplicated and missing/non-indexed ids are ignored.
    /// Term document frequencies and average document length remain global to
    /// the maintained index, so this returns the same ordering as a full
    /// [`Self::search`] followed by candidate filtering when enough global hits
    /// are materialized.
    #[must_use]
    pub fn search_candidates(
        &self,
        query: &str,
        candidates: &[NodeId],
        k: usize,
    ) -> Vec<TextSearchHit> {
        self.search_candidates_checked(query, candidates, k, CancellationChecker::disabled())
            .expect("disabled text-index checker cannot fail")
    }

    /// Rank explicit node candidates for `query` with cooperative cancellation checks.
    ///
    /// # Errors
    ///
    /// Returns [`TextSearchError::Cancelled`], [`TextSearchError::Timeout`], or
    /// [`TextSearchError::NodeScanBudgetExceeded`] when the supplied checker
    /// trips while deduplicating or scoring candidates.
    pub fn search_candidates_checked(
        &self,
        query: &str,
        candidates: &[NodeId],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<TextSearchHit>, TextSearchError> {
        self.validate_contract()?;
        checker.check()?;
        if k == 0 || candidates.is_empty() || self.document_lengths.is_empty() {
            return Ok(Vec::new());
        }
        let query_terms = unique_query_terms(query);
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }

        let (document_frequencies, postings_by_term) = self.query_postings(&query_terms);
        if postings_by_term.iter().all(Option::is_none) {
            return Ok(Vec::new());
        }
        let corpus_len = self.document_lengths.len() as f64;
        let average_document_len = self.total_document_len as f64 / corpus_len;
        let scoring = CandidateScoreInputs {
            postings_by_term: &postings_by_term,
            document_frequencies: &document_frequencies,
            corpus_len,
            average_document_len,
            k,
        };
        let candidates_are_strictly_ascending = node_ids_strictly_ascending(candidates);
        if checker.is_disabled()
            && candidates_are_strictly_ascending
            && self.ascending_candidates_cover_index(candidates)
        {
            return Ok(self.search(query, k));
        }
        if candidates_are_strictly_ascending {
            return self.score_sorted_indexed_candidates(candidates, scoring, checker);
        }

        let candidate_set = self.indexed_candidate_set(candidates, checker)?;
        if candidate_set.is_empty() {
            return Ok(Vec::new());
        }
        if checker.is_disabled() && candidate_set.len() == self.document_lengths.len() {
            return Ok(self.search(query, k));
        }
        self.score_indexed_candidates(candidate_set, scoring, checker)
    }

    fn score_sorted_indexed_candidates(
        &self,
        candidates: &[NodeId],
        scoring: CandidateScoreInputs<'_>,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<TextSearchHit>, TextSearchError> {
        let mut top_k = TextTopK::new(scoring.k);
        let mut postings_cursors =
            SmallVec::<[usize; 4]>::from_elem(0, scoring.postings_by_term.len());
        let mut candidates_since_check = 0usize;
        for &node_id in candidates {
            candidates_since_check += 1;
            if candidates_since_check >= TEXT_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(candidates_since_check)?;
                candidates_since_check = 0;
            }
            let Some(&len) = self.document_lengths.get(&node_id) else {
                continue;
            };
            let Some(doc) = sorted_candidate_document_stats(
                node_id,
                len,
                scoring.postings_by_term,
                &mut postings_cursors,
            ) else {
                continue;
            };
            let score = bm25_score(
                &doc,
                scoring.document_frequencies,
                scoring.corpus_len,
                scoring.average_document_len,
            );
            if score > 0.0 {
                top_k.push(node_id, score);
            }
        }
        if candidates_since_check > 0 {
            checker.note_nodes_scanned(candidates_since_check)?;
        }
        Ok(top_k.into_hits())
    }

    fn score_indexed_candidates(
        &self,
        candidates: impl IntoIterator<Item = NodeId>,
        scoring: CandidateScoreInputs<'_>,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<TextSearchHit>, TextSearchError> {
        let mut top_k = TextTopK::new(scoring.k);
        let mut candidates_since_check = 0usize;
        for node_id in candidates {
            candidates_since_check += 1;
            if candidates_since_check >= TEXT_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(candidates_since_check)?;
                candidates_since_check = 0;
            }
            let Some(&len) = self.document_lengths.get(&node_id) else {
                continue;
            };
            let Some(doc) = candidate_document_stats(node_id, len, scoring.postings_by_term) else {
                continue;
            };
            let score = bm25_score(
                &doc,
                scoring.document_frequencies,
                scoring.corpus_len,
                scoring.average_document_len,
            );
            if score > 0.0 {
                top_k.push(node_id, score);
            }
        }
        if candidates_since_check > 0 {
            checker.note_nodes_scanned(candidates_since_check)?;
        }
        Ok(top_k.into_hits())
    }

    fn indexed_candidate_set(
        &self,
        candidates: &[NodeId],
        checker: CancellationChecker<'_>,
    ) -> Result<FxHashSet<NodeId>, TextSearchError> {
        let mut set = FxHashSet::default();
        set.reserve(candidates.len().min(self.document_lengths.len()));
        let mut candidates_since_check = 0usize;
        for &candidate in candidates {
            candidates_since_check += 1;
            if candidates_since_check >= TEXT_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(candidates_since_check)?;
                candidates_since_check = 0;
            }
            if self.document_lengths.contains_key(&candidate) {
                set.insert(candidate);
            }
        }
        if candidates_since_check > 0 {
            checker.note_nodes_scanned(candidates_since_check)?;
        }
        Ok(set)
    }

    fn ascending_candidates_cover_index(&self, candidates: &[NodeId]) -> bool {
        candidates.len() == self.document_lengths.len()
            && candidates
                .iter()
                .all(|candidate| self.document_lengths.contains_key(candidate))
    }

    fn query_postings<'a>(
        &'a self,
        query_terms: &[String],
    ) -> (QueryDocumentFrequencies, QueryPostings<'a>) {
        let mut document_frequencies = QueryDocumentFrequencies::with_capacity(query_terms.len());
        let mut postings_by_term = QueryPostings::with_capacity(query_terms.len());
        for term in query_terms {
            match self.postings.get(term.as_str()) {
                Some(postings) => {
                    document_frequencies.push(u32::try_from(postings.len()).unwrap_or(u32::MAX));
                    postings_by_term.push(Some(postings.as_slice()));
                }
                None => {
                    document_frequencies.push(0);
                    postings_by_term.push(None);
                }
            }
        }
        (document_frequencies, postings_by_term)
    }
}

#[derive(Clone, Copy)]
struct CandidateScoreInputs<'a> {
    postings_by_term: &'a [Option<&'a [TextPosting]>],
    document_frequencies: &'a [u32],
    corpus_len: f64,
    average_document_len: f64,
    k: usize,
}

fn node_ids_strictly_ascending(nodes: &[NodeId]) -> bool {
    let Some((&first, rest)) = nodes.split_first() else {
        return true;
    };
    let mut previous = first;
    for &node in rest {
        if previous >= node {
            return false;
        }
        previous = node;
    }
    true
}

fn candidate_document_stats(
    node_id: NodeId,
    len: u32,
    postings_by_term: &[Option<&[TextPosting]>],
) -> Option<DocumentStats> {
    let mut doc = DocumentStats::zero(node_id, len, postings_by_term.len());
    let mut matched = false;
    for (term_index, postings) in postings_by_term.iter().enumerate() {
        let Some(postings) = postings else {
            continue;
        };
        if let Ok(index) = postings.binary_search_by_key(&node_id, |posting| posting.node_id) {
            doc.term_counts[term_index] = postings[index].term_count;
            matched = true;
        }
    }
    matched.then_some(doc)
}

fn sorted_candidate_document_stats(
    node_id: NodeId,
    len: u32,
    postings_by_term: &[Option<&[TextPosting]>],
    postings_cursors: &mut [usize],
) -> Option<DocumentStats> {
    let mut doc = DocumentStats::zero(node_id, len, postings_by_term.len());
    let mut matched = false;
    for (term_index, postings) in postings_by_term.iter().enumerate() {
        let Some(postings) = postings else {
            continue;
        };
        let cursor = &mut postings_cursors[term_index];
        while *cursor < postings.len() && postings[*cursor].node_id < node_id {
            *cursor += 1;
        }
        if *cursor < postings.len() && postings[*cursor].node_id == node_id {
            doc.term_counts[term_index] = postings[*cursor].term_count;
            matched = true;
        }
    }
    matched.then_some(doc)
}
