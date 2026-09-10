//! `selene.vector_score_nodes_batch` native built-in.
//!
//! Read-only graph-tier procedure that reranks one explicit `LIST<NODE>`
//! candidate set per query vector. This closes the graph/vector boundary for
//! graph-derived candidate producers: GQL patterns or graph algorithms can
//! build per-query candidate sets, then call the vector engine once and regroup
//! rows by `query_index`.

use selene_core::{Value, VectorMetric};

use super::meta::{StaticOutputColumn, StaticParameter};
use super::vector_common::{
    BatchMismatch, cardinality_arg, invalid_arg, live_node_list_sets_arg, metric_arg, queries_arg,
    query_index_too_large, string_arg, vector_search_error,
};
use crate::procedure_registry::ProcedureError;
use crate::{
    GqlType, GraphContext, ProcedureDefaultValue, ProcedureOutputColumn, ProcedureParameter,
    ProcedureResult,
};

const PROC_NAME: &str = "selene.vector_score_nodes_batch";

static VECTOR_SCORE_BATCH_OUTPUTS: [StaticOutputColumn; 3] = [
    StaticOutputColumn::new("query_index", GqlType::Uint64)
        .with_description("Zero-based query position."),
    StaticOutputColumn::new("node_id", GqlType::NodeRef).with_description("Scored node id."),
    StaticOutputColumn::new("distance", GqlType::Float64)
        .with_description("Lower-is-better distance."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("queries", GqlType::List(Box::new(GqlType::Vector)), false)
            .with_description("Query vectors."),
        StaticParameter::new(
            "nodes",
            GqlType::List(Box::new(GqlType::List(Box::new(GqlType::NodeRef)))),
            false,
        )
        .with_description("Per-query candidate nodes to score."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count per query."),
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
    VECTOR_SCORE_BATCH_OUTPUTS
        .iter()
        .cloned()
        .map(StaticOutputColumn::into_output_column)
        .collect()
}

pub(super) fn execute(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if !(4..=5).contains(&args.len()) {
        return Err(invalid_arg(format!("{PROC_NAME} expects 4 or 5 arguments")));
    }

    let property = string_arg(PROC_NAME, &args[0], "property")?;
    let queries = queries_arg(PROC_NAME, &args[1])?;
    let node_sets = live_node_list_sets_arg(ctx.snapshot(), PROC_NAME, &args[2], "nodes")?;
    if queries.len() != node_sets.len() {
        return Err(invalid_arg(format!(
            "{PROC_NAME} queries and nodes must have the same length"
        )));
    }
    let k = cardinality_arg(PROC_NAME, &args[3], "k")?;
    let metric = args
        .get(4)
        .map(|arg| metric_arg(PROC_NAME, arg))
        .transpose()?
        .unwrap_or(VectorMetric::SquaredEuclidean);

    let batch_hits = ctx
        .snapshot()
        .score_vector_nodes_batch_checked(
            &property,
            &queries,
            &node_sets,
            metric,
            k,
            ctx.cancellation_checker(),
        )
        .map_err(|error| {
            vector_search_error(
                error,
                "batched vector candidate scoring",
                BatchMismatch::InvalidArgument,
                "batched vector candidate scoring",
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
