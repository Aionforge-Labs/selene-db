//! `selene.vector_score_neighbors` native built-in.
//!
//! Read-only graph-tier procedure that derives candidates from one anchor's
//! labelled one-hop neighborhood, then scores those neighbor nodes by a vector
//! property. This keeps the engine primitive generic: callers choose the graph
//! edge semantics, while vector scoring remains native.

use selene_core::{Value, VectorMetric};
use selene_graph::{VectorNeighborDirection, VectorNeighborSearchOptions};

use super::meta::{StaticOutputColumn, StaticParameter};
use super::vector_common::{
    BatchMismatch, cardinality_arg, invalid_arg, metric_arg, neighbor_direction_arg, node_arg,
    query_arg, string_arg, vector_search_error,
};
use crate::procedure_registry::ProcedureError;
use crate::{
    GqlType, GraphContext, ProcedureDefaultValue, ProcedureOutputColumn, ProcedureParameter,
    ProcedureResult,
};

const PROC_NAME: &str = "selene.vector_score_neighbors";

static VECTOR_SCORE_NEIGHBOR_OUTPUTS: [StaticOutputColumn; 2] = [
    StaticOutputColumn::new("node_id", GqlType::NodeRef)
        .with_description("Scored neighbor node id."),
    StaticOutputColumn::new("distance", GqlType::Float64)
        .with_description("Lower-is-better distance."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("query", GqlType::Vector, false).with_description("Query vector."),
        StaticParameter::new("anchor", GqlType::NodeRef, false)
            .with_description("Anchor node whose graph neighbors form the candidate set."),
        StaticParameter::new("edge_label", GqlType::String, false)
            .with_description("Edge label used to derive neighbors."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
        StaticParameter::new("direction", GqlType::String, false)
            .with_description("Neighbor direction: outgoing, incoming, or both.")
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
    VECTOR_SCORE_NEIGHBOR_OUTPUTS
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
    let anchor = node_arg(ctx.snapshot(), PROC_NAME, &args[2], "anchor")?;
    let edge_label = string_arg(PROC_NAME, &args[3], "edge_label")?;
    let k = cardinality_arg(PROC_NAME, &args[4], "k")?;
    let direction = args
        .get(5)
        .map(|arg| neighbor_direction_arg(PROC_NAME, arg))
        .transpose()?
        .unwrap_or(VectorNeighborDirection::Outgoing);
    let metric = args
        .get(6)
        .map(|arg| metric_arg(PROC_NAME, arg))
        .transpose()?
        .unwrap_or(VectorMetric::SquaredEuclidean);
    let options = VectorNeighborSearchOptions::new(&edge_label, direction, metric, k);

    let hits = ctx
        .snapshot()
        .score_vector_neighbors_checked(
            &property,
            &query,
            anchor,
            options,
            ctx.cancellation_checker(),
        )
        .map_err(|error| {
            vector_search_error(
                error,
                "vector neighbor scoring",
                BatchMismatch::Internal("vector neighbor scoring received batch-shape error"),
                "vector neighbor scoring",
            )
        })?;

    let rows = hits
        .into_iter()
        .map(|hit| vec![Value::NodeRef(hit.node_id), Value::Float(hit.distance)])
        .collect();
    Ok(ProcedureResult { rows })
}
