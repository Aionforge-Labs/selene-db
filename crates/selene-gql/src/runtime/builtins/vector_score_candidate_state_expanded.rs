//! `selene.vector_score_candidate_state_expanded` native built-in.
//!
//! Read-only graph-tier procedure that expands root candidates through one
//! labelled graph hop, composes that expanded set with a named maintained
//! candidate-state set, then exact-reranks the composed set by a vector-valued
//! node property. This keeps graph-derived retrieval roots and maintained
//! currentness filters under one statement snapshot.

use selene_core::{Value, VectorMetric};
use selene_graph::VectorNeighborDirection;

use super::meta::{StaticOutputColumn, StaticParameter};
use super::vector_candidate_state_common::{
    CandidateStateOperation, candidate_state_error, operation_arg,
};
use super::vector_common::{
    BatchMismatch, candidate_set_arg, cardinality_arg, expansion_direction_arg, invalid_arg,
    metric_arg, query_arg, string_arg, vector_search_error,
};
use crate::procedure_registry::ProcedureError;
use crate::{
    GqlType, GraphContext, ProcedureDefaultValue, ProcedureOutputColumn, ProcedureParameter,
    ProcedureResult,
};

const PROC_NAME: &str = "selene.vector_score_candidate_state_expanded";

static VECTOR_SCORE_OUTPUTS: [StaticOutputColumn; 2] = [
    StaticOutputColumn::new("node_id", GqlType::NodeRef)
        .with_description("Scored composed-candidate node id."),
    StaticOutputColumn::new("distance", GqlType::Float64)
        .with_description("Lower-is-better distance."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("query", GqlType::Vector, false).with_description("Query vector."),
        StaticParameter::new("state_name", GqlType::String, false)
            .with_description("Maintained candidate-state name."),
        StaticParameter::new("roots", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Root candidate nodes to preserve and graph-expand."),
        StaticParameter::new("edge_label", GqlType::String, false)
            .with_description("Edge label used to expand root candidates."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
        StaticParameter::new("operation", GqlType::String, false)
            .with_description("Candidate-set algebra operation.")
            .with_default_doc("intersection")
            .with_default(ProcedureDefaultValue::String("intersection")),
        StaticParameter::new("direction", GqlType::String, false)
            .with_description("Expansion direction: outgoing, incoming, or both.")
            .with_default_doc("outgoing")
            .with_default(ProcedureDefaultValue::String("outgoing")),
        StaticParameter::new("metric", GqlType::String, false)
            .with_description("Distance metric.")
            .with_default_doc("squared_euclidean")
            .with_default(ProcedureDefaultValue::String("squared_euclidean")),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn output_columns() -> Vec<ProcedureOutputColumn> {
    VECTOR_SCORE_OUTPUTS
        .iter()
        .cloned()
        .map(StaticOutputColumn::into_output_column)
        .collect()
}

pub(super) fn execute(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if !(6..=9).contains(&args.len()) {
        return Err(invalid_arg(format!("{PROC_NAME} expects 6 to 9 arguments")));
    }

    let property = string_arg(PROC_NAME, &args[0], "property")?;
    let query = query_arg(PROC_NAME, &args[1])?;
    let state_name = string_arg(PROC_NAME, &args[2], "state_name")?;
    let roots = candidate_set_arg(ctx.snapshot(), PROC_NAME, &args[3], "roots")?;
    let edge_label = string_arg(PROC_NAME, &args[4], "edge_label")?;
    let k = cardinality_arg(PROC_NAME, &args[5], "k")?;
    let operation = args
        .get(6)
        .map(|arg| operation_arg(PROC_NAME, arg))
        .transpose()?
        .unwrap_or(CandidateStateOperation::Intersection);
    let direction = args
        .get(7)
        .map(|arg| expansion_direction_arg(PROC_NAME, arg))
        .transpose()?
        .unwrap_or(VectorNeighborDirection::Outgoing);
    let metric = args
        .get(8)
        .map(|arg| metric_arg(PROC_NAME, arg))
        .transpose()?
        .unwrap_or(VectorMetric::SquaredEuclidean);

    let state = ctx
        .vector_candidate_set(&state_name)
        .map_err(|error| candidate_state_error(PROC_NAME, error))?
        .ok_or_else(|| {
            invalid_arg(format!(
                "{PROC_NAME} unknown maintained candidate-state set '{}'",
                state_name.as_str()
            ))
        })?;
    let expanded = ctx
        .snapshot()
        .expand_vector_candidate_set_checked(
            &roots,
            &edge_label,
            direction,
            ctx.cancellation_checker(),
        )
        .map_err(|error| {
            vector_search_error(
                error,
                "maintained candidate-state expanded vector scoring",
                BatchMismatch::Internal(
                    "maintained candidate-state expanded scoring received batched-only error",
                ),
                "maintained candidate-state expanded vector scoring",
            )
        })?;
    let candidates = operation.compose(&state, &expanded);

    let hits = ctx
        .snapshot()
        .score_vector_candidate_set_checked(
            &property,
            &query,
            &candidates,
            metric,
            k,
            ctx.cancellation_checker(),
        )
        .map_err(|error| {
            vector_search_error(
                error,
                "maintained candidate-state expanded vector scoring",
                BatchMismatch::Internal(
                    "maintained candidate-state expanded scoring received batched-only error",
                ),
                "maintained candidate-state expanded vector scoring",
            )
        })?;

    let rows = hits
        .into_iter()
        .map(|hit| vec![Value::NodeRef(hit.node_id), Value::Float(hit.distance)])
        .collect();
    Ok(ProcedureResult { rows })
}
