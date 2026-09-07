//! TurboQuant approximate search over explicit candidate sets.

use roaring::RoaringBitmap;
use selene_core::{CancellationChecker, DbString, VectorValue};

use crate::error::GraphError;
use crate::graph::SeleneGraph;

use super::{
    ApproximateVectorSearchOptions, VectorCandidateSet, VectorNodeSearchHit, VectorSearchError,
    approx_batch, rerank_ann_row_candidates, turbo_quant_exact,
};

impl SeleneGraph {
    /// Approximately rank a canonical node candidate set through a TurboQuant index.
    ///
    /// This is the approximate counterpart to
    /// [`Self::score_vector_candidate_set_checked`]: callers supply the
    /// candidate set explicitly, TurboQuant preselects up to `ef_search`
    /// candidates within that set, and the graph layer exact-reranks the
    /// returned rows against primary `VECTOR` values. Missing nodes, nodes
    /// outside the registered `(label, property)` vector index, and nodes
    /// without a vector value are skipped under the normal snapshot visibility
    /// rules. When the search width covers every surviving indexed candidate
    /// row, the compressed preselection pass is skipped and those rows are
    /// exact-scored directly.
    pub fn approximate_vector_search_candidate_set_checked(
        &self,
        label: &DbString,
        property: &DbString,
        query: &VectorValue,
        candidates: &VectorCandidateSet,
        options: ApproximateVectorSearchOptions,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<VectorNodeSearchHit>, VectorSearchError> {
        checker.check()?;
        if options.k == 0 || candidates.is_empty() {
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
        if !index.is_turbo_quant() {
            return Err(VectorSearchError::ApproximateIndexMissing);
        }

        let allowed_rows = self.vector_candidate_rows(candidates, index.rows(), &checker)?;
        if allowed_rows.is_empty() {
            return Ok(Vec::new());
        }
        if turbo_quant_exact::covers_rows(&allowed_rows, options) {
            return rerank_ann_row_candidates(
                self,
                property,
                query,
                options.metric,
                options.k,
                turbo_quant_exact::row_hits(&allowed_rows),
                &checker,
            );
        }
        let row_hits = index
            .turbo_quant_candidates_in_rows(query, options.k, options.ef_search, &allowed_rows)
            .ok_or(VectorSearchError::ApproximateIndexMissing)?
            .map_err(GraphError::from)?;
        rerank_ann_row_candidates(
            self,
            property,
            query,
            options.metric,
            options.k,
            row_hits,
            &checker,
        )
    }

    /// Approximately rank one canonical node candidate set per query.
    ///
    /// Each `queries[i]` is searched only within `candidate_sets[i]` through a
    /// matching TurboQuant index, then exact-reranked against primary vector
    /// values. Output positions correspond to input query positions.
    pub fn approximate_vector_search_candidate_sets_batch_checked(
        &self,
        label: &DbString,
        property: &DbString,
        queries: &[VectorValue],
        candidate_sets: &[VectorCandidateSet],
        options: ApproximateVectorSearchOptions,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<Vec<VectorNodeSearchHit>>, VectorSearchError> {
        checker.check()?;
        if queries.len() != candidate_sets.len() {
            return Err(VectorSearchError::BatchLengthMismatch {
                queries: queries.len(),
                candidate_sets: candidate_sets.len(),
            });
        }
        let Some(first_query) = queries.first() else {
            return Ok(Vec::new());
        };
        if options.k == 0 {
            return Ok(vec![Vec::new(); queries.len()]);
        }
        let query_dimension = u32::try_from(first_query.dimension())
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
        if !index.is_turbo_quant() {
            return Err(VectorSearchError::ApproximateIndexMissing);
        }
        for query in queries {
            checker.check()?;
            let dimension = u32::try_from(query.dimension())
                .map_err(|_| VectorSearchError::ApproximateIndexMissing)?;
            if dimension != query_dimension {
                return Err(VectorSearchError::ApproximateIndexMissing);
            }
        }

        if let Some(first_candidate_set) = candidate_sets.first()
            && candidate_sets.iter().skip(1).all(|candidate_set| {
                approx_batch::candidate_sets_match(first_candidate_set, candidate_set)
            })
        {
            let allowed_rows =
                self.vector_candidate_rows(first_candidate_set, index.rows(), &checker)?;
            if turbo_quant_exact::covers_rows(&allowed_rows, options) {
                let row_batches = vec![turbo_quant_exact::row_hits(&allowed_rows); queries.len()];
                return approx_batch::rerank_ann_row_candidate_batches(
                    self,
                    property,
                    queries,
                    options.metric,
                    options.k,
                    row_batches,
                    &checker,
                );
            }
            let row_batches = index
                .turbo_quant_candidates_batch_in_shared_rows(
                    queries,
                    options.k,
                    options.ef_search,
                    &allowed_rows,
                )
                .ok_or(VectorSearchError::ApproximateIndexMissing)?
                .map_err(GraphError::from)?;
            return approx_batch::rerank_ann_row_candidate_batches(
                self,
                property,
                queries,
                options.metric,
                options.k,
                row_batches,
                &checker,
            );
        }

        let allowed_rows = candidate_sets
            .iter()
            .map(|candidates| self.vector_candidate_rows(candidates, index.rows(), &checker))
            .collect::<Result<Vec<_>, _>>()?;
        if allowed_rows
            .iter()
            .all(|rows| turbo_quant_exact::covers_rows(rows, options))
        {
            let row_batches = allowed_rows
                .iter()
                .map(turbo_quant_exact::row_hits)
                .collect::<Vec<_>>();
            return approx_batch::rerank_ann_row_candidate_batches(
                self,
                property,
                queries,
                options.metric,
                options.k,
                row_batches,
                &checker,
            );
        }
        let row_batches = index
            .turbo_quant_candidates_batch_in_rows(
                queries,
                options.k,
                options.ef_search,
                &allowed_rows,
            )
            .ok_or(VectorSearchError::ApproximateIndexMissing)?
            .map_err(GraphError::from)?;
        approx_batch::rerank_ann_row_candidate_batches(
            self,
            property,
            queries,
            options.metric,
            options.k,
            row_batches,
            &checker,
        )
    }

    fn vector_candidate_rows(
        &self,
        candidates: &VectorCandidateSet,
        index_rows: &RoaringBitmap,
        checker: &CancellationChecker<'_>,
    ) -> Result<RoaringBitmap, VectorSearchError> {
        let candidates = self.bind_vector_candidate_set(candidates)?;
        let validated = self
            .validate_node_candidates(&candidates)
            .map_err(|error| GraphError::Inconsistent {
                reason: format!("bound TurboQuant candidates failed validation: {error}"),
            })?;
        validated.filter_index_rows(index_rows, checker)
    }
}
