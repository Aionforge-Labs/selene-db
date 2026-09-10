//! `selene.vector_score_nodes` native built-in.
//!
//! Read-only graph-tier procedure that reranks an explicit `LIST<NODE>`
//! candidate set by a vector-valued node property. This is the policy-neutral
//! graph/vector bridge: candidate production stays in GQL patterns or graph
//! algorithms, while vector scoring stays in the native vector engine.

use selene_core::{Value, VectorMetric};

use super::meta::{StaticOutputColumn, StaticParameter};
use super::vector_common::{
    BatchMismatch, cardinality_arg, invalid_arg, live_node_list_arg, metric_arg, query_arg,
    string_arg, vector_search_error,
};
use crate::procedure_registry::ProcedureError;
use crate::{
    GqlType, GraphContext, ProcedureDefaultValue, ProcedureOutputColumn, ProcedureParameter,
    ProcedureResult,
};

const PROC_NAME: &str = "selene.vector_score_nodes";

static VECTOR_SCORE_OUTPUTS: [StaticOutputColumn; 2] = [
    StaticOutputColumn::new("node_id", GqlType::NodeRef).with_description("Scored node id."),
    StaticOutputColumn::new("distance", GqlType::Float64)
        .with_description("Lower-is-better distance."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("query", GqlType::Vector, false).with_description("Query vector."),
        StaticParameter::new("nodes", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Candidate nodes to score."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
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
    if !(4..=5).contains(&args.len()) {
        return Err(invalid_arg(format!("{PROC_NAME} expects 4 or 5 arguments")));
    }

    let property = string_arg(PROC_NAME, &args[0], "property")?;
    let query = query_arg(PROC_NAME, &args[1])?;
    let nodes = live_node_list_arg(ctx.snapshot(), PROC_NAME, &args[2], "nodes")?;
    let k = cardinality_arg(PROC_NAME, &args[3], "k")?;
    let metric = args
        .get(4)
        .map(|arg| metric_arg(PROC_NAME, arg))
        .transpose()?
        .unwrap_or(VectorMetric::SquaredEuclidean);

    let hits = ctx
        .snapshot()
        .score_vector_nodes_checked(
            &property,
            &query,
            &nodes,
            metric,
            k,
            ctx.cancellation_checker(),
        )
        .map_err(|error| {
            vector_search_error(
                error,
                "vector candidate scoring",
                BatchMismatch::Internal("vector candidate scoring received batched-only error"),
                "vector candidate scoring",
            )
        })?;

    let rows = hits
        .into_iter()
        .map(|hit| vec![Value::NodeRef(hit.node_id), Value::Float(hit.distance)])
        .collect();
    Ok(ProcedureResult { rows })
}
