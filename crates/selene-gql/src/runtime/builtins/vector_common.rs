//! Shared helpers for native vector and text-search built-ins.

use std::num::TryFromIntError;

use selene_core::{CoreError, DbString, NodeId, Value, VectorMetric, VectorValue};
use selene_graph::{GraphError, VectorCandidateSet, VectorNeighborDirection, VectorSearchError};

use crate::procedure_registry::ProcedureError;
use crate::runtime::reference_access::require_live_nodes;

/// Handling policy for vector batch-shape errors at a built-in boundary.
pub(super) enum BatchMismatch {
    /// Surface the storage-layer message as an invalid caller argument.
    InvalidArgument,
    /// Treat the error as an internal invariant violation with this message prefix.
    Internal(&'static str),
}

/// Parse a non-empty string argument.
pub(super) fn string_arg(
    proc_name: &'static str,
    value: &Value,
    name: &'static str,
) -> Result<DbString, ProcedureError> {
    let Value::String(value) = value else {
        return Err(invalid_arg(format!(
            "{proc_name} {name} must be a non-empty STRING"
        )));
    };
    if value.as_str().is_empty() {
        return Err(invalid_arg(format!(
            "{proc_name} {name} must be a non-empty STRING"
        )));
    }
    Ok(value.clone())
}

/// Parse a single vector query argument.
pub(super) fn query_arg(
    proc_name: &'static str,
    value: &Value,
) -> Result<VectorValue, ProcedureError> {
    let Value::Vector(query) = value else {
        return Err(invalid_arg(format!("{proc_name} query must be a VECTOR")));
    };
    Ok(query.clone())
}

/// Parse a batch of vector queries and enforce a stable dimension.
pub(super) fn queries_arg(
    proc_name: &'static str,
    value: &Value,
) -> Result<Vec<VectorValue>, ProcedureError> {
    let Value::List(values) = value else {
        return Err(invalid_arg(format!(
            "{proc_name} queries must be a LIST<VECTOR>"
        )));
    };
    let mut queries = Vec::with_capacity(values.len());
    let mut first_dimension = None;
    for (index, value) in values.iter().enumerate() {
        let Value::Vector(vector) = value else {
            return Err(invalid_arg(format!(
                "{proc_name} queries[{index}] must be a VECTOR"
            )));
        };
        match first_dimension {
            Some(dimension) if vector.dimension() != dimension => {
                return Err(invalid_arg(format!(
                    "{proc_name} queries must all have the same VECTOR dimension"
                )));
            }
            Some(_) => {}
            None => first_dimension = Some(vector.dimension()),
        }
        queries.push(vector.clone());
    }
    Ok(queries)
}

/// Parse one node reference argument.
pub(super) fn node_arg(
    graph: &selene_graph::SeleneGraph,
    proc_name: &'static str,
    value: &Value,
    name: &'static str,
) -> Result<NodeId, ProcedureError> {
    let Value::NodeRef(node_id) = value else {
        return Err(invalid_arg(format!("{proc_name} {name} must be a NODE")));
    };
    require_live_nodes(graph, std::iter::once(node_id))?;
    Ok(*node_id)
}

/// Parse explicit nodes for an operation that will inspect graph data.
pub(super) fn live_node_list_arg(
    graph: &selene_graph::SeleneGraph,
    proc_name: &'static str,
    value: &Value,
    name: &'static str,
) -> Result<Vec<NodeId>, ProcedureError> {
    let nodes = node_list_arg(proc_name, value, name)?;
    require_live_nodes(graph, nodes.iter())?;
    Ok(nodes)
}

/// Parse explicit batched nodes for an operation that inspects graph data.
pub(super) fn live_node_list_sets_arg(
    graph: &selene_graph::SeleneGraph,
    proc_name: &'static str,
    value: &Value,
    name: &'static str,
) -> Result<Vec<Vec<NodeId>>, ProcedureError> {
    let nodes = node_list_sets_arg(proc_name, value, name)?;
    require_live_nodes(graph, nodes.iter().flatten())?;
    Ok(nodes)
}

/// Parse a list of node references.
pub(super) fn node_list_arg(
    proc_name: &'static str,
    value: &Value,
    name: &'static str,
) -> Result<Vec<NodeId>, ProcedureError> {
    let Value::List(values) = value else {
        return Err(invalid_arg(format!(
            "{proc_name} {name} must be a LIST<NODE>"
        )));
    };
    let mut nodes = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let Value::NodeRef(node_id) = value else {
            return Err(invalid_arg(format!(
                "{proc_name} {name}[{index}] must be a NODE"
            )));
        };
        nodes.push(*node_id);
    }
    Ok(nodes)
}

