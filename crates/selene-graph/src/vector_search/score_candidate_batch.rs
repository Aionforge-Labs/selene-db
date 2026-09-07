use rayon::prelude::*;
use selene_core::{
    CancellationChecker, DbString, NodeId, VectorMetric, VectorMetricQuery, VectorTopK, VectorValue,
};

use crate::error::GraphError;
use crate::graph::SeleneGraph;
use crate::parallel_scan::{should_parallelize_scan, try_reduce_chunks};
use crate::validated_candidates::{ValidatedCandidateNode, ValidatedNodeCandidates};
use crate::{CandidateSet, Node};

use super::score::validate_batch_inputs;
use super::{
    VECTOR_SEARCH_CANCEL_STRIDE, VectorCandidateSet, VectorNodeSearchHit, VectorSearchError,
    vector_node_hits,
};

#[cfg(not(test))]
const VECTOR_CANDIDATE_BATCH_PARALLEL_MIN_TOTAL_NODES: usize = 4096;
#[cfg(test)]
const VECTOR_CANDIDATE_BATCH_PARALLEL_MIN_TOTAL_NODES: usize = 8;
#[cfg(not(test))]
const VECTOR_REPEATED_CANDIDATE_BATCH_PARALLEL_MIN_CANDIDATES: usize = 4096;
#[cfg(test)]
const VECTOR_REPEATED_CANDIDATE_BATCH_PARALLEL_MIN_CANDIDATES: usize = 4;
#[cfg(not(test))]
const VECTOR_REPEATED_CANDIDATE_BATCH_PARALLEL_CHUNK_NODES: usize = 32;
#[cfg(test)]
const VECTOR_REPEATED_CANDIDATE_BATCH_PARALLEL_CHUNK_NODES: usize = 2;
const VECTOR_CANDIDATE_BATCH_GROUP_MAX_SETS: usize = 128;

struct CandidateBatchScore<'a> {
    property: &'a DbString,
    queries: &'a [VectorValue],
    metric: VectorMetric,
    k: usize,
    checker: CancellationChecker<'a>,
}

