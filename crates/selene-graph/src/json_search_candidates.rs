//! Candidate-scoped exact JSON search over graph node properties.

use selene_core::{CancellationChecker, DbString, JsonPathSelector, JsonValue, NodeId};

use crate::error::{GraphError, GraphResult};
use crate::graph::SeleneGraph;
use crate::json_search::{
    JSON_SEARCH_CANCEL_STRIDE, JsonContainmentHit, JsonPathContainmentHit, JsonPathHit,
    JsonPathValueHit, JsonSearchError,
};
use crate::shared::SharedGraph;

/// Inputs for candidate-scoped JSON path-containment search.
#[derive(Clone, Copy, Debug)]
pub struct JsonPathContainmentCandidateOptions<'a> {
    /// JSON path selector array.
    pub path: &'a [JsonPathSelector],
    /// JSON candidate the selected path value must contain.
    pub candidate: &'a JsonValue,
    /// Candidate nodes to filter.
    pub candidates: &'a [NodeId],
    /// Maximum result count.
    pub k: usize,
}

impl<'a> JsonPathContainmentCandidateOptions<'a> {
    /// Construct candidate-scoped JSON path-containment options.
    #[must_use]
    pub const fn new(
        path: &'a [JsonPathSelector],
        candidate: &'a JsonValue,
        candidates: &'a [NodeId],
        k: usize,
    ) -> Self {
        Self {
            path,
            candidate,
            candidates,
            k,
        }
    }
}

impl SeleneGraph {
    /// Find candidate nodes whose JSON property contains `candidate`.
    pub fn exact_json_contains_candidate_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        candidate: &JsonValue,
        candidates: &[NodeId],
        k: usize,
    ) -> GraphResult<Vec<JsonContainmentHit>> {
        self.exact_json_contains_candidate_nodes_checked(
            label,
            property,
            candidate,
            candidates,
            k,
            CancellationChecker::disabled(),
        )
        .map_err(JsonSearchError::into_graph_error)
    }

    /// Find candidate JSON containment matches with cancellation checks.
    pub fn exact_json_contains_candidate_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        candidate: &JsonValue,
        candidates: &[NodeId],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonContainmentHit>, JsonSearchError> {
        self.filter_json_candidate_nodes(label, property, candidates, k, checker, |json| {
            json.contains(candidate).then_some(None)
        })
        .map(|hits| {
            hits.into_iter()
                .map(|(node_id, _)| JsonContainmentHit { node_id })
                .collect()
        })
    }

    /// Find candidate nodes whose JSON property has `path`.
    pub fn exact_json_path_exists_candidate_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidates: &[NodeId],
        k: usize,
    ) -> GraphResult<Vec<JsonPathHit>> {
        self.exact_json_path_exists_candidate_nodes_checked(
            label,
            property,
            path,
            candidates,
            k,
            CancellationChecker::disabled(),
        )
        .map_err(JsonSearchError::into_graph_error)
    }

    /// Find candidate JSON path-existence matches with cancellation checks.
    pub fn exact_json_path_exists_candidate_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidates: &[NodeId],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathHit>, JsonSearchError> {
        if path.is_empty() {
            return Ok(Vec::new());
        }
        self.filter_json_candidate_nodes(label, property, candidates, k, checker, |json| {
            json.path_exists(path).then_some(None)
        })
        .map(|hits| {
            hits.into_iter()
                .map(|(node_id, _)| JsonPathHit { node_id })
                .collect()
        })
    }

    /// Find candidate nodes whose selected JSON path contains `candidate`.
    pub fn exact_json_path_contains_candidate_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        options: JsonPathContainmentCandidateOptions<'_>,
    ) -> GraphResult<Vec<JsonPathContainmentHit>> {
        self.exact_json_path_contains_candidate_nodes_checked(
            label,
            property,
            options,
            CancellationChecker::disabled(),
        )
        .map_err(JsonSearchError::into_graph_error)
    }

    /// Find candidate JSON path-containment matches with cancellation checks.
    pub fn exact_json_path_contains_candidate_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        options: JsonPathContainmentCandidateOptions<'_>,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathContainmentHit>, JsonSearchError> {
        if options.path.is_empty() {
            return Ok(Vec::new());
        }
        self.filter_json_candidate_nodes(
            label,
            property,
            options.candidates,
            options.k,
            checker,
            |json| {
                json.path_contains(options.path, options.candidate)
                    .then_some(None)
            },
        )
        .map(|hits| {
            hits.into_iter()
                .map(|(node_id, _)| JsonPathContainmentHit { node_id })
                .collect()
        })
    }

    /// Return selected JSON values for matching candidate nodes.
    pub fn exact_json_path_value_candidate_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidates: &[NodeId],
        k: usize,
    ) -> GraphResult<Vec<JsonPathValueHit>> {
        self.exact_json_path_value_candidate_nodes_checked(
            label,
            property,
            path,
            candidates,
            k,
            CancellationChecker::disabled(),
        )
        .map_err(JsonSearchError::into_graph_error)
    }

    /// Return selected candidate JSON path values with cancellation checks.
    pub fn exact_json_path_value_candidate_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidates: &[NodeId],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathValueHit>, JsonSearchError> {
        if path.is_empty() {
            return Ok(Vec::new());
        }
        self.filter_json_candidate_nodes(label, property, candidates, k, checker, |json| {
            json.path_value(path).map(Some)
        })
        .map(|hits| {
            hits.into_iter()
                .filter_map(|(node_id, value)| {
                    value.map(|value| JsonPathValueHit { node_id, value })
                })
                .collect()
        })
    }

    fn filter_json_candidate_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        candidates: &[NodeId],
        k: usize,
        checker: CancellationChecker<'_>,
        mut predicate: impl FnMut(&JsonValue) -> Option<Option<JsonValue>>,
    ) -> Result<Vec<(NodeId, Option<JsonValue>)>, JsonSearchError> {
        checker.check()?;
        if k == 0 || candidates.is_empty() {
            return Ok(Vec::new());
        }
        let candidates = self.bind_node_candidates(candidates.iter().copied())?;
        let validated = self
            .validate_node_candidates(&candidates)
            .map_err(|error| GraphError::Inconsistent {
                reason: format!("fresh JSON candidates failed validation: {error}"),
            })?;
        let mut hits = Vec::new();
        let hit_capacity = k.min(validated.len()).min(JSON_SEARCH_CANCEL_STRIDE);
        let mut candidates_since_check = 0usize;
        for &candidate in validated.as_slice() {
            candidates_since_check += 1;
            if candidates_since_check >= JSON_SEARCH_CANCEL_STRIDE {
                checker.note_nodes_scanned(candidates_since_check)?;
                candidates_since_check = 0;
            }
            if !candidate.has_label(label)? {
                continue;
            }
            let Some(value) = candidate.json_property(property)? else {
                continue;
            };
            if let Some(selected) = predicate(value) {
                if hits.is_empty() {
                    hits.reserve(hit_capacity);
                }
                hits.push((candidate.node_id(), selected));
                if hits.len() == k {
                    break;
                }
            }
        }
        if candidates_since_check > 0 {
            checker.note_nodes_scanned(candidates_since_check)?;
        }
        Ok(hits)
    }
}

