use std::sync::Arc;

use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;

use selene_core::{DbString, NodeId};

use super::{TextIndex, TextPosting, TextTerm, count_document_terms};

pub(crate) struct TextIndexBuilder {
    label: DbString,
    property: DbString,
    rows: RoaringBitmap,
    document_lengths: FxHashMap<NodeId, u32>,
    document_terms: FxHashMap<NodeId, Arc<[TextTerm]>>,
    postings: FxHashMap<TextTerm, Vec<TextPosting>>,
    total_document_len: u64,
    posting_count: usize,
}

impl TextIndexBuilder {
    pub(super) fn empty(label: DbString, property: DbString) -> Self {
        Self::with_document_capacity(label, property, 0)
    }

    pub(super) fn with_document_capacity(
        label: DbString,
        property: DbString,
        document_capacity: usize,
    ) -> Self {
        Self {
            label,
            property,
            rows: RoaringBitmap::new(),
            document_lengths: FxHashMap::with_capacity_and_hasher(
                document_capacity,
                Default::default(),
            ),
            document_terms: FxHashMap::with_capacity_and_hasher(
                document_capacity,
                Default::default(),
            ),
            postings: FxHashMap::default(),
            total_document_len: 0,
            posting_count: 0,
        }
    }

    pub(crate) fn insert_document(&mut self, row: u32, node_id: NodeId, text: &str) {
        let (counts, len) = count_document_terms(text, |token| {
            intern_existing_builder_term(&self.postings, token)
        });
        if len == 0 {
            return;
        }

        self.rows.insert(row);
        self.document_lengths.insert(node_id, len);
        self.total_document_len = self.total_document_len.saturating_add(u64::from(len));
        let mut terms = Vec::with_capacity(counts.len());
        counts.for_each(|term, term_count| {
            let postings = self.postings.entry(Arc::clone(&term)).or_default();
            postings.push(TextPosting {
                node_id,
                term_count,
            });
            self.posting_count = self.posting_count.saturating_add(1);
            terms.push(term);
        });
        self.document_terms.insert(node_id, Arc::from(terms));
    }

    pub(super) fn finish(mut self) -> TextIndex {
        for postings in self.postings.values_mut() {
            postings.sort_by_key(|posting| posting.node_id);
        }
        self.document_lengths.shrink_to_fit();
        self.document_terms.shrink_to_fit();
        TextIndex {
            label: self.label,
            property: self.property,
            rows: self.rows,
            document_lengths: self.document_lengths,
            document_terms: self.document_terms,
            postings: self
                .postings
                .into_iter()
                .map(|(term, postings)| (term, Arc::new(postings)))
                .collect(),
            total_document_len: self.total_document_len,
            posting_count: self.posting_count,
        }
    }
}

fn intern_existing_builder_term(
    postings: &FxHashMap<TextTerm, Vec<TextPosting>>,
    token: &str,
) -> TextTerm {
    postings
        .get_key_value(token)
        .map(|(term, _)| Arc::clone(term))
        .unwrap_or_else(|| TextTerm::from(token))
}
