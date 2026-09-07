//! Exact JSON search over graph node properties.
//!
//! This module is the JSON correctness oracle and small-corpus path. It scans
//! the current graph snapshot for JSON-valued node properties and returns node
//! candidates whose stored JSON matches the requested exact predicate.

use std::collections::BinaryHeap;
use std::time::Duration;

use selene_core::{
    CancellationCause, CancellationChecker, DbString, JsonPathSelector, JsonValue, JsonValueRef,
    NodeId,
};

use crate::error::{GraphError, GraphResult};
use crate::graph::SeleneGraph;
use crate::shared::SharedGraph;
use crate::validated_candidates::ValidatedNodeCandidates;

#[path = "json_search/parallel.rs"]
mod parallel;

pub(crate) const JSON_SEARCH_CANCEL_STRIDE: usize = 1024;
pub(crate) const JSON_SEARCH_PARALLEL_CHUNK_ROWS: usize = 2048;
#[cfg(not(test))]
pub(crate) const JSON_SEARCH_PARALLEL_MIN_ROWS: u64 = 16_384;
#[cfg(test)]
pub(crate) const JSON_SEARCH_PARALLEL_MIN_ROWS: u64 = 8;
/// Maximum selector count accepted by GQL JSON path search procedures.
pub const JSON_PATH_SELECTOR_LIMIT: usize = 64;

/// One JSON-containment node hit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonContainmentHit {
    /// Matched node id.
    pub node_id: NodeId,
}

/// One JSON path-existence node hit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonPathHit {
    /// Matched node id.
    pub node_id: NodeId,
}

/// One JSON path-containment node hit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonPathContainmentHit {
    /// Matched node id.
    pub node_id: NodeId,
}

/// One JSON path-value node hit.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonPathValueHit {
    /// Matched node id.
    pub node_id: NodeId,
    /// JSON value selected by the requested path.
    pub value: JsonValue,
}

/// Error returned by checked JSON search APIs.
#[derive(Debug, thiserror::Error)]
pub enum JsonSearchError {
    /// Graph storage or consistency failure.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// Caller requested cooperative cancellation.
    #[error("JSON search cancelled")]
    Cancelled,
    /// Statement deadline elapsed.
    #[error("JSON search timed out after {elapsed:?}")]
    Timeout {
        /// Wall-clock duration since the deadline elapsed.
        elapsed: Duration,
    },
    /// Deterministic node-scan budget was exceeded.
    #[error("JSON search node scan budget exceeded ({scanned} > {limit})")]
    NodeScanBudgetExceeded {
        /// Maximum allowed scanned nodes.
        limit: usize,
        /// Observed scanned nodes after the batch that crossed the limit.
        scanned: usize,
    },
}

impl JsonSearchError {
    pub(crate) fn into_graph_error(self) -> GraphError {
        match self {
            Self::Graph(error) => error,
            Self::Cancelled | Self::Timeout { .. } | Self::NodeScanBudgetExceeded { .. } => {
                GraphError::Inconsistent {
                    reason: format!("disabled JSON-search checker returned {self}"),
                }
            }
        }
    }
}

