//! `selene.text_search_nodes` and candidate text-scoring native built-ins.
//!
//! Read-only graph-tier procedure exposing BM25 full-text search over
//! string-valued node properties. A registered maintained text index is used
//! when present; otherwise global search falls back to the exact scan oracle.
//! Candidate-scoped scoring requires a registered text index so a read call does
//! not unexpectedly build a full transient postings index. These are
//! intentionally `CALL selene.*` surfaces rather than grammar syntax: full-text
//! search is an implementation-defined engine capability layered on ISO GQL
//! values.

use selene_core::{DbString, Value};
mod support;
use support::{query_arg, query_list_arg, score_nodes, state_candidates, text_search_error};

use super::meta::{StaticOutputColumn, StaticParameter};
use super::retrieval_filter::{
    append_edge_filter_parameters, append_node_filter_parameters, optional_filter_candidates,
};
use super::vector_candidate_state_common::{
    CandidateStateOperation, candidate_state_error, operation_arg,
};
use super::vector_common::{
    BatchMismatch, candidate_set_arg, candidate_sets_arg, cardinality_arg, expansion_direction_arg,
    invalid_arg, live_node_list_arg, live_node_list_sets_arg, query_index_too_large, string_arg,
    vector_search_error,
};
use crate::procedure_registry::ProcedureError;
use crate::{GqlType, GraphContext, ProcedureOutputColumn, ProcedureParameter, ProcedureResult};

const PROC_NAME: &str = "selene.text_search_nodes";
const SCORE_PROC_NAME: &str = "selene.text_score_nodes";
const SCORE_BATCH_PROC_NAME: &str = "selene.text_score_nodes_batch";
const SCORE_STATE_PROC_NAME: &str = "selene.text_score_candidate_state";
const SCORE_STATE_NODES_PROC_NAME: &str = "selene.text_score_candidate_state_nodes";
const SCORE_STATE_EXPANDED_BATCH_PROC_NAME: &str =
    "selene.text_score_candidate_state_expanded_batch";

static TEXT_SEARCH_PARAMS: [StaticParameter; 4] = [
    StaticParameter::new("label", GqlType::String, false).with_description("Node label."),
    StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
    StaticParameter::new("query", GqlType::String, false)
        .with_description("Full-text query string."),
    StaticParameter::new("k", GqlType::Integer, false).with_description("Maximum result count."),
];