/// Parse a list of node-reference lists.
pub(super) fn node_list_sets_arg(
    proc_name: &'static str,
    value: &Value,
    name: &'static str,
) -> Result<Vec<Vec<NodeId>>, ProcedureError> {
    let Value::List(values) = value else {
        return Err(invalid_arg(format!(
            "{proc_name} {name} must be a LIST<LIST<NODE>>"
        )));
    };
    let mut node_sets = Vec::with_capacity(values.len());
    for (set_index, value) in values.iter().enumerate() {
        let Value::List(values) = value else {
            return Err(invalid_arg(format!(
                "{proc_name} {name}[{set_index}] must be a LIST<NODE>"
            )));
        };
        let mut nodes = Vec::with_capacity(values.len());
        for (node_index, value) in values.iter().enumerate() {
            let Value::NodeRef(node_id) = value else {
                return Err(invalid_arg(format!(
                    "{proc_name} {name}[{set_index}][{node_index}] must be a NODE"
                )));
            };
            nodes.push(*node_id);
        }
        node_sets.push(nodes);
    }
    Ok(node_sets)
}

/// Parse a node list into a vector candidate set.
pub(super) fn candidate_set_arg(
    graph: &selene_graph::SeleneGraph,
    proc_name: &'static str,
    value: &Value,
    name: &'static str,
) -> Result<VectorCandidateSet, ProcedureError> {
    live_node_list_arg(graph, proc_name, value, name).map(VectorCandidateSet::from_nodes)
}

/// Parse node-list batches into vector candidate sets.
pub(super) fn candidate_sets_arg(
    graph: &selene_graph::SeleneGraph,
    proc_name: &'static str,
    value: &Value,
    name: &'static str,
) -> Result<Vec<VectorCandidateSet>, ProcedureError> {
    Ok(live_node_list_sets_arg(graph, proc_name, value, name)?
        .into_iter()
        .map(VectorCandidateSet::from_nodes)
        .collect())
}

/// Parse a non-negative result-count argument.
pub(super) fn cardinality_arg(
    proc_name: &'static str,
    value: &Value,
    name: &'static str,
) -> Result<usize, ProcedureError> {
    match value {
        Value::Int(value) if *value >= 0 => {
            usize::try_from(*value).map_err(|err| too_large(proc_name, err, name))
        }
        Value::Uint(value) => {
            usize::try_from(*value).map_err(|err| too_large(proc_name, err, name))
        }
        _ => Err(invalid_arg(format!(
            "{proc_name} {name} must be a non-negative INTEGER"
        ))),
    }
}

/// Parse a vector distance metric argument.
pub(super) fn metric_arg(
    proc_name: &'static str,
    value: &Value,
) -> Result<VectorMetric, ProcedureError> {
    let metric = string_arg(proc_name, value, "metric")?;
    let raw = metric.as_str();
    match raw.to_ascii_lowercase().as_str() {
        "squared_euclidean" | "sq_l2" | "l2" | "euclidean" => Ok(VectorMetric::SquaredEuclidean),
        "cosine" => Ok(VectorMetric::Cosine),
        "negative_inner_product" | "inner_product" | "mips" | "dot" => {
            Ok(VectorMetric::NegativeInnerProduct)
        }
        _ => Err(invalid_arg(format!(
            "unknown vector metric '{raw}'; expected squared_euclidean, cosine, or negative_inner_product"
        ))),
    }
}

/// Parse a neighbor-scoring direction argument.
pub(super) fn neighbor_direction_arg(
    proc_name: &'static str,
    value: &Value,
) -> Result<VectorNeighborDirection, ProcedureError> {
    direction_arg(proc_name, value, "neighbor")
}

/// Parse an expanded-candidate direction argument.
pub(super) fn expansion_direction_arg(
    proc_name: &'static str,
    value: &Value,
) -> Result<VectorNeighborDirection, ProcedureError> {
    direction_arg(proc_name, value, "expansion")
}

/// Convert an over-large query-index conversion into a procedure error.
pub(super) fn query_index_too_large(
    proc_name: &'static str,
    _err: TryFromIntError,
) -> ProcedureError {
    invalid_arg(format!(
        "{proc_name} query count is too large for this platform"
    ))
}

