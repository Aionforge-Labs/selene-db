//! Error surface for `selene-algorithms`.
//!
//! Per spec 16 §3 E04, weight extraction is infallible — there are no
//! `WeightProperty*` variants. The starter enum carries only consumer-facing
//! variants algorithms raise to signal "your input is not algorithm-meaningful."
//!
//! `#[non_exhaustive]` lets BRIEFs 52–55 (structural / pathfinding / centrality
//! / community) extend with algorithm-specific variants without breaking the
//! crate's public API.

use selene_core::{CancellationCause, CancellationChecker, NodeId};
use thiserror::Error;

/// Cancellation check stride for algorithm hot loops.
pub(crate) const ALGORITHM_CANCEL_CHECK_STRIDE: usize = 1024;

/// Cooperative cancellation outcome for long-running algorithms.
#[derive(Clone, Copy, Debug, Error)]
#[error("algorithm aborted: {cause:?}")]
pub struct AlgorithmAborted {
    /// Cause reported by the shared cancellation checker.
    pub cause: CancellationCause,
}

impl From<CancellationCause> for AlgorithmAborted {
    #[inline]
    fn from(cause: CancellationCause) -> Self {
        Self { cause }
    }
}

#[inline(always)]
pub(crate) fn check_algorithm(checker: CancellationChecker<'_>) -> Result<(), AlgorithmAborted> {
    if checker.is_disabled() {
        return Ok(());
    }
    checker.check().map_err(AlgorithmAborted::from)
}

#[inline(always)]
pub(crate) fn check_algorithm_stride(
    checker: CancellationChecker<'_>,
    units_since_check: &mut usize,
) -> Result<(), AlgorithmAborted> {
    if checker.is_disabled() {
        return Ok(());
    }
    *units_since_check = units_since_check.saturating_add(1);
    if *units_since_check >= ALGORITHM_CANCEL_CHECK_STRIDE {
        check_algorithm(checker)?;
        *units_since_check = 0;
    }
    Ok(())
}

/// Errors raised by `selene-algorithms` surfaces.
///
/// `GraphProjection::build` cannot raise these — they exist for algorithm
/// surfaces (BRIEFs 52–55) that need to refuse meaningless inputs (e.g.,
/// PageRank on a projection with zero nodes, or a path query against a node
/// the projection does not contain).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AlgorithmsError {
    /// Algorithm refuses an empty projection because the result is undefined or
    /// useless for the caller (e.g., centrality scores over zero nodes).
    ///
    /// `reason` describes why the algorithm requires a non-empty input.
    #[error("empty projection: {reason}")]
    EmptyProjection {
        /// Why the algorithm requires a non-empty projection.
        reason: &'static str,
    },

    /// Algorithm received a `NodeId` that is not part of the projection
    /// (e.g., source node for single-source pathfinding not in scope).
    #[error("node {id:?} is not part of this projection")]
    NodeNotInProjection {
        /// The offending node ID.
        id: NodeId,
    },

    /// `ProjectionCatalog::ensure_fresh` (or another catalog operation that
    /// requires the named projection to already exist) was called with a name
    /// that is not registered. The catalog is a caching surface, not a
    /// projection factory; callers register projections explicitly via
    /// `project()` before referencing them.
    #[error("no projection registered for name {name:?}")]
    NoSuchProjection {
        /// The unrecognized projection name.
        name: String,
    },

    /// Underlying graph error.
    #[error("graph error: {0}")]
    Graph(#[from] selene_graph::GraphError),

    /// Candidate set validation error.
    #[error("candidate set error: {0}")]
    CandidateSet(#[from] selene_graph::CandidateSetError),
}
