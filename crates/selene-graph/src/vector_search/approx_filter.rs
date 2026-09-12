//! Approximate vector search constrained by a row allowlist.

use roaring::RoaringBitmap;
use selene_core::{CancellationChecker, DbString, VectorValue};

use crate::error::GraphError;
use crate::graph::SeleneGraph;
use crate::vector_index::HnswSearchScratch;

use super::{
    ApproximateVectorSearchOptions, VectorNodeSearchHit, VectorSearchError,
    ann_row_hits_to_node_hits, rerank_ann_row_candidates, turbo_quant_exact,
};

impl SeleneGraph {
    /// Approximately rank vector-valued node properties while admitting only
    /// candidates in `candidates`.
    ///
    /// HNSW filters its bounded global search beam before final top-k, not
    /// before traversal. IVF admits allowed rows from only the probed lists.
    /// This can return fewer than `k` hits even with more eligible live vectors.
    /// No exact refill is performed. Candidate identity is validated against
    /// this pinned graph even for an empty allowlist or `k == 0`.
    pub fn approximate_vector_search_nodes_in_candidates_checked(
        &self,
        label: &DbString,
        property: &DbString,
        query: &VectorValue,
        candidates: &crate::CandidateSet<crate::Node>,
        options: ApproximateVectorSearchOptions,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<VectorNodeSearchHit>, VectorSearchError> {
        checker.check()?;
        let validated = self.validate_node_candidates(candidates).map_err(|error| {
            VectorSearchError::Graph(GraphError::Inconsistent {
                reason: format!("allowed candidate set failed validation: {error}"),
            })
        })?;
        if options.k == 0 || candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut allowed_rows = RoaringBitmap::new();
        for node in validated.as_slice() {
            allowed_rows.insert(node.row().get());
        }
        self.approximate_vector_search_nodes_in_rows_checked(
            label,
            property,
            query,
            &allowed_rows,
            options,
            checker,
        )
    }

    /// Approximately rank vector-valued node properties while admitting only
    /// rows in `allowed_rows`.
    pub(crate) fn approximate_vector_search_nodes_in_rows_checked(
        &self,
        label: &DbString,
        property: &DbString,
        query: &VectorValue,
        allowed_rows: &RoaringBitmap,
        options: ApproximateVectorSearchOptions,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<VectorNodeSearchHit>, VectorSearchError> {
        checker.check()?;
        if options.k == 0 || allowed_rows.is_empty() {
            return Ok(Vec::new());
        }
        let query_dimension = u32::try_from(query.dimension())
            .map_err(|_| VectorSearchError::ApproximateIndexMissing)?;
        let Some(index) = self
            .vector_index_for(label, property)
            .filter(|index| index.dimension() == query_dimension)
        else {
            return Err(VectorSearchError::ApproximateIndexMissing);
        };
        let Some(indexed_metric) = index.ann_metric() else {
            return Err(VectorSearchError::ApproximateIndexMissing);
        };
        if indexed_metric != options.metric {
            return Err(VectorSearchError::ApproximateMetricMismatch {
                indexed: indexed_metric,
                requested: options.metric,
            });
        }
        if index.is_turbo_quant() {
            let mut allowed_index_rows = allowed_rows.clone();
            allowed_index_rows &= index.rows();
            if allowed_index_rows.is_empty() {
                return Ok(Vec::new());
            }
            if turbo_quant_exact::covers_rows(&allowed_index_rows, options) {
                return rerank_ann_row_candidates(
                    self,
                    property,
                    query,
                    options.metric,
                    options.k,
                    turbo_quant_exact::row_hits(&allowed_index_rows),
                    &checker,
                );
            }
            let row_hits = index
                .turbo_quant_candidates_in_rows(
                    query,
                    options.k,
                    options.ef_search,
                    &allowed_index_rows,
                )
                .ok_or(VectorSearchError::ApproximateIndexMissing)?
                .map_err(GraphError::from)?;
            return rerank_ann_row_candidates(
                self,
                property,
                query,
                options.metric,
                options.k,
                row_hits,
                &checker,
            );
        }

        let mut scratch = HnswSearchScratch::default();
        let row_hits = index
            .ann_search_in_rows_with_scratch(
                query,
                options.k,
                options.ef_search,
                allowed_rows,
                &mut scratch,
            )
            .ok_or(VectorSearchError::ApproximateIndexMissing)?
            .map_err(GraphError::from)?;

        ann_row_hits_to_node_hits(self, label, row_hits, &checker)
    }
}
