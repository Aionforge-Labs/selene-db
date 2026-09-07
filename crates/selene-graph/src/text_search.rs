//! Exact BM25 full-text search over graph node properties.
//!
//! This module is the full-text correctness oracle: it scans the current graph
//! snapshot, tokenizes string properties, computes BM25 document statistics for
//! the requested `(label, property)` surface, and returns a deterministic
//! top-`k` ranking. Future maintained or postings-backed text indexes should
//! use this path as their ordering and recall reference.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap};
use std::time::Duration;

use selene_core::{CancellationCause, CancellationChecker, DbString, NodeId};
use smallvec::SmallVec;

use crate::error::{GraphError, GraphResult};
use crate::graph::SeleneGraph;
use crate::parallel_scan::{should_parallelize_scan, try_reduce_chunks};
use crate::shared::SharedGraph;
use crate::validated_candidates::ValidatedCandidateNode;
use crate::{CandidateSet, Node};

pub(crate) const TEXT_SEARCH_CANCEL_STRIDE: usize = 1024;
#[cfg(not(test))]
const TEXT_SEARCH_PARALLEL_CHUNK_ROWS: usize = 2048;
#[cfg(test)]
const TEXT_SEARCH_PARALLEL_CHUNK_ROWS: usize = 4;

#[cfg(not(test))]
const TEXT_SEARCH_PARALLEL_MIN_ROWS: u64 = 16_384;
#[cfg(test)]
const TEXT_SEARCH_PARALLEL_MIN_ROWS: u64 = 8;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

type TermCounts = SmallVec<[u32; 4]>;

/// One BM25-ranked node hit.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSearchHit {
    /// Matched node id.
    pub node_id: NodeId,
    /// Higher-is-better BM25 score.
    pub score: f64,
}

/// Error returned by checked text-search APIs.
#[derive(Debug, thiserror::Error)]
pub enum TextSearchError {
    /// Graph storage or consistency failure.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// Caller requested cooperative cancellation.
    #[error("text search cancelled")]
    Cancelled,
    /// Statement deadline elapsed.
    #[error("text search timed out after {elapsed:?}")]
    Timeout {
        /// Wall-clock duration since the deadline elapsed.
        elapsed: Duration,
    },
    /// Deterministic node-scan budget was exceeded.
    #[error("text search node scan budget exceeded ({scanned} > {limit})")]
    NodeScanBudgetExceeded {
        /// Maximum allowed scanned nodes.
        limit: usize,
        /// Observed scanned nodes after the batch that crossed the limit.
        scanned: usize,
    },
}

impl TextSearchError {
    fn into_graph_error(self) -> GraphError {
        match self {
            Self::Graph(error) => error,
            Self::Cancelled | Self::Timeout { .. } | Self::NodeScanBudgetExceeded { .. } => {
                GraphError::Inconsistent {
                    reason: format!("disabled text-search checker returned {self}"),
                }
            }
        }
    }
}

impl From<CancellationCause> for TextSearchError {
    fn from(cause: CancellationCause) -> Self {
        match cause {
            CancellationCause::Cancelled => Self::Cancelled,
            CancellationCause::Timeout { elapsed } => Self::Timeout { elapsed },
            CancellationCause::NodeScanBudgetExceeded { limit, scanned } => {
                Self::NodeScanBudgetExceeded { limit, scanned }
            }
        }
    }
}

