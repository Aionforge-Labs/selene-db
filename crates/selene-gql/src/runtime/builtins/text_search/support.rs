//! Text argument diagnostics and the single snapshot-bound scoring entry point.

use super::invalid_arg;
use crate::{GraphContext, ProcedureError};
use selene_core::{DbString, NodeId, Value};
use selene_graph::{GraphError, TextIndex, TextSearchError, TextSearchHit};

pub(super) fn state_candidates(
    ctx: &GraphContext<'_>,
    name: &DbString,
) -> Result<Option<selene_graph::VectorCandidateSet>, selene_graph::ProviderError> {
    ctx.node_candidate_set(name)
        .map(|set| set.map(|set| selene_graph::VectorCandidateSet::from_nodes(set.iter())))
}

pub(super) fn score_nodes(
    ctx: &GraphContext<'_>,
    index: &TextIndex,
    query: &str,
    nodes: &[NodeId],
    k: usize,
) -> Result<Vec<TextSearchHit>, ProcedureError> {
    let candidates = ctx
        .snapshot()
        .bind_node_candidates(nodes.iter().copied())
        .map_err(|e| text_search_error(e.into()))?;
    ctx.snapshot()
        .score_text_candidates_checked(
            index.label(),
            index.property(),
            query,
            &candidates,
            k,
            ctx.cancellation_checker(),
        )
        .map_err(text_search_error)
}

pub(super) fn query_arg<'a>(
    proc_name: &'static str,
    value: &'a Value,
) -> Result<&'a str, ProcedureError> {
    let Value::String(value) = value else {
        return Err(invalid_arg(format!("{proc_name} query must be a STRING")));
    };
    Ok(value.as_str())
}

pub(super) fn query_list_arg(
    proc_name: &'static str,
    value: &Value,
) -> Result<Vec<DbString>, ProcedureError> {
    let Value::List(values) = value else {
        return Err(invalid_arg(format!(
            "{proc_name} queries must be a LIST<STRING>"
        )));
    };
    let mut queries = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let Value::String(query) = value else {
            return Err(invalid_arg(format!(
                "{proc_name} queries[{index}] must be a STRING"
            )));
        };
        queries.push(query.clone());
    }
    Ok(queries)
}

pub(super) fn text_search_error(error: TextSearchError) -> ProcedureError {
    match error {
        TextSearchError::Cancelled => ProcedureError::Cancelled,
        TextSearchError::Timeout { elapsed } => ProcedureError::Timeout { elapsed },
        TextSearchError::NodeScanBudgetExceeded { limit, scanned } => {
            ProcedureError::NodeScanBudgetExceeded { limit, scanned }
        }
        TextSearchError::Graph(GraphError::Inconsistent { reason }) => ProcedureError::Internal {
            detail: format!("graph inconsistency during text search: {reason}"),
        },
        TextSearchError::Graph(other) => ProcedureError::Internal {
            detail: format!("unexpected graph error during text search: {other}"),
        },
    }
}
