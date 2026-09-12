//! Derived-index compatibility, not a durable accelerator format.

use super::TextIndex;
use crate::{GraphError, GraphResult};

// Bump together with changes to tokenization, document eligibility or BM25
// statistics/scoring. Registrations persist no postings; reopen always rebuilds.
pub(super) const VERSION: u32 = 1;

impl TextIndex {
    /// Whether this derived index uses the installed tokenizer/BM25 contract.
    /// Incompatible postings are never mixed with current query statistics.
    #[must_use]
    pub fn has_current_contract(&self) -> bool {
        self.contract_version == VERSION
    }

    pub(super) fn validate_contract(&self) -> GraphResult<()> {
        if self.has_current_contract() {
            return Ok(());
        }
        Err(GraphError::Inconsistent {
            reason: "text tokenizer/BM25 contract changed; rebuild the derived index".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SharedGraph;
    use selene_core::{CancellationChecker, GraphId, db_string};

    #[test]
    fn stale_analyzer_is_declined_and_rebuild_replaces_it() {
        let graph = SharedGraph::new(GraphId::new(408));
        let label = db_string("Doc").unwrap();
        let property = db_string("body").unwrap();
        graph
            .create_text_index(label.clone(), property.clone())
            .unwrap();
        let mut snapshot = graph.read().as_ref().clone();
        let entry = snapshot
            .text_index
            .get_mut(&(label.clone(), property.clone()))
            .unwrap();
        let index = std::sync::Arc::make_mut(&mut entry.index);
        index.contract_version = VERSION + 1;
        assert!(
            index
                .search_checked("memory", 0, CancellationChecker::disabled())
                .is_err()
        );
        assert!(
            index
                .search_candidates_checked("memory", &[], 0, CancellationChecker::disabled())
                .is_err()
        );
        assert!(snapshot.text_index_for(&label, &property).is_none());
        super::super::rebuild_text_indexes(&mut snapshot).unwrap();
        assert!(
            snapshot
                .text_index_for(&label, &property)
                .unwrap()
                .has_current_contract()
        );
    }
}