impl From<CancellationCause> for JsonSearchError {
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
    /// Exhaustively find JSON-valued node properties containing `candidate`.
    pub fn exact_json_contains_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        candidate: &JsonValue,
        k: usize,
    ) -> GraphResult<Vec<JsonContainmentHit>> {
        self.exact_json_contains_nodes_checked(
            label,
            property,
            candidate,
            k,
            CancellationChecker::disabled(),
        )
        .map_err(JsonSearchError::into_graph_error)
    }

    /// Exhaustively find JSON-valued node properties with cancellation checks.
    pub fn exact_json_contains_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        candidate: &JsonValue,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonContainmentHit>, JsonSearchError> {
        checker.check()?;
        if k == 0 {
            return Ok(Vec::new());
        }
        let label_candidates = self.json_label_candidates(label)?;
        if label_candidates.is_empty() {
            return Ok(Vec::new());
        }
        if parallel::should_parallelize_json_scan(label_candidates.len(), k) {
            let scan = parallel::JsonScan::new(property);
            return parallel::contains_nodes(
                scan,
                candidate,
                k,
                label_candidates.as_slice(),
                checker,
            );
        }

        let mut top_k = JsonContainmentTopK::new(k);
        let mut rows_since_check = 0usize;
        for &candidate_node in label_candidates.as_slice() {
            rows_since_check += 1;
            if rows_since_check >= JSON_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(rows_since_check)?;
                rows_since_check = 0;
            }
            let Some(value) = candidate_node.json_property(property)? else {
                continue;
            };
            if value.contains(candidate) {
                top_k.push(candidate_node.node_id());
            }
        }
        if rows_since_check > 0 {
            checker.note_nodes_scanned(rows_since_check)?;
        }
        Ok(top_k.into_hits())
    }

    /// Exhaustively find JSON-valued node properties where `path` exists.
    pub fn exact_json_path_exists_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        k: usize,
    ) -> GraphResult<Vec<JsonPathHit>> {
        self.exact_json_path_exists_nodes_checked(
            label,
            property,
            path,
            k,
            CancellationChecker::disabled(),
        )
        .map_err(JsonSearchError::into_graph_error)
    }

    /// Exhaustively find JSON-valued node properties with path-existence checks.
    pub fn exact_json_path_exists_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathHit>, JsonSearchError> {
        checker.check()?;
        if k == 0 || path.is_empty() {
            return Ok(Vec::new());
        }
        let label_candidates = self.json_label_candidates(label)?;
        if label_candidates.is_empty() {
            return Ok(Vec::new());
        }
        if parallel::should_parallelize_json_scan(label_candidates.len(), k) {
            let scan = parallel::JsonScan::new(property);
            return parallel::path_exists_nodes(
                scan,
                path,
                k,
                label_candidates.as_slice(),
                checker,
            );
        }

        let mut top_k = JsonContainmentTopK::new(k);
        let mut rows_since_check = 0usize;
        for &candidate_node in label_candidates.as_slice() {
            rows_since_check += 1;
            if rows_since_check >= JSON_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(rows_since_check)?;
                rows_since_check = 0;
            }
            let Some(value) = candidate_node.json_property(property)? else {
                continue;
            };
            if value.path_exists(path) {
                top_k.push(candidate_node.node_id());
            }
        }
        if rows_since_check > 0 {
            checker.note_nodes_scanned(rows_since_check)?;
        }
        Ok(top_k.into_path_hits())
    }

    /// Exhaustively find JSON-valued node properties whose selected path
    /// contains `candidate`.
    pub fn exact_json_path_contains_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidate: &JsonValue,
        k: usize,
    ) -> GraphResult<Vec<JsonPathContainmentHit>> {
        self.exact_json_path_contains_nodes_checked(
            label,
            property,
            path,
            candidate,
            k,
            CancellationChecker::disabled(),
        )
        .map_err(JsonSearchError::into_graph_error)
    }

    /// Exhaustively find JSON path containment matches with cancellation checks.
    pub fn exact_json_path_contains_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidate: &JsonValue,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathContainmentHit>, JsonSearchError> {
        checker.check()?;
        if k == 0 || path.is_empty() {
            return Ok(Vec::new());
        }
        let label_candidates = self.json_label_candidates(label)?;
        if label_candidates.is_empty() {
            return Ok(Vec::new());
        }
        if parallel::should_parallelize_json_scan(label_candidates.len(), k) {
            let scan = parallel::JsonScan::new(property);
            return parallel::path_contains_nodes(
                scan,
                path,
                candidate,
                k,
                label_candidates.as_slice(),
                checker,
            );
        }

        let mut top_k = JsonContainmentTopK::new(k);
        let mut rows_since_check = 0usize;
        for &candidate_node in label_candidates.as_slice() {
            rows_since_check += 1;
            if rows_since_check >= JSON_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(rows_since_check)?;
                rows_since_check = 0;
            }
            let Some(value) = candidate_node.json_property(property)? else {
                continue;
            };
            if value.path_contains(path, candidate) {
                top_k.push(candidate_node.node_id());
            }
        }
        if rows_since_check > 0 {
            checker.note_nodes_scanned(rows_since_check)?;
        }
        Ok(top_k.into_path_containment_hits())
    }

    /// Exhaustively find JSON-valued node properties where `path` selects a value.
    pub fn exact_json_path_value_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        k: usize,
    ) -> GraphResult<Vec<JsonPathValueHit>> {
        self.exact_json_path_value_nodes_checked(
            label,
            property,
            path,
            k,
            CancellationChecker::disabled(),
        )
        .map_err(JsonSearchError::into_graph_error)
    }

    /// Exhaustively find JSON path values with cancellation checks.
    pub fn exact_json_path_value_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathValueHit>, JsonSearchError> {
        checker.check()?;
        if k == 0 || path.is_empty() {
            return Ok(Vec::new());
        }
        let label_candidates = self.json_label_candidates(label)?;
        if label_candidates.is_empty() {
            return Ok(Vec::new());
        }
        if parallel::should_parallelize_json_scan(label_candidates.len(), k) {
            let scan = parallel::JsonScan::new(property);
            return parallel::path_value_nodes(scan, path, k, label_candidates.as_slice(), checker);
        }

        let mut top_k = JsonPathValueTopK::new(k);
        let mut rows_since_check = 0usize;
        for &candidate_node in label_candidates.as_slice() {
            rows_since_check += 1;
            if rows_since_check >= JSON_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(rows_since_check)?;
                rows_since_check = 0;
            }
            let Some(value) = candidate_node.json_property(property)? else {
                continue;
            };
            let Some(value) = value.path_value_ref(path) else {
                continue;
            };
            top_k.push(candidate_node.node_id(), value);
        }
        if rows_since_check > 0 {
            checker.note_nodes_scanned(rows_since_check)?;
        }
        Ok(top_k.into_hits())
    }

    fn json_label_candidates<'a>(
        &'a self,
        label: &DbString,
    ) -> Result<ValidatedNodeCandidates<'a>, JsonSearchError> {
        let candidates = self.node_candidates_with_label(label)?;
        self.validate_node_candidates(&candidates)
            .map_err(|error| GraphError::Inconsistent {
                reason: format!("fresh JSON label candidates failed validation: {error}"),
            })
            .map_err(JsonSearchError::from)
    }
}

