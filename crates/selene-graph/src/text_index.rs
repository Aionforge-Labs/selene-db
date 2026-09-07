//! Reusable BM25 postings indexes over graph node string properties.
//!
//! `TextIndex` is an in-memory maintained artifact for one `(label, property)`
//! pair. Durable state stores only the registration; postings are derived from
//! primary node rows at registration, recovery, compaction, and commit-time
//! maintenance. The exact scorer in [`crate::text_search`] remains the
//! correctness oracle; this module reuses the same tokenizer, IDF formula, and
//! top-k ordering.

use std::mem::size_of;
use std::sync::Arc;

use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use selene_core::{CancellationChecker, DbString, NodeId};

use crate::error::{GraphError, GraphResult};
use crate::graph::SeleneGraph;
use crate::shared::SharedGraph;
use crate::text_search::{
    DocumentStats, TextSearchError, TextSearchHit, TextTopK, bm25_score, tokenize_borrowed,
    unique_query_terms,
};

#[path = "text_index/builder.rs"]
mod builder;
#[path = "text_index/candidate.rs"]
mod candidate;
#[path = "text_index/maintenance.rs"]
mod maintenance;
#[path = "text_index/postings.rs"]
mod postings;
pub(crate) use builder::TextIndexBuilder;
use postings::{remove_posting, upsert_posting};

type QueryDocumentFrequencies = SmallVec<[u32; 4]>;
type QueryPostings<'a> = SmallVec<[Option<&'a [TextPosting]>; 4]>;
type InlineDocumentTermCounts = SmallVec<[(TextTerm, u32); 8]>;
type TextTerm = Arc<str>;

const INLINE_DOCUMENT_TERM_COUNT_CAPACITY: usize = 8;

pub(crate) use maintenance::{
    apply_node_create, apply_node_delete, apply_node_update, rebuild_text_indexes,
};

/// In-memory BM25 postings index for one node `(label, property)` pair.
#[derive(Clone, Debug)]
pub struct TextIndex {
    label: DbString,
    property: DbString,
    rows: RoaringBitmap,
    document_lengths: FxHashMap<NodeId, u32>,
    document_terms: FxHashMap<NodeId, Arc<[TextTerm]>>,
    postings: FxHashMap<TextTerm, Arc<Vec<TextPosting>>>,
    total_document_len: u64,
    posting_count: usize,
}

impl TextIndex {
    /// Build a postings index from the current graph snapshot.
    ///
    /// Only alive nodes that carry `label` and whose `property` value is a
    /// non-empty string document are indexed. Non-string values are ignored, which
    /// matches the exact BM25 scan path.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] if the label index references a row
    /// without a resolvable node id or property row.
    pub fn build(graph: &SeleneGraph, label: DbString, property: DbString) -> GraphResult<Self> {
        let candidates = graph.node_candidates_with_label(&label)?;
        let validated = graph
            .validate_node_candidates(&candidates)
            .map_err(|error| GraphError::Inconsistent {
                reason: format!("fresh text-index candidates failed validation: {error}"),
            })?;
        if validated.is_empty() {
            return Ok(TextIndexBuilder::empty(label, property).finish());
        }
        let mut index = TextIndexBuilder::with_document_capacity(
            label.clone(),
            property.clone(),
            validated.len(),
        );

        for candidate in validated.as_slice() {
            let Some(text) = candidate.string_property(&property)? else {
                continue;
            };
            candidate.insert_into_text_index(&mut index, text.as_str());
        }
        Ok(index.finish())
    }

    /// Construct an empty postings index for `label.property`.
    #[must_use]
    pub fn empty(label: DbString, property: DbString) -> Self {
        Self {
            label,
            property,
            rows: RoaringBitmap::new(),
            document_lengths: FxHashMap::default(),
            document_terms: FxHashMap::default(),
            postings: FxHashMap::default(),
            total_document_len: 0,
            posting_count: 0,
        }
    }

    /// Return the indexed node label.
    #[must_use]
    pub const fn label(&self) -> &DbString {
        &self.label
    }