impl SeleneGraph {
    pub(super) fn score_vector_node_id_sets_batch_bound_checked<C>(
        &self,
        property: &DbString,
        queries: &[VectorValue],
        candidate_sets: &[C],
        metric: VectorMetric,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<Vec<VectorNodeSearchHit>>, VectorSearchError>
    where
        C: AsRef<[NodeId]>,
    {
        checker.check()?;
        validate_batch_inputs(queries, candidate_sets.len())?;
        if queries.is_empty() {
            return Ok(Vec::new());
        }
        if k == 0 {
            return Ok(vec![Vec::new(); queries.len()]);
        }
        let mut bound_sets = Vec::<CandidateSet<Node>>::with_capacity(candidate_sets.len());
        for (index, set) in candidate_sets.iter().enumerate() {
            if let Some(previous) = candidate_sets[..index]
                .iter()
                .position(|other| other.as_ref() == set.as_ref())
            {
                bound_sets.push(bound_sets[previous].clone());
            } else {
                bound_sets.push(self.bind_node_candidates(set.as_ref().iter().copied())?);
            }
        }
        self.score_bound_candidate_sets_batch(property, queries, &bound_sets, metric, k, checker)
    }

    pub(super) fn score_vector_candidate_sets_batch_bound_checked(
        &self,
        property: &DbString,
        queries: &[VectorValue],
        candidate_sets: &[VectorCandidateSet],
        metric: VectorMetric,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<Vec<VectorNodeSearchHit>>, VectorSearchError> {
        checker.check()?;
        validate_batch_inputs(queries, candidate_sets.len())?;
        if queries.is_empty() {
            return Ok(Vec::new());
        }
        if k == 0 {
            return Ok(vec![Vec::new(); queries.len()]);
        }
        let mut bound_sets = Vec::<CandidateSet<Node>>::with_capacity(candidate_sets.len());
        for (index, set) in candidate_sets.iter().enumerate() {
            if let Some(previous) = candidate_sets[..index]
                .iter()
                .position(|other| other.as_nodes() == set.as_nodes())
            {
                bound_sets.push(bound_sets[previous].clone());
            } else {
                bound_sets.push(self.bind_vector_candidate_set(set)?);
            }
        }
        self.score_bound_candidate_sets_batch(property, queries, &bound_sets, metric, k, checker)
    }

    pub(crate) fn score_bound_candidate_sets_batch(
        &self,
        property: &DbString,
        queries: &[VectorValue],
        candidate_sets: &[CandidateSet<Node>],
        metric: VectorMetric,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<Vec<VectorNodeSearchHit>>, VectorSearchError> {
        let candidate_rows = self.materialize_candidate_sets(candidate_sets)?;
        let should_parallelize_batch =
            should_parallelize_candidate_batch_scoring(&candidate_rows, k);
        if let Some(candidates) = candidate_rows.first()
            && should_parallelize_repeated_candidate_batch(queries.len(), candidates.len(), k)
            && candidate_sets_all_match(&candidate_rows)
        {
            return self.score_repeated_vector_candidate_set_batch_parallel(
                property,
                queries,
                candidates.as_slice(),
                metric,
                k,
                checker,
            );
        }
        if should_parallelize_batch {
            return self.score_vector_candidate_sets_batch_parallel(
                property,
                queries,
                &candidate_rows,
                metric,
                k,
                checker,
            );
        }
        if candidate_sets_all_match(&candidate_rows) {
            return self.score_repeated_vector_candidate_set_batch_serial(
                property,
                queries,
                candidate_rows[0].as_slice(),
                metric,
                k,
                checker,
            );
        }
        self.score_vector_candidate_sets_batch_grouped_serial(
            property,
            queries,
            &candidate_rows,
            metric,
            k,
            checker,
        )
    }

    pub(super) fn score_vector_candidate_sets_batch_parallel(
        &self,
        property: &DbString,
        queries: &[VectorValue],
        candidate_sets: &[ValidatedNodeCandidates<'_>],
        metric: VectorMetric,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<Vec<VectorNodeSearchHit>>, VectorSearchError> {
        queries
            .par_iter()
            .zip(candidate_sets.par_iter())
            .map(|(query, candidates)| {
                checker.check()?;
                self.score_bound_candidate_set(property, query, candidates, metric, k, checker)
            })
            .collect()
    }

    pub(super) fn score_repeated_vector_candidate_set_batch_serial(
        &self,
        property: &DbString,
        queries: &[VectorValue],
        candidates: &[ValidatedCandidateNode<'_>],
        metric: VectorMetric,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<Vec<VectorNodeSearchHit>>, VectorSearchError> {
        checker.check()?;
        if candidates.is_empty() {
            return Ok(vec![Vec::new(); queries.len()]);
        }

        let mut scorers = Vec::with_capacity(queries.len());
        for query in queries {
            scorers.push(metric.bind_query(query).map_err(GraphError::from)?);
        }

        let mut top_ks = (0..queries.len())
            .map(|_| VectorTopK::new(k))
            .collect::<Vec<_>>();
        let mut candidates_since_check = 0usize;
        for &candidate in candidates {
            candidates_since_check += 1;
            if candidates_since_check >= VECTOR_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(candidates_since_check)?;
                candidates_since_check = 0;
            }
            let Some(vector) = candidate.vector_property(property)? else {
                continue;
            };
            for (scorer, top_k) in scorers.iter().zip(top_ks.iter_mut()) {
                let distance = scorer.distance(vector).map_err(GraphError::from)?;
                top_k.push_distance(candidate.node_id(), distance);
            }
        }
        if candidates_since_check > 0 {
            checker.note_nodes_scanned(candidates_since_check)?;
        }

        Ok(top_ks.into_iter().map(vector_node_hits).collect())
    }

    pub(super) fn score_repeated_vector_candidate_set_batch_parallel(
        &self,
        property: &DbString,
        queries: &[VectorValue],
        candidates: &[ValidatedCandidateNode<'_>],
        metric: VectorMetric,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<Vec<VectorNodeSearchHit>>, VectorSearchError> {
        checker.check()?;
        if candidates.is_empty() {
            return Ok(vec![Vec::new(); queries.len()]);
        }

        let scorers = queries
            .iter()
            .map(|query| metric.bind_query(query).map_err(GraphError::from))
            .collect::<Result<Vec<_>, _>>()?;
        let top_ks = try_reduce_chunks(
            candidates,
            VECTOR_REPEATED_CANDIDATE_BATCH_PARALLEL_CHUNK_NODES,
            checker,
            || new_batch_top_ks(queries.len(), k),
            |chunk| self.score_repeated_vector_candidate_set_chunk(property, &scorers, chunk, k),
            merge_batch_top_ks,
        )?;

        Ok(top_ks.into_iter().map(vector_node_hits).collect())
    }

    pub(super) fn score_vector_candidate_sets_batch_grouped_serial(
        &self,
        property: &DbString,
        queries: &[VectorValue],
        candidate_sets: &[ValidatedNodeCandidates<'_>],
        metric: VectorMetric,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<Vec<VectorNodeSearchHit>>, VectorSearchError> {
        let groups = repeated_candidate_set_groups(candidate_sets);
        if groups.is_empty() {
            let mut batch_hits = Vec::with_capacity(queries.len());
            for (query, candidates) in queries.iter().zip(candidate_sets) {
                checker.check()?;
                batch_hits.push(
                    self.score_bound_candidate_set(
                        property, query, candidates, metric, k, checker,
                    )?,
                );
            }
            return Ok(batch_hits);
        }

        let mut batch_hits = (0..queries.len()).map(|_| None).collect::<Vec<_>>();
        let score = CandidateBatchScore {
            property,
            queries,
            metric,
            k,
            checker,
        };
        for group in groups {
            let hits = self.score_repeated_vector_candidate_set_indexed_serial(
                &score,
                &group,
                candidate_sets[group[0]].as_slice(),
            )?;
            for (query_index, hits) in group.into_iter().zip(hits) {
                batch_hits[query_index] = Some(hits);
            }
        }
        for (query_index, (query, candidates)) in queries.iter().zip(candidate_sets).enumerate() {
            if batch_hits[query_index].is_some() {
                continue;
            }
            checker.check()?;
            batch_hits[query_index] = Some(
                self.score_bound_candidate_set(property, query, candidates, metric, k, checker)?,
            );
        }

        Ok(batch_hits
            .into_iter()
            .map(|hits| hits.expect("batched vector scoring fills every query slot"))
            .collect())
    }

    fn score_repeated_vector_candidate_set_indexed_serial(
        &self,
        score: &CandidateBatchScore<'_>,
        query_indices: &[usize],
        candidates: &[ValidatedCandidateNode<'_>],
    ) -> Result<Vec<Vec<VectorNodeSearchHit>>, VectorSearchError> {
        score.checker.check()?;
        if candidates.is_empty() {
            return Ok(vec![Vec::new(); query_indices.len()]);
        }

        let mut scorers = Vec::with_capacity(query_indices.len());
        for query_index in query_indices {
            scorers.push(
                score
                    .metric
                    .bind_query(&score.queries[*query_index])
                    .map_err(GraphError::from)?,
            );
        }

        let mut top_ks = (0..query_indices.len())
            .map(|_| VectorTopK::new(score.k))
            .collect::<Vec<_>>();
        let mut candidates_since_check = 0usize;
        for &candidate in candidates {
            candidates_since_check += 1;
            if candidates_since_check >= VECTOR_SEARCH_CANCEL_STRIDE {
                score.checker.note_nodes_scanned(candidates_since_check)?;
                candidates_since_check = 0;
            }
            let Some(vector) = candidate.vector_property(score.property)? else {
                continue;
            };
            for (scorer, top_k) in scorers.iter().zip(top_ks.iter_mut()) {
                let distance = scorer.distance(vector).map_err(GraphError::from)?;
                top_k.push_distance(candidate.node_id(), distance);
            }
        }
        if candidates_since_check > 0 {
            score.checker.note_nodes_scanned(candidates_since_check)?;
        }

        Ok(top_ks.into_iter().map(vector_node_hits).collect())
    }

    fn score_repeated_vector_candidate_set_chunk(
        &self,
        property: &DbString,
        scorers: &[VectorMetricQuery<'_>],
        candidates: &[ValidatedCandidateNode<'_>],
        k: usize,
    ) -> Result<Vec<VectorTopK<NodeId>>, VectorSearchError> {
        let mut top_ks = new_batch_top_ks(scorers.len(), k);
        for &candidate in candidates {
            let Some(vector) = candidate.vector_property(property)? else {
                continue;
            };
            for (scorer, top_k) in scorers.iter().zip(top_ks.iter_mut()) {
                let distance = scorer.distance(vector).map_err(GraphError::from)?;
                top_k.push_distance(candidate.node_id(), distance);
            }
        }
        Ok(top_ks)
    }

    fn score_bound_candidate_set(
        &self,
        property: &DbString,
        query: &VectorValue,
        candidates: &ValidatedNodeCandidates<'_>,
        metric: VectorMetric,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<VectorNodeSearchHit>, VectorSearchError> {
        checker.check()?;
        let scorer = metric.bind_query(query).map_err(GraphError::from)?;
        self.score_vector_candidate_set_serial(property, scorer, candidates.as_slice(), k, checker)
    }

    fn materialize_candidate_sets<'a>(
        &'a self,
        candidate_sets: &[CandidateSet<Node>],
    ) -> Result<Vec<ValidatedNodeCandidates<'a>>, VectorSearchError> {
        let mut materialized: Vec<ValidatedNodeCandidates<'a>> =
            Vec::with_capacity(candidate_sets.len());
        for (index, candidates) in candidate_sets.iter().enumerate() {
            if let Some(previous) = candidate_sets[..index]
                .iter()
                .position(|other| candidate_sets_match(other, candidates))
            {
                materialized.push(materialized[previous].clone());
                continue;
            }
            let validated = self.validate_node_candidates(candidates).map_err(|error| {
                GraphError::Inconsistent {
                    reason: format!("bound batch-vector candidates failed validation: {error}"),
                }
            })?;
            materialized.push(validated);
        }
        Ok(materialized)
    }
}

pub(super) fn candidate_sets_all_match(candidate_sets: &[ValidatedNodeCandidates<'_>]) -> bool {
    let Some(first) = candidate_sets.first() else {
        return false;
    };
    candidate_sets.len() > 1
        && candidate_sets
            .iter()
            .skip(1)
            .all(|candidates| materialized_sets_match(first, candidates))
}

pub(super) fn should_parallelize_candidate_batch_scoring(
    candidate_sets: &[ValidatedNodeCandidates<'_>],
    k: usize,
) -> bool {
    if candidate_sets.len() <= 1 {
        return false;
    }
    let mut total_candidates = 0_usize;
    let mut max_candidates = 0_usize;
    let mut non_empty_sets = 0_usize;
    for candidate_count in candidate_sets.iter().map(|candidates| candidates.len()) {
        total_candidates += candidate_count;
        max_candidates = max_candidates.max(candidate_count);
        non_empty_sets += usize::from(candidate_count != 0);
    }
    if non_empty_sets <= 1 || max_candidates.saturating_mul(2) > total_candidates {
        return false;
    }

    should_parallelize_scan(
        total_candidates as u64,
        k,
        VECTOR_CANDIDATE_BATCH_PARALLEL_MIN_TOTAL_NODES as u64,
    )
}

pub(super) fn should_parallelize_repeated_candidate_batch(
    query_count: usize,
    candidate_count: usize,
    k: usize,
) -> bool {
    query_count > 1
        && candidate_count >= VECTOR_REPEATED_CANDIDATE_BATCH_PARALLEL_MIN_CANDIDATES
        && should_parallelize_scan(
            query_count.saturating_mul(candidate_count) as u64,
            k,
            VECTOR_CANDIDATE_BATCH_PARALLEL_MIN_TOTAL_NODES as u64,
        )
}

fn candidate_sets_match(lhs: &CandidateSet<Node>, rhs: &CandidateSet<Node>) -> bool {
    lhs.len() == rhs.len() && lhs.iter().eq(rhs.iter())
}

fn materialized_sets_match(
    lhs: &ValidatedNodeCandidates<'_>,
    rhs: &ValidatedNodeCandidates<'_>,
) -> bool {
    lhs.ptr_eq(rhs) || (lhs.len() == rhs.len() && lhs.node_ids().eq(rhs.node_ids()))
}

fn repeated_candidate_set_groups(
    candidate_sets: &[ValidatedNodeCandidates<'_>],
) -> Vec<Vec<usize>> {
    if candidate_sets.len() <= 2 || candidate_sets.len() > VECTOR_CANDIDATE_BATCH_GROUP_MAX_SETS {
        return Vec::new();
    }
    let mut assigned = vec![false; candidate_sets.len()];
    let mut groups = Vec::new();
    for index in 0..candidate_sets.len() {
        if assigned[index] {
            continue;
        }
        let mut group = Vec::new();
        for next in index + 1..candidate_sets.len() {
            if !assigned[next]
                && materialized_sets_match(&candidate_sets[index], &candidate_sets[next])
            {
                if group.is_empty() {
                    group.push(index);
                    assigned[index] = true;
                }
                group.push(next);
                assigned[next] = true;
            }
        }
        if group.len() > 1 {
            groups.push(group);
        }
    }
    groups
}

fn new_batch_top_ks(query_count: usize, k: usize) -> Vec<VectorTopK<NodeId>> {
    (0..query_count).map(|_| VectorTopK::new(k)).collect()
}

fn merge_batch_top_ks(
    mut lhs: Vec<VectorTopK<NodeId>>,
    rhs: Vec<VectorTopK<NodeId>>,
) -> Result<Vec<VectorTopK<NodeId>>, VectorSearchError> {
    debug_assert_eq!(lhs.len(), rhs.len());
    for (lhs_top_k, rhs_top_k) in lhs.iter_mut().zip(rhs) {
        for hit in rhs_top_k.into_hits() {
            lhs_top_k.push_distance(hit.key, hit.distance);
        }
    }
    Ok(lhs)
}