impl SharedGraph {
    /// Exhaustively find JSON-valued node properties in the current snapshot.
    pub fn exact_json_contains_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        candidate: &JsonValue,
        k: usize,
    ) -> GraphResult<Vec<JsonContainmentHit>> {
        self.read()
            .exact_json_contains_nodes(label, property, candidate, k)
    }

    /// Exhaustively find JSON-valued node properties with cancellation checks.
    pub fn exact_json_contains_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        candidate: &JsonValue,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonContainmentHit>, JsonSearchError> {
        self.read()
            .exact_json_contains_nodes_checked(label, property, candidate, k, checker)
    }

    /// Exhaustively find JSON-valued node properties where `path` exists.
    pub fn exact_json_path_exists_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        k: usize,
    ) -> GraphResult<Vec<JsonPathHit>> {
        self.read()
            .exact_json_path_exists_nodes(label, property, path, k)
    }

    /// Exhaustively find JSON-valued node properties with path-existence checks.
    pub fn exact_json_path_exists_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathHit>, JsonSearchError> {
        self.read()
            .exact_json_path_exists_nodes_checked(label, property, path, k, checker)
    }

    /// Exhaustively find JSON-valued node properties whose selected path
    /// contains `candidate`.
    pub fn exact_json_path_contains_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidate: &JsonValue,
        k: usize,
    ) -> GraphResult<Vec<JsonPathContainmentHit>> {
        self.read()
            .exact_json_path_contains_nodes(label, property, path, candidate, k)
    }

    /// Exhaustively find JSON path containment matches with cancellation checks.
    pub fn exact_json_path_contains_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidate: &JsonValue,
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathContainmentHit>, JsonSearchError> {
        self.read()
            .exact_json_path_contains_nodes_checked(label, property, path, candidate, k, checker)
    }

    /// Exhaustively find JSON-valued node properties where `path` selects a value.
    pub fn exact_json_path_value_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        k: usize,
    ) -> GraphResult<Vec<JsonPathValueHit>> {
        self.read()
            .exact_json_path_value_nodes(label, property, path, k)
    }

    /// Exhaustively find JSON path values with cancellation checks.
    pub fn exact_json_path_value_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathValueHit>, JsonSearchError> {
        self.read()
            .exact_json_path_value_nodes_checked(label, property, path, k, checker)
    }
}