impl SharedGraph {
    /// Find candidate nodes whose JSON property contains `candidate`.
    pub fn exact_json_contains_candidate_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        candidate: &JsonValue,
        candidates: &[NodeId],
        k: usize,
    ) -> GraphResult<Vec<JsonContainmentHit>> {
        self.read()
            .exact_json_contains_candidate_nodes(label, property, candidate, candidates, k)
    }

    /// Find candidate JSON containment matches with cancellation checks.
    pub fn exact_json_contains_candidate_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        candidate: &JsonValue,
        candidates: &[NodeId],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonContainmentHit>, JsonSearchError> {
        self.read().exact_json_contains_candidate_nodes_checked(
            label, property, candidate, candidates, k, checker,
        )
    }

    /// Find candidate nodes whose JSON property has `path`.
    pub fn exact_json_path_exists_candidate_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidates: &[NodeId],
        k: usize,
    ) -> GraphResult<Vec<JsonPathHit>> {
        self.read()
            .exact_json_path_exists_candidate_nodes(label, property, path, candidates, k)
    }

    /// Find candidate JSON path-existence matches with cancellation checks.
    pub fn exact_json_path_exists_candidate_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidates: &[NodeId],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathHit>, JsonSearchError> {
        self.read().exact_json_path_exists_candidate_nodes_checked(
            label, property, path, candidates, k, checker,
        )
    }

    /// Find candidate nodes whose selected JSON path contains `candidate`.
    pub fn exact_json_path_contains_candidate_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        options: JsonPathContainmentCandidateOptions<'_>,
    ) -> GraphResult<Vec<JsonPathContainmentHit>> {
        self.read()
            .exact_json_path_contains_candidate_nodes(label, property, options)
    }

    /// Find candidate JSON path-containment matches with cancellation checks.
    pub fn exact_json_path_contains_candidate_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        options: JsonPathContainmentCandidateOptions<'_>,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathContainmentHit>, JsonSearchError> {
        self.read()
            .exact_json_path_contains_candidate_nodes_checked(label, property, options, checker)
    }

    /// Return selected JSON values for matching candidate nodes.
    pub fn exact_json_path_value_candidate_nodes(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidates: &[NodeId],
        k: usize,
    ) -> GraphResult<Vec<JsonPathValueHit>> {
        self.read()
            .exact_json_path_value_candidate_nodes(label, property, path, candidates, k)
    }

    /// Return selected candidate JSON path values with cancellation checks.
    pub fn exact_json_path_value_candidate_nodes_checked(
        &self,
        label: &DbString,
        property: &DbString,
        path: &[JsonPathSelector],
        candidates: &[NodeId],
        k: usize,
        checker: CancellationChecker<'_>,
    ) -> Result<Vec<JsonPathValueHit>, JsonSearchError> {
        self.read().exact_json_path_value_candidate_nodes_checked(
            label, property, path, candidates, k, checker,
        )
    }
}