impl SeleneGraph {
    /// Exhaustively rank string-valued node properties using BM25.
    ///
    /// This is the full-text correctness oracle and small-corpus path. It scans
    /// the row bitmap for `label`, skips nodes where `property` is absent or not
    /// a string, tokenizes documents with the built-in Unicode-aware tokenizer,
    /// and ranks matches with Okapi BM25 (`k1 = 1.2`, `b = 0.75`). Query tokens
    /// are deduplicated so repeated query terms do not overweight a document.
    pub fn exact_text_search_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        query: &str,
        k: usize,
    ) -> GraphResult<Vec<TextSearchHit>> {
        self.exact_text_search_nodes_checked(
            label,
            property,
            query,
            k,
            CancellationChecker::disabled(),
        )
        .map_err(TextSearchError::into_graph_error)
    }

    /// Exhaustively rank string-valued node properties with cancellation checks.
    pub fn exact_text_search_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        query: &str,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<TextSearchHit>, TextSearchError> {
        self.exact_text_search_nodes_filtered_checked(label, property, query, k, None, checker)
    }

    /// Exhaustively rank text documents while admitting only `candidates`.
    pub fn exact_text_search_nodes_in_candidates_checked(
        &self,
        label: &DbString,
        property: &DbString,
        query: &str,
        k: usize,
        candidates: &CandidateSet<Node>,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<TextSearchHit>, TextSearchError> {
        self.exact_text_search_nodes_filtered_checked(
            label,
            property,
            query,
            k,
            Some(candidates),
            checker,
        )
    }

    pub(crate) fn exact_text_search_nodes_filtered_checked(
        &self,
        label: &DbString,
        property: &DbString,
        query: &str,
        k: usize,
        allowed: Option<&CandidateSet<Node>>,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<TextSearchHit>, TextSearchError> {
        checker.check()?;
        if k == 0 {
            return Ok(Vec::new());
        }
        let query_terms = unique_query_terms(query);
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(allowed) = allowed {
            self.validate_node_candidates(allowed)
                .map_err(|error| GraphError::Inconsistent {
                    reason: format!("allowed text-search candidates failed validation: {error}"),
                })?;
        }
        let candidates = self.node_candidates_with_label(label)?;
        let validated = self
            .validate_node_candidates(&candidates)
            .map_err(|error| GraphError::Inconsistent {
                reason: format!("fresh text-search candidates failed validation: {error}"),
            })?;
        if validated.is_empty() {
            return Ok(Vec::new());
        }

        let scan = TextScan::new(property, &query_terms, allowed);
        let chunk = if should_parallelize_text_scan(validated.len(), k) {
            exact_text_scan_parallel(scan, validated.as_slice(), checker)?
        } else {
            exact_text_scan_serial(scan, validated.as_slice(), checker)?
        };
        Ok(rank_text_docs(chunk, k))
    }
}

impl SharedGraph {
    /// Exhaustively rank string-valued node properties in the current snapshot.
    pub fn exact_text_search_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        query: &str,
        k: usize,
    ) -> GraphResult<Vec<TextSearchHit>> {
        self.read()
            .exact_text_search_nodes(label, property, query, k)
    }

    /// Exhaustively rank string-valued node properties with cancellation checks.
    pub fn exact_text_search_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        query: &str,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<TextSearchHit>, TextSearchError> {
        self.read()
            .exact_text_search_nodes_checked(label, property, query, k, checker)
    }
}

#[derive(Clone, Copy)]
struct TextScan<'a> {
    property: &'a DbString,
    query_terms: &'a [String],
    allowed: Option<&'a CandidateSet<Node>>,
}

impl<'a> TextScan<'a> {
    fn new(
        property: &'a DbString,
        query_terms: &'a [String],
        allowed: Option<&'a CandidateSet<Node>>,
    ) -> Self {
        Self {
            property,
            query_terms,
            allowed,
        }
    }

    fn document_for_candidate(
        self,
        candidate: ValidatedCandidateNode<'_>,
    ) -> Result<Option<DocumentStats>, TextSearchError> {
        let Some(text) = candidate.string_property(self.property)? else {
            return Ok(None);
        };
        Ok(document_stats(
            candidate.node_id(),
            text.as_str(),
            self.query_terms,
            self.allowed
                .is_none_or(|allowed| allowed.contains(candidate.node_id())),
        ))
    }
}

#[derive(Debug)]
struct TextScanChunk {
    docs: Vec<DocumentStats>,
    document_frequencies: Vec<u32>,
    total_document_len: u64,
}

