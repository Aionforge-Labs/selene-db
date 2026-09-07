//! Threshold-gated Rayon helpers for global exact JSON scans.

use selene_core::{CancellationChecker, DbString, JsonPathSelector, JsonValue, NodeId};

use crate::parallel_scan::{should_parallelize_scan, try_reduce_chunks};
use crate::validated_candidates::ValidatedCandidateNode;

use super::{
    JSON_SEARCH_PARALLEL_CHUNK_ROWS, JSON_SEARCH_PARALLEL_MIN_ROWS, JsonContainmentHit,
    JsonContainmentTopK, JsonPathContainmentHit, JsonPathHit, JsonPathValueHit, JsonPathValueTopK,
    JsonSearchError,
};

/// Borrowed inputs shared by every global JSON scan chunk.
#[derive(Clone, Copy)]
pub(super) struct JsonScan<'a> {
    property: &'a DbString,
}

impl<'a> JsonScan<'a> {
    /// Build shared scan inputs for a label/property JSON scan.
    pub(super) fn new(property: &'a DbString) -> Self {
        Self { property }
    }

    fn value_for_candidate(
        self,
        candidate: ValidatedCandidateNode<'a>,
    ) -> Result<Option<(NodeId, &'a JsonValue)>, JsonSearchError> {
        let value = candidate.json_property(self.property)?;
        Ok(value.map(|val| (candidate.node_id(), val)))
    }
}

/// Return true when a global JSON scan should use Rayon.
pub(super) fn should_parallelize_json_scan(row_count: usize, k: usize) -> bool {
    should_parallelize_scan(row_count as u64, k, JSON_SEARCH_PARALLEL_MIN_ROWS)
}

/// Parallel implementation of JSON containment scan.
pub(super) fn contains_nodes(
    scan: JsonScan<'_>,
    candidate: &JsonValue,
    k: usize,
    candidates: &[ValidatedCandidateNode<'_>],
    checker: CancellationChecker<'_>,
) -> Result<Vec<JsonContainmentHit>, JsonSearchError> {
    let top_k = try_reduce_chunks(
        candidates,
        JSON_SEARCH_PARALLEL_CHUNK_ROWS,
        checker,
        || JsonContainmentTopK::new(k),
        |chunk| contains_chunk(scan, candidate, k, chunk),
        merge_node_top_k,
    )?;
    Ok(top_k.into_hits())
}

/// Parallel implementation of JSON path-existence scan.
pub(super) fn path_exists_nodes(
    scan: JsonScan<'_>,
    path: &[JsonPathSelector],
    k: usize,
    candidates: &[ValidatedCandidateNode<'_>],
    checker: CancellationChecker<'_>,
) -> Result<Vec<JsonPathHit>, JsonSearchError> {
    let top_k = try_reduce_chunks(
        candidates,
        JSON_SEARCH_PARALLEL_CHUNK_ROWS,
        checker,
        || JsonContainmentTopK::new(k),
        |chunk| path_exists_chunk(scan, path, k, chunk),
        merge_node_top_k,
    )?;
    Ok(top_k.into_path_hits())
}

/// Parallel implementation of JSON path-containment scan.
pub(super) fn path_contains_nodes(
    scan: JsonScan<'_>,
    path: &[JsonPathSelector],
    candidate: &JsonValue,
    k: usize,
    candidates: &[ValidatedCandidateNode<'_>],
    checker: CancellationChecker<'_>,
) -> Result<Vec<JsonPathContainmentHit>, JsonSearchError> {
    let top_k = try_reduce_chunks(
        candidates,
        JSON_SEARCH_PARALLEL_CHUNK_ROWS,
        checker,
        || JsonContainmentTopK::new(k),
        |chunk| path_contains_chunk(scan, path, candidate, k, chunk),
        merge_node_top_k,
    )?;
    Ok(top_k.into_path_containment_hits())
}

/// Parallel implementation of JSON path-value scan.
pub(super) fn path_value_nodes(
    scan: JsonScan<'_>,
    path: &[JsonPathSelector],
    k: usize,
    candidates: &[ValidatedCandidateNode<'_>],
    checker: CancellationChecker<'_>,
) -> Result<Vec<JsonPathValueHit>, JsonSearchError> {
    let top_k = try_reduce_chunks(
        candidates,
        JSON_SEARCH_PARALLEL_CHUNK_ROWS,
        checker,
        || JsonPathValueTopK::new(k),
        |chunk| path_value_chunk(scan, path, k, chunk),
        merge_value_top_k,
    )?;
    Ok(top_k.into_hits())
}

fn contains_chunk(
    scan: JsonScan<'_>,
    candidate: &JsonValue,
    k: usize,
    candidates: &[ValidatedCandidateNode<'_>],
) -> Result<JsonContainmentTopK, JsonSearchError> {
    let mut top_k = JsonContainmentTopK::new(k);
    for &entry in candidates {
        let Some((node_id, value)) = scan.value_for_candidate(entry)? else {
            continue;
        };
        if value.contains(candidate) {
            top_k.push(node_id);
        }
    }
    Ok(top_k)
}

fn path_exists_chunk(
    scan: JsonScan<'_>,
    path: &[JsonPathSelector],
    k: usize,
    candidates: &[ValidatedCandidateNode<'_>],
) -> Result<JsonContainmentTopK, JsonSearchError> {
    let mut top_k = JsonContainmentTopK::new(k);
    for &entry in candidates {
        let Some((node_id, value)) = scan.value_for_candidate(entry)? else {
            continue;
        };
        if value.path_exists(path) {
            top_k.push(node_id);
        }
    }
    Ok(top_k)
}

fn path_contains_chunk(
    scan: JsonScan<'_>,
    path: &[JsonPathSelector],
    candidate: &JsonValue,
    k: usize,
    candidates: &[ValidatedCandidateNode<'_>],
) -> Result<JsonContainmentTopK, JsonSearchError> {
    let mut top_k = JsonContainmentTopK::new(k);
    for &entry in candidates {
        let Some((node_id, value)) = scan.value_for_candidate(entry)? else {
            continue;
        };
        if value.path_contains(path, candidate) {
            top_k.push(node_id);
        }
    }
    Ok(top_k)
}

fn path_value_chunk(
    scan: JsonScan<'_>,
    path: &[JsonPathSelector],
    k: usize,
    candidates: &[ValidatedCandidateNode<'_>],
) -> Result<JsonPathValueTopK, JsonSearchError> {
    let mut top_k = JsonPathValueTopK::new(k);
    for &entry in candidates {
        let Some((node_id, value)) = scan.value_for_candidate(entry)? else {
            continue;
        };
        let Some(selected) = value.path_value_ref(path) else {
            continue;
        };
        top_k.push(node_id, selected);
    }
    Ok(top_k)
}

fn merge_node_top_k(
    mut lhs: JsonContainmentTopK,
    rhs: JsonContainmentTopK,
) -> Result<JsonContainmentTopK, JsonSearchError> {
    for hit in rhs.into_hits() {
        lhs.push(hit.node_id);
    }
    Ok(lhs)
}

fn merge_value_top_k(
    mut lhs: JsonPathValueTopK,
    rhs: JsonPathValueTopK,
) -> Result<JsonPathValueTopK, JsonSearchError> {
    for hit in rhs.into_hits() {
        lhs.push_owned(hit.node_id, hit.value);
    }
    Ok(lhs)
}
