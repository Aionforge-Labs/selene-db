//! Error-mapping helpers for the native `algo.*` procedures.
//!
//! Ported verbatim from the historical procedure-pack error adapter so the
//! user-visible `ProcedureError` detail strings (and their GQLSTATUS mapping)
//! are byte-identical to the pack era. The native runners
//! call the underlying `selene_algorithms` `*_with_checker` functions directly,
//! so these mappers receive the raw algorithm error types and render the same
//! procedure-qualified messages.

use selene_algorithms::{AlgorithmAborted, AlgorithmsError, PathfindingError, TopoSortError};
use selene_core::CancellationCause;

use crate::ProcedureError;

pub(super) fn invalid_argument(detail: impl Into<String>) -> ProcedureError {
    ProcedureError::InvalidArgument {
        detail: detail.into(),
    }
}

pub(super) fn algorithm_error(error: AlgorithmsError) -> ProcedureError {
    native_error(error.to_string(), error)
}

fn native_error(
    detail: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ProcedureError {
    ProcedureError::Native {
        detail: detail.into(),
        source: std::sync::Arc::new(source),
    }
}

pub(super) fn algorithm_aborted(error: AlgorithmAborted) -> ProcedureError {
    match error.cause {
        CancellationCause::Cancelled => ProcedureError::Cancelled,
        CancellationCause::Timeout { elapsed } => ProcedureError::Timeout { elapsed },
        CancellationCause::NodeScanBudgetExceeded { limit, scanned } => {
            ProcedureError::NodeScanBudgetExceeded { limit, scanned }
        }
    }
}

pub(super) fn topo_sort_error(error: TopoSortError) -> ProcedureError {
    match error {
        TopoSortError::NotADag { .. } => native_error(
            "algo.topological_sort: projection contains a directed cycle",
            error,
        ),
        TopoSortError::Aborted { source } => algorithm_aborted(source),
        other => native_error(other.to_string(), other),
    }
}

pub(super) fn pathfinding_error(
    procedure: &'static str,
    error: PathfindingError,
) -> ProcedureError {
    match error {
        PathfindingError::NegativeWeight { .. } => native_error(
            format!("{procedure}: traversed edge has negative weight"),
            error,
        ),
        PathfindingError::NaNWeight { .. } => {
            native_error(format!("{procedure}: traversed edge has NaN weight"), error)
        }
        PathfindingError::TooLarge { .. } => native_error(
            "algo.apsp: projection node count exceeds max_nodes limit",
            error,
        ),
        PathfindingError::Aborted { source } => algorithm_aborted(source),
        other => native_error(format!("{procedure}: pathfinding failed"), other),
    }
}
