//! `selene.vector_score_neighbors_batch` native built-in.
//!
//! Read-only graph-tier procedure that scores one labelled one-hop neighborhood
//! per query vector. `queries[i]` is scored against neighbors derived from
//! `anchors[i]`, and rows are grouped by `query_index`.

use selene_core::{Value, VectorMetric};
use selene_graph::{VectorNeighborDirection, VectorNeighborSearchOptions};

use super::meta::{StaticOutputColumn, StaticParameter};
use super::vector_common::{
    BatchMismatch, cardinality_arg, invalid_arg, live_node_list_arg, metric_arg,
    neighbor_direction_arg, queries_arg, query_index_too_large, string_arg, vector_search_error,
};
use crate::procedure_registry::ProcedureError;
use crate::{
    GqlType, GraphContext, ProcedureDefaultValue, ProcedureOutputColumn, ProcedureParameter,
    ProcedureResult,
};

const PROC_NAME: &str = "selene.vector_score_neighbors_batch";

static VECTOR_SCORE_NEIGHBOR_BATCH_OUTPUTS: [StaticOutputColumn; 3] = [
    StaticOutputColumn::new("query_index", GqlType::Uint64)
        .with_description("Zero-based query position."),
    StaticOutputColumn::new("node_id", GqlType::NodeRef)
        .with_description("Scored neighbor node id."),
    StaticOutputColumn::new("distance", GqlType::Float64)
        .with_description("Lower-is-better distance."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("queries", GqlType::List(Box::new(GqlType::Vector)), false)
            .with_description("Query vectors."),
        StaticParameter::new("anchors", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Anchor nodes whose graph neighbors form the candidate sets."),
        StaticParameter::new("edge_label", GqlType::String, false)
            .with_description("Edge label used to derive neighbors."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count per query."),
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
    VECTOR_SCORE_NEIGHBOR_BATCH_OUTPUTS
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
    let queries = queries_arg(PROC_NAME, &args[1])?;
    let anchors = live_node_list_arg(ctx.snapshot(), PROC_NAME, &args[2], "anchors")?;
    if queries.len() != anchors.len() {
        return Err(invalid_arg(format!(
            "{PROC_NAME} queries and anchors must have the same length"
        )));
    }
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

    let batch_hits = ctx
        .snapshot()
        .score_vector_neighbors_batch_checked(
            &property,
            &queries,
            &anchors,
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

    let mut rows = Vec::with_capacity(batch_hits.iter().map(Vec::len).sum());
    for (query_index, hits) in batch_hits.into_iter().enumerate() {
        let query_index =
            u64::try_from(query_index).map_err(|err| query_index_too_large(PROC_NAME, err))?;
        for hit in hits {
            rows.push(vec![
                Value::Uint(query_index),
                Value::NodeRef(hit.node_id),
                Value::Float(hit.distance),
            ]);
        }
    }
    Ok(ProcedureResult { rows })
}