impl TextScanChunk {
    fn empty(query_term_count: usize) -> Self {
        Self {
            docs: Vec::new(),
            document_frequencies: vec![0; query_term_count],
            total_document_len: 0,
        }
    }

    fn push(&mut self, doc: DocumentStats) {
        for (frequency, count) in self.document_frequencies.iter_mut().zip(&doc.term_counts) {
            if *count > 0 {
                *frequency = frequency.saturating_add(1);
            }
        }
        self.total_document_len = self.total_document_len.saturating_add(u64::from(doc.len));
        self.docs.push(doc);
    }
}

fn should_parallelize_text_scan(row_count: usize, k: usize) -> bool {
    should_parallelize_scan(row_count as u64, k, TEXT_SEARCH_PARALLEL_MIN_ROWS)
}

fn exact_text_scan_parallel(
    scan: TextScan<'_>,
    candidates: &[ValidatedCandidateNode<'_>],
    checker: CancellationChecker<'_>,
) -> Result<TextScanChunk, TextSearchError> {
    try_reduce_chunks(
        candidates,
        TEXT_SEARCH_PARALLEL_CHUNK_ROWS,
        checker,
        || TextScanChunk::empty(scan.query_terms.len()),
        |chunk| exact_text_scan_chunk(scan, chunk),
        merge_text_scan_chunks,
    )
}

fn exact_text_scan_serial(
    scan: TextScan<'_>,
    candidates: &[ValidatedCandidateNode<'_>],
    checker: CancellationChecker<'_>,
) -> Result<TextScanChunk, TextSearchError> {
    let mut chunk = TextScanChunk::empty(scan.query_terms.len());
    let mut rows_since_check = 0usize;
    for &candidate in candidates {
        rows_since_check += 1;
        if rows_since_check >= TEXT_SEARCH_CANCEL_STRIDE {
            checker.note_nodes_scanned(rows_since_check)?;
            rows_since_check = 0;
        }
        if let Some(doc) = scan.document_for_candidate(candidate)? {
            chunk.push(doc);
        }
    }
    if rows_since_check > 0 {
        checker.note_nodes_scanned(rows_since_check)?;
    }
    Ok(chunk)
}

fn exact_text_scan_chunk(
    scan: TextScan<'_>,
    candidates: &[ValidatedCandidateNode<'_>],
) -> Result<TextScanChunk, TextSearchError> {
    let mut chunk = TextScanChunk::empty(scan.query_terms.len());
    for &candidate in candidates {
        if let Some(doc) = scan.document_for_candidate(candidate)? {
            chunk.push(doc);
        }
    }
    Ok(chunk)
}

fn merge_text_scan_chunks(
    mut lhs: TextScanChunk,
    mut rhs: TextScanChunk,
) -> Result<TextScanChunk, TextSearchError> {
    for (lhs_frequency, rhs_frequency) in lhs
        .document_frequencies
        .iter_mut()
        .zip(&rhs.document_frequencies)
    {
        *lhs_frequency = lhs_frequency.saturating_add(*rhs_frequency);
    }
    lhs.total_document_len = lhs
        .total_document_len
        .saturating_add(rhs.total_document_len);
    lhs.docs.append(&mut rhs.docs);
    Ok(lhs)
}

fn rank_text_docs(chunk: TextScanChunk, k: usize) -> Vec<TextSearchHit> {
    if chunk.docs.is_empty() {
        return Vec::new();
    }
    let corpus_len = chunk.docs.len() as f64;
    let average_document_len = chunk.total_document_len as f64 / corpus_len;
    let mut top_k = TextTopK::new(k);
    for doc in chunk.docs {
        if !doc.admitted {
            continue;
        }
        let score = bm25_score(
            &doc,
            &chunk.document_frequencies,
            corpus_len,
            average_document_len,
        );
        if score > 0.0 {
            top_k.push(doc.node_id, score);
        }
    }
    top_k.into_hits()
}

#[derive(Debug)]
pub(crate) struct DocumentStats {
    pub(crate) node_id: NodeId,
    len: u32,
    pub(crate) term_counts: TermCounts,
    admitted: bool,
}

impl DocumentStats {
    pub(crate) fn zero(node_id: NodeId, len: u32, query_term_count: usize) -> Self {
        Self {
            node_id,
            len,
            term_counts: TermCounts::from_elem(0, query_term_count),
            admitted: true,
        }
    }
}

pub(crate) fn unique_query_terms(query: &str) -> Vec<String> {
    let terms: BTreeSet<_> = tokenize_borrowed(query).map(Cow::into_owned).collect();
    terms.into_iter().collect()
}

fn document_stats(
    node_id: NodeId,
    text: &str,
    query_terms: &[String],
    admitted: bool,
) -> Option<DocumentStats> {
    let mut term_counts = TermCounts::from_elem(0, query_terms.len());
    let mut len = 0_u32;
    let short_query = query_terms.len() <= 4;
    for token in tokenize_borrowed(text) {
        len = len.saturating_add(1);
        if short_query {
            for (index, term) in query_terms.iter().enumerate() {
                if term.as_str() == token.as_ref() {
                    term_counts[index] = term_counts[index].saturating_add(1);
                    break;
                }
            }
        } else if let Ok(index) =
            query_terms.binary_search_by(|term| term.as_str().cmp(token.as_ref()))
        {
            term_counts[index] = term_counts[index].saturating_add(1);
        }
    }
    (len > 0).then_some(DocumentStats {
        node_id,
        len,
        term_counts,
        admitted,
    })
}

/// Iterate lowercase alphanumeric tokens, borrowing when lowercase is unchanged.
pub(crate) fn tokenize_borrowed(text: &str) -> Tokenizer<'_> {
    Tokenizer {
        text,
        offset: 0,
        ascii: text.is_ascii(),
    }
}

/// Borrowing tokenizer for BM25 query/document processing.
pub(crate) struct Tokenizer<'a> {
    text: &'a str,
    offset: usize,
    ascii: bool,
}

impl<'a> Tokenizer<'a> {
    fn next_ascii(&mut self) -> Option<Cow<'a, str>> {
        let mut start = None;
        let mut end = self.text.len();
        let mut owned = None::<String>;

        let bytes = self.text.as_bytes();
        let mut index = self.offset;
        while index < bytes.len() {
            let byte = bytes[index];
            if !byte.is_ascii_alphanumeric() {
                if start.is_some() {
                    end = index;
                    self.offset = index + 1;
                    break;
                }
                self.offset = index + 1;
                index += 1;
                continue;
            }

            let start_index = *start.get_or_insert(index);
            let lowered = byte.to_ascii_lowercase();
            if let Some(buffer) = owned.as_mut() {
                buffer.push(char::from(lowered));
            } else if lowered != byte {
                let mut buffer = self.text[start_index..index].to_owned();
                buffer.push(char::from(lowered));
                owned = Some(buffer);
            }
            index += 1;
        }

        let start = start?;
        if self.offset <= start {
            self.offset = self.text.len();
        }

        Some(match owned {
            Some(token) => Cow::Owned(token),
            None => Cow::Borrowed(&self.text[start..end]),
        })
    }

    fn next_unicode(&mut self) -> Option<Cow<'a, str>> {
        let mut start = None;
        let mut end = self.text.len();
        let mut owned = None::<String>;

        let base = self.offset;
        for (relative_index, ch) in self.text[base..].char_indices() {
            let index = base + relative_index;
            if !ch.is_alphanumeric() {
                if start.is_some() {
                    end = index;
                    self.offset = index + ch.len_utf8();
                    break;
                }
                self.offset = index + ch.len_utf8();
                continue;
            }

            let start_index = *start.get_or_insert(index);
            let mut lowercase = ch.to_lowercase();
            let first = lowercase
                .next()
                .expect("char lowercase mapping yields at least one char");
            let second = lowercase.next();
            let unchanged = first == ch && second.is_none();
            if let Some(buffer) = owned.as_mut() {
                if unchanged {
                    buffer.push(ch);
                } else {
                    buffer.push(first);
                    if let Some(second) = second {
                        buffer.push(second);
                    }
                    buffer.extend(lowercase);
                }
            } else if !unchanged {
                let mut buffer = self.text[start_index..index].to_owned();
                buffer.push(first);
                if let Some(second) = second {
                    buffer.push(second);
                }
                buffer.extend(lowercase);
                owned = Some(buffer);
            }
        }

        let start = start?;
        if self.offset <= start {
            self.offset = self.text.len();
        }

        Some(match owned {
            Some(token) => Cow::Owned(token),
            None => Cow::Borrowed(&self.text[start..end]),
        })
    }
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = Cow<'a, str>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ascii {
            self.next_ascii()
        } else {
            self.next_unicode()
        }
    }
}