/// Map vector graph/search errors through the GQL procedure boundary.
pub(super) fn vector_search_error(
    error: VectorSearchError,
    graph_context: &'static str,
    batch_mismatch: BatchMismatch,
    approximate_context: &'static str,
) -> ProcedureError {
    match error {
        VectorSearchError::Graph(error) => graph_error(error, graph_context),
        VectorSearchError::Cancelled => ProcedureError::Cancelled,
        VectorSearchError::Timeout { elapsed } => ProcedureError::Timeout { elapsed },
        VectorSearchError::NodeScanBudgetExceeded { limit, scanned } => {
            ProcedureError::NodeScanBudgetExceeded { limit, scanned }
        }
        VectorSearchError::BatchLengthMismatch { .. } => match batch_mismatch {
            BatchMismatch::InvalidArgument => invalid_arg(format!("{error}")),
            BatchMismatch::Internal(prefix) => ProcedureError::Internal {
                detail: format!("{prefix}: {error}"),
            },
        },
        VectorSearchError::ApproximateIndexMissing
        | VectorSearchError::ApproximateMetricMismatch { .. } => ProcedureError::Internal {
            detail: format!("{approximate_context} received approximate-only error: {error}"),
        },
    }
}

/// Map ANN vector graph/search errors through the GQL procedure boundary.
pub(super) fn approximate_vector_search_error(
    proc_name: &'static str,
    error: VectorSearchError,
    graph_context: &'static str,
    batch_mismatch: BatchMismatch,
) -> ProcedureError {
    match error {
        VectorSearchError::Graph(error) => graph_error(error, graph_context),
        VectorSearchError::Cancelled => ProcedureError::Cancelled,
        VectorSearchError::Timeout { elapsed } => ProcedureError::Timeout { elapsed },
        VectorSearchError::NodeScanBudgetExceeded { limit, scanned } => {
            ProcedureError::NodeScanBudgetExceeded { limit, scanned }
        }
        VectorSearchError::BatchLengthMismatch { .. } => match batch_mismatch {
            BatchMismatch::InvalidArgument => invalid_arg(format!("{error}")),
            BatchMismatch::Internal(prefix) => ProcedureError::Internal {
                detail: format!("{prefix}: {error}"),
            },
        },
        VectorSearchError::ApproximateIndexMissing => {
            invalid_arg(format!("{proc_name} requires a matching ANN vector index"))
        }
        VectorSearchError::ApproximateMetricMismatch { indexed, requested } => {
            invalid_arg(format!(
                "{proc_name} requested {requested:?}, but the ANN vector index uses {indexed:?}"
            ))
        }
    }
}

/// Construct an invalid-argument procedure error.
pub(super) fn invalid_arg(detail: impl Into<String>) -> ProcedureError {
    ProcedureError::InvalidArgument {
        detail: detail.into(),
    }
}

fn direction_arg(
    proc_name: &'static str,
    value: &Value,
    context: &'static str,
) -> Result<VectorNeighborDirection, ProcedureError> {
    let direction = string_arg(proc_name, value, "direction")?;
    let raw = direction.as_str();
    match raw.to_ascii_lowercase().as_str() {
        "outgoing" | "out" => Ok(VectorNeighborDirection::Outgoing),
        "incoming" | "in" => Ok(VectorNeighborDirection::Incoming),
        "both" | "any" => Ok(VectorNeighborDirection::Both),
        _ => Err(invalid_arg(format!(
            "unknown vector {context} direction '{raw}'; expected outgoing, incoming, or both"
        ))),
    }
}

fn too_large(proc_name: &'static str, _err: TryFromIntError, name: &'static str) -> ProcedureError {
    invalid_arg(format!("{proc_name} {name} is too large for this platform"))
}

fn graph_error(error: GraphError, context: &'static str) -> ProcedureError {
    match error {
        GraphError::Core(core @ CoreError::VectorDimensionMismatch { .. })
        | GraphError::Core(core @ CoreError::VectorZeroNorm { .. }) => {
            invalid_arg(format!("{core}"))
        }
        GraphError::Inconsistent { reason } => ProcedureError::Internal {
            detail: format!("graph inconsistency during {context}: {reason}"),
        },
        other => ProcedureError::Internal {
            detail: format!("unexpected graph error during {context}: {other}"),
        },
    }
}