    /// Return the indexed node property.
    #[must_use]
    pub const fn property(&self) -> &DbString {
        &self.property
    }

    /// Return the indexed row bitmap.
    #[must_use]
    pub const fn rows(&self) -> &RoaringBitmap {
        &self.rows
    }

    /// Return the number of indexed string documents.
    #[must_use]
    pub fn document_count(&self) -> usize {
        self.document_lengths.len()
    }

    /// Return the number of distinct indexed terms.
    #[must_use]
    pub fn term_count(&self) -> usize {
        self.postings.len()
    }

    /// Return the total number of term-document postings.
    #[must_use]
    pub const fn posting_count(&self) -> usize {
        self.posting_count
    }

    /// Return aggregate index counters.
    #[must_use]
    pub fn stats(&self) -> TextIndexStats {
        TextIndexStats {
            indexed_rows: self.rows.len(),
            documents: self.document_count(),
            distinct_terms: self.term_count(),
            postings: self.posting_count,
            total_document_len: self.total_document_len,
        }
    }

    /// Return an estimated memory usage snapshot for this index.
    #[must_use]
    pub fn memory_usage(&self) -> TextIndexMemoryUsage {
        let row_bitmap_bytes = roaring_heap_bytes(&self.rows);
        let row_bitmap_serialized_bytes = self.rows.serialized_size();
        let document_length_bytes = self
            .document_lengths
            .capacity()
            .saturating_mul(size_of::<(NodeId, u32)>());
        let mut document_term_bytes = self
            .document_terms
            .capacity()
            .saturating_mul(size_of::<(NodeId, Arc<[TextTerm]>)>());
        for terms in self.document_terms.values() {
            document_term_bytes = document_term_bytes
                .saturating_add(terms.len().saturating_mul(size_of::<TextTerm>()));
        }
        let mut posting_bytes = 0usize;
        let mut term_bytes = 0usize;
        for (term, postings) in &self.postings {
            term_bytes = term_bytes.saturating_add(term.len());
            posting_bytes = posting_bytes
                .saturating_add(postings.capacity().saturating_mul(size_of::<TextPosting>()));
        }
        let terms_table_bytes = self
            .postings
            .capacity()
            .saturating_mul(size_of::<(TextTerm, Arc<Vec<TextPosting>>)>());
        let estimated_index_bytes = size_of::<Self>()
            .saturating_add(row_bitmap_bytes)
            .saturating_add(document_length_bytes)
            .saturating_add(document_term_bytes)
            .saturating_add(terms_table_bytes)
            .saturating_add(term_bytes)
            .saturating_add(posting_bytes);
        TextIndexMemoryUsage {
            indexed_rows: self.rows.len(),
            documents: self.document_count(),
            distinct_terms: self.term_count(),
            postings: self.posting_count,
            row_bitmap_bytes,
            row_bitmap_serialized_bytes,
            document_length_bytes,
            document_term_bytes,
            terms_table_bytes,
            term_bytes,
            posting_bytes,
            estimated_index_bytes,
        }
    }

    /// Rank indexed documents for `query` using BM25.
    #[must_use]
    pub fn search(&self, query: &str, k: usize) -> Vec<TextSearchHit> {
        self.search_checked(query, k, CancellationChecker::disabled())
            .expect("disabled text-index checker cannot fail")
    }

