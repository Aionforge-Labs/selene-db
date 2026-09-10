//! `selene.vector_score_expanded_candidates` native built-in.
//!
//! Read-only graph-tier procedure that treats an explicit `LIST<NODE>` as root
//! candidates, expands that set through one labelled graph hop, and scores the
//! canonical expanded set by a vector-valued node property. This is the native
//! GQL entry point for graph-augmented vector reranking without adding
//! non-standard syntax.

use selene_core::{Value, VectorMetric};
use selene_graph::VectorNeighborDirection;

use super::meta::{StaticOutputColumn, StaticParameter};
use super::vector_common::{
    BatchMismatch, candidate_set_arg, cardinality_arg, expansion_direction_arg, invalid_arg,
    metric_arg, query_arg, string_arg, vector_search_error,
};
use crate::procedure_registry::ProcedureError;
use crate::{
    GqlType, GraphContext, ProcedureDefaultValue, ProcedureOutputColumn, ProcedureParameter,
    ProcedureResult,
};

const PROC_NAME: &str = "selene.vector_score_expanded_candidates";

static VECTOR_SCORE_EXPANDED_OUTPUTS: [StaticOutputColumn; 2] = [
    StaticOutputColumn::new("node_id", GqlType::NodeRef)
        .with_description("Scored expanded-candidate node id."),
    StaticOutputColumn::new("distance", GqlType::Float64)
        .with_description("Lower-is-better distance."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("query", GqlType::Vector, false).with_description("Query vector."),
        StaticParameter::new("roots", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Root candidate nodes to preserve and graph-expand."),
        StaticParameter::new("edge_label", GqlType::String, false)
            .with_description("Edge label used to expand root candidates."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
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
    VECTOR_SCORE_EXPANDED_OUTPUTS
        .iter()
        .cloned()
        .map(StaticOutputColumn::into_output_column)
        .collect()
}

pub(super) fn execute(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if !(5..=7).contains(&args.len()) {
        return Err(invalid_arg(format!(
            "{PROC_NAME} expects 5, 6, or 7 arguments"
        )));
    }

    let property = string_arg(PROC_NAME, &args[0], "property")?;
    let query = query_arg(PROC_NAME, &args[1])?;
    let roots = candidate_set_arg(ctx.snapshot(), PROC_NAME, &args[2], "roots")?;
    let edge_label = string_arg(PROC_NAME, &args[3], "edge_label")?;
    let k = cardinality_arg(PROC_NAME, &args[4], "k")?;
    let direction = args
        .get(5)
        .map(|arg| expansion_direction_arg(PROC_NAME, arg))
        .transpose()?
        .unwrap_or(VectorNeighborDirection::Outgoing);
    let metric = args
        .get(6)
        .map(|arg| metric_arg(PROC_NAME, arg))
        .transpose()?
        .unwrap_or(VectorMetric::SquaredEuclidean);

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
                "expanded vector candidate scoring",
                BatchMismatch::Internal(
                    "expanded vector candidate scoring received batched-only error",
                ),
                "expanded vector candidate scoring",
            )
        })?;
    let hits = ctx
        .snapshot()
        .score_vector_candidate_set_checked(
            &property,
            &query,
            &expanded,
            metric,
            k,
            ctx.cancellation_checker(),
        )
        .map_err(|error| {
            vector_search_error(
                error,
                "expanded vector candidate scoring",
                BatchMismatch::Internal(
                    "expanded vector candidate scoring received batched-only error",
                ),
                "expanded vector candidate scoring",
            )
        })?;

    let rows = hits
        .into_iter()
        .map(|hit| vec![Value::NodeRef(hit.node_id), Value::Float(hit.distance)])
        .collect();
    Ok(ProcedureResult { rows })
}