struct JsonContainmentTopK {
    k: usize,
    nodes: BinaryHeap<NodeId>,
}

impl JsonContainmentTopK {
    fn new(k: usize) -> Self {
        Self {
            k,
            nodes: BinaryHeap::with_capacity(k),
        }
    }

    fn push(&mut self, node_id: NodeId) {
        if self.k == 0 {
            return;
        }
        if self.nodes.len() < self.k {
            self.nodes.push(node_id);
            return;
        }
        let Some(mut max_node_id) = self.nodes.peek_mut() else {
            return;
        };
        if node_id < *max_node_id {
            *max_node_id = node_id;
        }
    }

    fn into_hits(self) -> Vec<JsonContainmentHit> {
        self.nodes
            .into_sorted_vec()
            .into_iter()
            .map(|node_id| JsonContainmentHit { node_id })
            .collect()
    }

    fn into_path_hits(self) -> Vec<JsonPathHit> {
        self.nodes
            .into_sorted_vec()
            .into_iter()
            .map(|node_id| JsonPathHit { node_id })
            .collect()
    }

    fn into_path_containment_hits(self) -> Vec<JsonPathContainmentHit> {
        self.nodes
            .into_sorted_vec()
            .into_iter()
            .map(|node_id| JsonPathContainmentHit { node_id })
            .collect()
    }
}

struct JsonPathValueCandidate {
    node_id: NodeId,
    value: JsonValue,
}

impl PartialEq for JsonPathValueCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id
    }
}

impl Eq for JsonPathValueCandidate {}

impl PartialOrd for JsonPathValueCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsonPathValueCandidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.node_id.cmp(&other.node_id)
    }
}

struct JsonPathValueTopK {
    k: usize,
    nodes: BinaryHeap<JsonPathValueCandidate>,
}

impl JsonPathValueTopK {
    fn new(k: usize) -> Self {
        Self {
            k,
            nodes: BinaryHeap::with_capacity(k),
        }
    }

    fn push(&mut self, node_id: NodeId, value: JsonValueRef<'_>) {
        self.push_with(node_id, || value.to_owned_json_value());
    }

    fn push_owned(&mut self, node_id: NodeId, value: JsonValue) {
        self.push_with(node_id, || value);
    }

    fn push_with(&mut self, node_id: NodeId, value: impl FnOnce() -> JsonValue) {
        if self.k == 0 {
            return;
        }
        if self.nodes.len() < self.k {
            self.nodes.push(JsonPathValueCandidate {
                node_id,
                value: value(),
            });
            return;
        }
        let Some(mut max_node) = self.nodes.peek_mut() else {
            return;
        };
        if node_id < max_node.node_id {
            *max_node = JsonPathValueCandidate {
                node_id,
                value: value(),
            };
        }
    }

    fn into_hits(self) -> Vec<JsonPathValueHit> {
        self.nodes
            .into_sorted_vec()
            .into_iter()
            .map(|hit| JsonPathValueHit {
                node_id: hit.node_id,
                value: hit.value,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