pub(crate) fn bm25_score(
    doc: &DocumentStats,
    document_frequencies: &[u32],
    corpus_len: f64,
    average_document_len: f64,
) -> f64 {
    let document_len = f64::from(doc.len);
    doc.term_counts
        .iter()
        .zip(document_frequencies)
        .filter(|(term_count, _)| **term_count > 0)
        .map(|(term_count, document_frequency)| {
            let term_count = f64::from(*term_count);
            let document_frequency = f64::from(*document_frequency);
            let idf =
                (1.0 + (corpus_len - document_frequency + 0.5) / (document_frequency + 0.5)).ln();
            let normalization = term_count
                + BM25_K1 * (1.0 - BM25_B + BM25_B * document_len / average_document_len);
            idf * (term_count * (BM25_K1 + 1.0)) / normalization
        })
        .sum()
}

#[derive(Debug)]
pub(crate) struct TextTopK {
    k: usize,
    heap: BinaryHeap<TextHeapEntry>,
}

impl TextTopK {
    pub(crate) fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::with_capacity(k),
        }
    }

    pub(crate) fn push(&mut self, node_id: NodeId, score: f64) {
        debug_assert!(score.is_finite(), "BM25 scores must be finite");
        if self.k == 0 {
            return;
        }
        let entry = TextHeapEntry { score, node_id };
        if self.heap.len() < self.k {
            self.heap.push(entry);
            return;
        }
        let Some(mut worst) = self.heap.peek_mut() else {
            return;
        };
        if entry.cmp(&*worst).is_lt() {
            *worst = entry;
        }
    }

    pub(crate) fn into_hits(self) -> Vec<TextSearchHit> {
        let mut hits: Vec<_> = self
            .heap
            .into_iter()
            .map(|entry| TextSearchHit {
                node_id: entry.node_id,
                score: entry.score,
            })
            .collect();
        hits.sort_by(compare_hit);
        hits
    }
}

#[derive(Debug)]
struct TextHeapEntry {
    score: f64,
    node_id: NodeId,
}

impl Eq for TextHeapEntry {}

impl PartialEq for TextHeapEntry {
    fn eq(&self, rhs: &Self) -> bool {
        self.score.to_bits() == rhs.score.to_bits() && self.node_id == rhs.node_id
    }
}

impl Ord for TextHeapEntry {
    fn cmp(&self, rhs: &Self) -> Ordering {
        rhs.score
            .total_cmp(&self.score)
            .then_with(|| self.node_id.cmp(&rhs.node_id))
    }
}

impl PartialOrd for TextHeapEntry {
    fn partial_cmp(&self, rhs: &Self) -> Option<Ordering> {
        Some(self.cmp(rhs))
    }
}

fn compare_hit(lhs: &TextSearchHit, rhs: &TextSearchHit) -> Ordering {
    rhs.score
        .total_cmp(&lhs.score)
        .then_with(|| lhs.node_id.cmp(&rhs.node_id))
}

#[cfg(test)]
#[path = "text_search/tests.rs"]
mod tests;