    /// Rank indexed documents for `query` with cooperative cancellation checks.
    ///
    /// The query path visits only postings for query terms, not every indexed
    /// document. Scores and tie ordering match the exact scan oracle.
    ///
    /// # Errors
    ///
    /// Returns [`TextSearchError::Cancelled`], [`TextSearchError::Timeout`], or
    /// [`TextSearchError::NodeScanBudgetExceeded`] when the supplied checker
    /// trips while collecting postings or scoring candidate documents.
    pub fn search_checked(
        &self,
        query: &str,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<TextSearchHit>, TextSearchError> {
        checker.check()?;
        if k == 0 || self.document_lengths.is_empty() {
            return Ok(Vec::new());
        }
        let query_terms = unique_query_terms(query);
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }

        let mut document_frequencies = QueryDocumentFrequencies::with_capacity(query_terms.len());
        let mut postings_by_term = QueryPostings::with_capacity(query_terms.len());
        let mut candidate_capacity = 0usize;
        for term in &query_terms {
            match self.postings.get(term.as_str()) {
                Some(postings) => {
                    candidate_capacity = candidate_capacity.saturating_add(postings.len());
                    document_frequencies.push(u32::try_from(postings.len()).unwrap_or(u32::MAX));
                    postings_by_term.push(Some(postings.as_slice()));
                }
                None => {
                    document_frequencies.push(0);
                    postings_by_term.push(None);
                }
            }
        }
        let candidate_capacity = candidate_capacity.min(self.document_lengths.len());
        if candidate_capacity == 0 {
            return Ok(Vec::new());
        }

        let mut candidates: FxHashMap<NodeId, DocumentStats> = FxHashMap::default();
        candidates.reserve(candidate_capacity);
        let mut postings_since_check = 0usize;

        for (term_index, postings) in postings_by_term.into_iter().enumerate() {
            let Some(postings) = postings else {
                continue;
            };
            for posting in postings {
                postings_since_check += 1;
                if postings_since_check >= crate::text_search::TEXT_SEARCH_CANCEL_STRIDE {
                    checker.check()?;
                    postings_since_check = 0;
                }
                let len = *self
                    .document_lengths
                    .get(&posting.node_id)
                    .expect("posting node must have document length");
                let doc = candidates.entry(posting.node_id).or_insert_with(|| {
                    DocumentStats::zero(posting.node_id, len, query_terms.len())
                });
                doc.term_counts[term_index] = posting.term_count;
            }
        }

        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let corpus_len = self.document_lengths.len() as f64;
        let average_document_len = self.total_document_len as f64 / corpus_len;
        let mut top_k = TextTopK::new(k);
        let mut docs_since_check = 0usize;
        for doc in candidates.into_values() {
            docs_since_check += 1;
            if docs_since_check >= crate::text_search::TEXT_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(docs_since_check)?;
                docs_since_check = 0;
            }
            let score = bm25_score(
                &doc,
                &document_frequencies,
                corpus_len,
                average_document_len,
            );
            if score > 0.0 {
                top_k.push(doc.node_id, score);
            }
        }
        if docs_since_check > 0 {
            checker.note_nodes_scanned(docs_since_check)?;
        }
        Ok(top_k.into_hits())
    }

    pub(crate) fn insert_document(&mut self, row: u32, node_id: NodeId, text: &str) {
        let (counts, len) = count_document_terms(text, |token| {
            intern_existing_posting_term(&self.postings, token)
        });
        if len == 0 {
            self.remove_document(row, node_id);
            return;
        }
        if self.document_lengths.contains_key(&node_id) {
            self.replace_counted_document(row, node_id, counts, len);
        } else {
            self.insert_counted_document(row, node_id, counts, len);
        }
    }

    fn insert_counted_document(
        &mut self,
        row: u32,
        node_id: NodeId,
        counts: DocumentTermCounts,
        len: u32,
    ) {
        self.rows.insert(row);
        self.document_lengths.insert(node_id, len);
        self.total_document_len = self.total_document_len.saturating_add(u64::from(len));
        let mut terms = Vec::with_capacity(counts.len());
        counts.for_each(|term, term_count| {
            let postings = self
                .postings
                .entry(Arc::clone(&term))
                .or_insert_with(|| Arc::new(Vec::new()));
            let postings = Arc::make_mut(postings);
            match postings.binary_search_by_key(&node_id, |posting| posting.node_id) {
                Ok(index) => {
                    postings[index].term_count = term_count;
                }
                Err(index) => {
                    postings.insert(
                        index,
                        TextPosting {
                            node_id,
                            term_count,
                        },
                    );
                    self.posting_count = self.posting_count.saturating_add(1);
                }
            }
            terms.push(term);
        });
        self.document_terms.insert(node_id, Arc::from(terms));
    }

    fn replace_counted_document(
        &mut self,
        row: u32,
        node_id: NodeId,
        counts: DocumentTermCounts,
        len: u32,
    ) {
        let Some(old_terms) = self.document_terms.remove(&node_id) else {
            self.remove_document(row, node_id);
            self.insert_counted_document(row, node_id, counts, len);
            return;
        };

        self.rows.insert(row);
        let old_len = self.document_lengths.insert(node_id, len).unwrap_or(0);
        self.total_document_len = self
            .total_document_len
            .saturating_sub(u64::from(old_len))
            .saturating_add(u64::from(len));

        let mut terms = Vec::with_capacity(counts.len());
        counts.for_each(|term, term_count| {
            upsert_posting(
                &mut self.postings,
                &mut self.posting_count,
                &term,
                node_id,
                term_count,
            );
            terms.push(term);
        });
        for old_term in old_terms.iter() {
            if terms.iter().any(|term| term.as_ref() == old_term.as_ref()) {
                continue;
            }
            remove_posting(
                &mut self.postings,
                &mut self.posting_count,
                old_term,
                node_id,
            );
        }
        self.document_terms.insert(node_id, Arc::from(terms));
    }

    pub(crate) fn remove_document(&mut self, row: u32, node_id: NodeId) {
        self.rows.remove(row);
        let Some(length) = self.document_lengths.remove(&node_id) else {
            return;
        };
        self.total_document_len = self.total_document_len.saturating_sub(u64::from(length));
        let Some(terms) = self.document_terms.remove(&node_id) else {
            return;
        };
        for term in terms.iter() {
            remove_posting(&mut self.postings, &mut self.posting_count, term, node_id);
        }
    }

    pub(crate) fn rows_eq(&self, reference: &Self) -> bool {
        self.rows == reference.rows
            && self.document_lengths == reference.document_lengths
            && self.total_document_len == reference.total_document_len
            && self.posting_count == reference.posting_count
            && self.postings == reference.postings
    }
}

