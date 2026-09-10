//! `selene.vector_score_expanded_candidates_batch` native built-in.
//!
//! Read-only graph-tier procedure that expands one root candidate set per
//! query vector through a labelled one-hop graph neighborhood, then reranks
//! each canonical expanded set by a vector-valued node property.

use selene_core::{Value, VectorMetric};
use selene_graph::{VectorNeighborDirection, VectorNeighborSearchOptions};

use super::meta::{StaticOutputColumn, StaticParameter};
use super::vector_common::{
    BatchMismatch, candidate_sets_arg, cardinality_arg, expansion_direction_arg, invalid_arg,
    metric_arg, queries_arg, query_index_too_large, string_arg, vector_search_error,
};
use crate::procedure_registry::ProcedureError;
use crate::{
    GqlType, GraphContext, ProcedureDefaultValue, ProcedureOutputColumn, ProcedureParameter,
    ProcedureResult,
};

const PROC_NAME: &str = "selene.vector_score_expanded_candidates_batch";

static VECTOR_SCORE_EXPANDED_BATCH_OUTPUTS: [StaticOutputColumn; 3] = [
    StaticOutputColumn::new("query_index", GqlType::Uint64)
        .with_description("Zero-based query position."),
    StaticOutputColumn::new("node_id", GqlType::NodeRef)
        .with_description("Scored expanded-candidate node id."),
    StaticOutputColumn::new("distance", GqlType::Float64)
        .with_description("Lower-is-better distance."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("queries", GqlType::List(Box::new(GqlType::Vector)), false)
            .with_description("Query vectors."),
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
    VECTOR_SCORE_EXPANDED_BATCH_OUTPUTS
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
    let root_sets = candidate_sets_arg(ctx.snapshot(), PROC_NAME, &args[2], "roots")?;
    if queries.len() != root_sets.len() {
        return Err(invalid_arg(format!(
            "{PROC_NAME} queries and roots must have the same length"
        )));
    }
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

    let snapshot = ctx.snapshot();
    let batch_hits = snapshot
        .score_vector_expanded_candidate_sets_batch_checked(
            &property,
            &queries,
            &root_sets,
            VectorNeighborSearchOptions::new(&edge_label, direction, metric, k),
            ctx.cancellation_checker(),
        )
        .map_err(|error| {
            vector_search_error(
                error,
                "batched expanded vector candidate scoring",
                BatchMismatch::InvalidArgument,
                "batched expanded vector candidate scoring",
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