static TEXT_SEARCH_OUTPUTS: [StaticOutputColumn; 2] = [
    StaticOutputColumn::new("node_id", GqlType::NodeRef).with_description("Matched node id."),
    StaticOutputColumn::new("score", GqlType::Float64)
        .with_description("Higher-is-better BM25 score."),
];
static TEXT_SCORE_BATCH_OUTPUTS: [StaticOutputColumn; 3] = [
    StaticOutputColumn::new("query_index", GqlType::Uint64)
        .with_description("Zero-based query position."),
    StaticOutputColumn::new("node_id", GqlType::NodeRef).with_description("Matched node id."),
    StaticOutputColumn::new("score", GqlType::Float64)
        .with_description("Higher-is-better BM25 score."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    let mut params: Vec<_> = TEXT_SEARCH_PARAMS
        .iter()
        .cloned()
        .map(StaticParameter::into_parameter)
        .collect();
    append_node_filter_parameters(&mut params);
    append_edge_filter_parameters(&mut params);
    params
}

pub(super) fn score_signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("label", GqlType::String, false).with_description("Node label."),
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("query", GqlType::String, false)
            .with_description("Full-text query string."),
        StaticParameter::new("nodes", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Candidate nodes to score."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn score_batch_signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("label", GqlType::String, false).with_description("Node label."),
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("queries", GqlType::List(Box::new(GqlType::String)), false)
            .with_description("Full-text query strings."),
        StaticParameter::new(
            "nodes",
            GqlType::List(Box::new(GqlType::List(Box::new(GqlType::NodeRef)))),
            false,
        )
        .with_description("Per-query candidate nodes to score."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count per query."),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn score_state_signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("label", GqlType::String, false).with_description("Node label."),
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("query", GqlType::String, false)
            .with_description("Full-text query string."),
        StaticParameter::new("state_name", GqlType::String, false)
            .with_description("Maintained candidate-state name."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn score_state_nodes_signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("label", GqlType::String, false).with_description("Node label."),
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("query", GqlType::String, false)
            .with_description("Full-text query string."),
        StaticParameter::new("state_name", GqlType::String, false)
            .with_description("Maintained candidate-state name."),
        StaticParameter::new("nodes", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Explicit candidate nodes to compose with the maintained state."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
        StaticParameter::new("operation", GqlType::String, false)
            .with_description("Candidate-set algebra operation.")
            .with_default_doc("intersection")
            .with_default(crate::ProcedureDefaultValue::String("intersection")),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn score_state_expanded_batch_signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("label", GqlType::String, false).with_description("Node label."),
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("queries", GqlType::List(Box::new(GqlType::String)), false)
            .with_description("Full-text query strings."),
        StaticParameter::new("state_name", GqlType::String, false)
            .with_description("Maintained candidate-state name."),
        StaticParameter::new(
            "roots",
            GqlType::List(Box::new(GqlType::List(Box::new(GqlType::NodeRef)))),
            false,
        )
        .with_description("Per-query root candidate nodes to preserve and graph-expand."),
        StaticParameter::new("edge_label", GqlType::String, false)
            .with_description("Edge label used to expand root candidates."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count per query."),
        StaticParameter::new("operation", GqlType::String, false)
            .with_description("Candidate-set algebra operation.")
            .with_default_doc("intersection")
            .with_default(crate::ProcedureDefaultValue::String("intersection")),
        StaticParameter::new("direction", GqlType::String, false)
            .with_description("Expansion direction: outgoing, incoming, or both.")
            .with_default_doc("outgoing")
            .with_default(crate::ProcedureDefaultValue::String("outgoing")),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn output_columns() -> Vec<ProcedureOutputColumn> {
    TEXT_SEARCH_OUTPUTS
        .iter()
        .cloned()
        .map(StaticOutputColumn::into_output_column)
        .collect()
}

pub(super) fn score_batch_output_columns() -> Vec<ProcedureOutputColumn> {
    TEXT_SCORE_BATCH_OUTPUTS
        .iter()
        .cloned()
        .map(StaticOutputColumn::into_output_column)
        .collect()
}

pub(super) fn execute(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if !matches!(args.len(), 4 | 6 | 10) {
        return Err(invalid_arg(format!(
            "{PROC_NAME} expects 4, 6, or 10 arguments"
        )));
    }

    let label = string_arg(PROC_NAME, &args[0], "label")?;
    let property = string_arg(PROC_NAME, &args[1], "property")?;
    let query = query_arg(PROC_NAME, &args[2])?;
    let k = cardinality_arg(PROC_NAME, &args[3], "k")?;

    let snapshot = ctx.snapshot();
    let filter_candidates = if args.len() >= 6 {
        let edge_filter = if args.len() == 10 {
            Some((&args[6], &args[7], &args[8], &args[9]))
        } else {
            None
        };
        optional_filter_candidates(PROC_NAME, snapshot, &label, &args[4], &args[5], edge_filter)?
    } else {
        None
    };
    let hits = match snapshot.text_index_for(&label, &property) {
        Some(index) => {
            if let Some(candidates) = &filter_candidates {
                let nodes: Vec<_> = candidates.iter().collect();
                score_nodes(ctx, &index, query, &nodes, k)?
            } else {
                index
                    .search_checked(query, k, ctx.cancellation_checker())
                    .map_err(text_search_error)?
            }
        }
        None => {
            if let Some(candidates) = &filter_candidates {
                snapshot
                    .exact_text_search_nodes_in_candidates_checked(
                        &label,
                        &property,
                        query,
                        k,
                        candidates,
                        ctx.cancellation_checker(),
                    )
                    .map_err(text_search_error)?
            } else {
                snapshot
                    .exact_text_search_nodes_checked(
                        &label,
                        &property,
                        query,
                        k,
                        ctx.cancellation_checker(),
                    )
                    .map_err(text_search_error)?
            }
        }
    };
    Ok(ProcedureResult {
        rows: hits
            .into_iter()
            .map(|hit| vec![Value::NodeRef(hit.node_id), Value::Float(hit.score)])
            .collect(),
    })
}

pub(super) fn execute_score(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if args.len() != 5 {
        return Err(invalid_arg(format!(
            "{SCORE_PROC_NAME} expects 5 arguments"
        )));
    }

    let label = string_arg(SCORE_PROC_NAME, &args[0], "label")?;
    let property = string_arg(SCORE_PROC_NAME, &args[1], "property")?;
    let query = query_arg(SCORE_PROC_NAME, &args[2])?;
    let nodes = live_node_list_arg(ctx.snapshot(), SCORE_PROC_NAME, &args[3], "nodes")?;
    let k = cardinality_arg(SCORE_PROC_NAME, &args[4], "k")?;

    let snapshot = ctx.snapshot();
    let Some(index) = snapshot.text_index_for(&label, &property) else {
        return Err(invalid_arg(format!(
            "{SCORE_PROC_NAME} requires a text index for {}.{}; call selene.create_text_index first",
            label.as_str(),
            property.as_str()
        )));
    };
    let hits = score_nodes(ctx, &index, query, &nodes, k)?;

    Ok(ProcedureResult {
        rows: hits
            .into_iter()
            .map(|hit| vec![Value::NodeRef(hit.node_id), Value::Float(hit.score)])
            .collect(),
    })
}

pub(super) fn execute_score_batch(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if args.len() != 5 {
        return Err(invalid_arg(format!(
            "{SCORE_BATCH_PROC_NAME} expects 5 arguments"
        )));
    }

    let label = string_arg(SCORE_BATCH_PROC_NAME, &args[0], "label")?;
    let property = string_arg(SCORE_BATCH_PROC_NAME, &args[1], "property")?;
    let queries = query_list_arg(SCORE_BATCH_PROC_NAME, &args[2])?;
    let node_sets =
        live_node_list_sets_arg(ctx.snapshot(), SCORE_BATCH_PROC_NAME, &args[3], "nodes")?;
    if queries.len() != node_sets.len() {
        return Err(invalid_arg(format!(
            "{SCORE_BATCH_PROC_NAME} queries and nodes must have the same length"
        )));
    }
    let k = cardinality_arg(SCORE_BATCH_PROC_NAME, &args[4], "k")?;

    let snapshot = ctx.snapshot();
    let Some(index) = snapshot.text_index_for(&label, &property) else {
        return Err(invalid_arg(format!(
            "{SCORE_BATCH_PROC_NAME} requires a text index for {}.{}; call selene.create_text_index first",
            label.as_str(),
            property.as_str()
        )));
    };

    let mut rows = Vec::new();
    for (query_index, (query, nodes)) in queries.iter().zip(node_sets.iter()).enumerate() {
        let query_index = u64::try_from(query_index)
            .map_err(|err| query_index_too_large(SCORE_BATCH_PROC_NAME, err))?;
        let hits = score_nodes(ctx, &index, query.as_str(), nodes, k)?;
        rows.reserve(hits.len());
        for hit in hits {
            rows.push(vec![
                Value::Uint(query_index),
                Value::NodeRef(hit.node_id),
                Value::Float(hit.score),
            ]);
        }
    }
    Ok(ProcedureResult { rows })
}

pub(super) fn execute_score_state(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if args.len() != 5 {
        return Err(invalid_arg(format!(
            "{SCORE_STATE_PROC_NAME} expects 5 arguments"
        )));
    }

    let label = string_arg(SCORE_STATE_PROC_NAME, &args[0], "label")?;
    let property = string_arg(SCORE_STATE_PROC_NAME, &args[1], "property")?;
    let query = query_arg(SCORE_STATE_PROC_NAME, &args[2])?;
    let state_name = string_arg(SCORE_STATE_PROC_NAME, &args[3], "state_name")?;
    let k = cardinality_arg(SCORE_STATE_PROC_NAME, &args[4], "k")?;

    let candidates = state_candidates(ctx, &state_name)
        .map_err(|error| candidate_state_error(SCORE_STATE_PROC_NAME, error))?
        .ok_or_else(|| {
            invalid_arg(format!(
                "{SCORE_STATE_PROC_NAME} unknown maintained candidate-state set '{}'",
                state_name.as_str()
            ))
        })?;
    let index = text_index_for_score(ctx, SCORE_STATE_PROC_NAME, &label, &property)?;
    let hits = score_nodes(ctx, &index, query, candidates.as_nodes(), k)?;

    Ok(ProcedureResult {
        rows: hits
            .into_iter()
            .map(|hit| vec![Value::NodeRef(hit.node_id), Value::Float(hit.score)])
            .collect(),
    })
}

pub(super) fn execute_score_state_nodes(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if !(6..=7).contains(&args.len()) {
        return Err(invalid_arg(format!(
            "{SCORE_STATE_NODES_PROC_NAME} expects 6 or 7 arguments"
        )));
    }

    let label = string_arg(SCORE_STATE_NODES_PROC_NAME, &args[0], "label")?;
    let property = string_arg(SCORE_STATE_NODES_PROC_NAME, &args[1], "property")?;
    let query = query_arg(SCORE_STATE_NODES_PROC_NAME, &args[2])?;
    let state_name = string_arg(SCORE_STATE_NODES_PROC_NAME, &args[3], "state_name")?;
    let nodes = candidate_set_arg(
        ctx.snapshot(),
        SCORE_STATE_NODES_PROC_NAME,
        &args[4],
        "nodes",
    )?;
    let k = cardinality_arg(SCORE_STATE_NODES_PROC_NAME, &args[5], "k")?;
    let operation = args
        .get(6)
        .map(|arg| operation_arg(SCORE_STATE_NODES_PROC_NAME, arg))
        .transpose()?
        .unwrap_or(CandidateStateOperation::Intersection);

    let state = state_candidates(ctx, &state_name)
        .map_err(|error| candidate_state_error(SCORE_STATE_NODES_PROC_NAME, error))?
        .ok_or_else(|| {
            invalid_arg(format!(
                "{SCORE_STATE_NODES_PROC_NAME} unknown maintained candidate-state set '{}'",
                state_name.as_str()
            ))
        })?;
    let candidates = operation.compose(&state, &nodes);
    let index = text_index_for_score(ctx, SCORE_STATE_NODES_PROC_NAME, &label, &property)?;
    let hits = score_nodes(ctx, &index, query, candidates.as_nodes(), k)?;

    Ok(ProcedureResult {
        rows: hits
            .into_iter()
            .map(|hit| vec![Value::NodeRef(hit.node_id), Value::Float(hit.score)])
            .collect(),
    })
}

pub(super) fn execute_score_state_expanded_batch(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if !(7..=9).contains(&args.len()) {
        return Err(invalid_arg(format!(
            "{SCORE_STATE_EXPANDED_BATCH_PROC_NAME} expects 7 to 9 arguments"
        )));
    }

    let label = string_arg(SCORE_STATE_EXPANDED_BATCH_PROC_NAME, &args[0], "label")?;
    let property = string_arg(SCORE_STATE_EXPANDED_BATCH_PROC_NAME, &args[1], "property")?;
    let queries = query_list_arg(SCORE_STATE_EXPANDED_BATCH_PROC_NAME, &args[2])?;
    let state_name = string_arg(SCORE_STATE_EXPANDED_BATCH_PROC_NAME, &args[3], "state_name")?;
    let root_sets = candidate_sets_arg(
        ctx.snapshot(),
        SCORE_STATE_EXPANDED_BATCH_PROC_NAME,
        &args[4],
        "roots",
    )?;
    if queries.len() != root_sets.len() {
        return Err(invalid_arg(format!(
            "{SCORE_STATE_EXPANDED_BATCH_PROC_NAME} queries and roots must have the same length"
        )));
    }
    let edge_label = string_arg(SCORE_STATE_EXPANDED_BATCH_PROC_NAME, &args[5], "edge_label")?;
    let k = cardinality_arg(SCORE_STATE_EXPANDED_BATCH_PROC_NAME, &args[6], "k")?;
    let operation = args
        .get(7)
        .map(|arg| operation_arg(SCORE_STATE_EXPANDED_BATCH_PROC_NAME, arg))
        .transpose()?
        .unwrap_or(CandidateStateOperation::Intersection);
    let direction = args
        .get(8)
        .map(|arg| expansion_direction_arg(SCORE_STATE_EXPANDED_BATCH_PROC_NAME, arg))
        .transpose()?
        .unwrap_or(selene_graph::VectorNeighborDirection::Outgoing);

    let state = state_candidates(ctx, &state_name)
        .map_err(|error| candidate_state_error(SCORE_STATE_EXPANDED_BATCH_PROC_NAME, error))?
        .ok_or_else(|| {
            invalid_arg(format!(
                "{SCORE_STATE_EXPANDED_BATCH_PROC_NAME} unknown maintained candidate-state set '{}'",
                state_name.as_str()
            ))
        })?;

    let snapshot = ctx.snapshot();
    let Some(index) = snapshot.text_index_for(&label, &property) else {
        return Err(invalid_arg(format!(
            "{SCORE_STATE_EXPANDED_BATCH_PROC_NAME} requires a text index for {}.{}; call selene.create_text_index first",
            label.as_str(),
            property.as_str()
        )));
    };

    let expanded_sets = snapshot
        .expand_vector_candidate_sets_batch_checked(
            &root_sets,
            &edge_label,
            direction,
            k,
            ctx.cancellation_checker(),
        )
        .map_err(|error| {
            vector_search_error(
                error,
                "batched maintained candidate-state expanded BM25 scoring",
                BatchMismatch::Internal(
                    "batched maintained candidate-state text expansion received batched-only error",
                ),
                "batched maintained candidate-state expanded BM25 scoring",
            )
        })?;

    let mut rows = Vec::new();
    for (query_index, (query, expanded)) in queries.iter().zip(expanded_sets.iter()).enumerate() {
        let query_index = u64::try_from(query_index)
            .map_err(|err| query_index_too_large(SCORE_STATE_EXPANDED_BATCH_PROC_NAME, err))?;
        let candidates = operation.compose(&state, expanded);
        let hits = score_nodes(ctx, &index, query.as_str(), candidates.as_nodes(), k)?;
        rows.reserve(hits.len());
        for hit in hits {
            rows.push(vec![
                Value::Uint(query_index),
                Value::NodeRef(hit.node_id),
                Value::Float(hit.score),
            ]);
        }
    }
    Ok(ProcedureResult { rows })
}

fn text_index_for_score(
    ctx: &GraphContext<'_>,
    proc_name: &'static str,
    label: &DbString,
    property: &DbString,
) -> Result<std::sync::Arc<selene_graph::TextIndex>, ProcedureError> {
    ctx.snapshot()
        .text_index_for(label, property)
        .ok_or_else(|| {
            invalid_arg(format!(
                "{proc_name} requires a text index for {}.{}; call selene.create_text_index first",
                label.as_str(),
                property.as_str()
            ))
        })
}