/// Aggregate counters for a [`TextIndex`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextIndexStats {
    /// Number of indexed graph rows.
    pub indexed_rows: u64,
    /// Number of string documents with at least one token.
    pub documents: usize,
    /// Number of distinct terms in the vocabulary.
    pub distinct_terms: usize,
    /// Number of term-document postings.
    pub postings: usize,
    /// Sum of token counts across indexed documents.
    pub total_document_len: u64,
}

/// Estimated memory usage for one [`TextIndex`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextIndexMemoryUsage {
    /// Number of indexed graph rows.
    pub indexed_rows: u64,
    /// Number of string documents with at least one token.
    pub documents: usize,
    /// Number of distinct terms in the vocabulary.
    pub distinct_terms: usize,
    /// Number of term-document postings.
    pub postings: usize,
    /// Estimated heap bytes used by the row bitmap containers.
    pub row_bitmap_bytes: usize,
    /// Serialized byte size of the row bitmap.
    pub row_bitmap_serialized_bytes: usize,
    /// Estimated bytes for the document-length map.
    pub document_length_bytes: usize,
    /// Estimated bytes for per-document term lists used by commit maintenance.
    pub document_term_bytes: usize,
    /// Estimated bytes for the postings hash table.
    pub terms_table_bytes: usize,
    /// Estimated bytes for term string buffers.
    pub term_bytes: usize,
    /// Estimated bytes for posting vectors.
    pub posting_bytes: usize,
    /// Estimated bytes reachable from the index object.
    pub estimated_index_bytes: usize,
}

impl SeleneGraph {
    /// Build a reusable BM25 postings index for `label.property`.
    ///
    /// The returned index is tied to this graph snapshot. Mutations committed
    /// after the snapshot is read require rebuilding or durable registration in a
    /// later maintained-index layer.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] if graph label/property columns are
    /// internally inconsistent while the snapshot is scanned.
    pub fn build_text_index(
        &self,
        label: &DbString,
        property: &DbString,
    ) -> GraphResult<TextIndex> {
        TextIndex::build(self, label.clone(), property.clone())
    }

    /// Rank string-valued node properties through a transient postings index.
    ///
    /// This is primarily useful for tests and benchmark comparisons. Repeated
    /// production queries should build a [`TextIndex`] once and call
    /// [`TextIndex::search`] directly.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] if index construction observes corrupt
    /// graph columns.
    pub fn indexed_text_search_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        query: &str,
        k: usize,
    ) -> GraphResult<Vec<TextSearchHit>> {
        Ok(self.build_text_index(label, property)?.search(query, k))
    }
}

impl SharedGraph {
    /// Build a reusable BM25 postings index from the current shared snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] if index construction observes corrupt
    /// graph columns.
    pub fn build_text_index(
        &self,
        label: &DbString,
        property: &DbString,
    ) -> GraphResult<TextIndex> {
        self.read().build_text_index(label, property)
    }

    /// Rank string-valued node properties through a transient postings index.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Inconsistent`] if index construction observes corrupt
    /// graph columns.
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TextPosting {
    node_id: NodeId,
    term_count: u32,
}

enum DocumentTermCounts {
    Inline(InlineDocumentTermCounts),
    Map(FxHashMap<TextTerm, u32>),
}

impl DocumentTermCounts {
    fn new() -> Self {
        Self::Inline(InlineDocumentTermCounts::new())
    }

    fn increment(&mut self, term: TextTerm) {
        match self {
            Self::Inline(counts) => {
                if let Some((_, count)) = counts
                    .iter_mut()
                    .find(|(existing, _)| existing.as_ref() == term.as_ref())
                {
                    *count = count.saturating_add(1);
                    return;
                }
                if counts.len() < INLINE_DOCUMENT_TERM_COUNT_CAPACITY {
                    counts.push((term, 1));
                    return;
                }

                let mut map =
                    FxHashMap::with_capacity_and_hasher(counts.len() + 1, Default::default());
                for (term, count) in counts.drain(..) {
                    map.insert(term, count);
                }
                map.insert(term, 1);
                *self = Self::Map(map);
            }
            Self::Map(counts) => {
                let count = counts.entry(term).or_insert(0);
                *count = count.saturating_add(1);
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Inline(counts) => counts.len(),
            Self::Map(counts) => counts.len(),
        }
    }

    fn for_each(self, mut visit: impl FnMut(TextTerm, u32)) {
        match self {
            Self::Inline(counts) => {
                for (term, count) in counts {
                    visit(term, count);
                }
            }
            Self::Map(counts) => {
                for (term, count) in counts {
                    visit(term, count);
                }
            }
        }
    }
}

fn count_document_terms(
    text: &str,
    mut intern: impl FnMut(&str) -> TextTerm,
) -> (DocumentTermCounts, u32) {
    let mut counts = DocumentTermCounts::new();
    let mut len = 0_u32;
    for token in tokenize_borrowed(text) {
        len = len.saturating_add(1);
        counts.increment(intern(token.as_ref()));
    }
    (counts, len)
}

fn intern_existing_posting_term(
    postings: &FxHashMap<TextTerm, Arc<Vec<TextPosting>>>,
    token: &str,
) -> TextTerm {
    postings
        .get_key_value(token)
        .map(|(term, _)| Arc::clone(term))
        .unwrap_or_else(|| Arc::from(token))
}

fn roaring_heap_bytes(rows: &RoaringBitmap) -> usize {
    let statistics = rows.statistics();
    usize::try_from(
        statistics
            .n_bytes_array_containers
            .saturating_add(statistics.n_bytes_run_containers)
            .saturating_add(statistics.n_bytes_bitset_containers),
    )
    .unwrap_or(usize::MAX)
}

#[cfg(test)]
#[path = "text_index/tests.rs"]
mod tests;
