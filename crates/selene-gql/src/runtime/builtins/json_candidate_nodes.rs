//! Candidate-scoped native JSON search built-ins.
//!
//! These procedures mirror the global JSON search surfaces but restrict work to
//! an explicit `LIST<NODE>` candidate set. They are read-only graph-tier
//! primitives for composing graph-produced candidates with JSON filters without
//! adding non-standard grammar.

use selene_core::Value;
use selene_graph::JsonPathContainmentCandidateOptions;

use super::json_path_common::{json_search_error, path_arg};
use super::meta::{StaticOutputColumn, StaticParameter};
use super::vector_common::{cardinality_arg, invalid_arg, live_node_list_arg, string_arg};
use crate::procedure_registry::ProcedureError;
use crate::{GqlType, GraphContext, ProcedureOutputColumn, ProcedureParameter, ProcedureResult};

const CONTAINS_PROC_NAME: &str = "selene.json_contains_candidate_nodes";
const PATH_EXISTS_PROC_NAME: &str = "selene.json_path_exists_candidate_nodes";
const PATH_CONTAINS_PROC_NAME: &str = "selene.json_path_contains_candidate_nodes";
const PATH_VALUE_PROC_NAME: &str = "selene.json_path_value_candidate_nodes";

static NODE_OUTPUTS: [StaticOutputColumn; 1] =
    [StaticOutputColumn::new("node_id", GqlType::NodeRef).with_description("Matched node id.")];

static VALUE_OUTPUTS: [StaticOutputColumn; 2] = [
    StaticOutputColumn::new("node_id", GqlType::NodeRef).with_description("Matched node id."),
    StaticOutputColumn::new("value", GqlType::Json)
        .with_description("JSON value selected by the path."),
];

pub(super) fn contains_signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("label", GqlType::String, false).with_description("Node label."),
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("candidate", GqlType::Json, false)
            .with_description("JSON candidate that stored values must contain."),
        StaticParameter::new("nodes", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Candidate nodes to filter."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn path_exists_signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("label", GqlType::String, false).with_description("Node label."),
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("path", GqlType::Json, false)
            .with_description("JSON array of string object keys and integer array indexes."),
        StaticParameter::new("nodes", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Candidate nodes to filter."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn path_contains_signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("label", GqlType::String, false).with_description("Node label."),
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("path", GqlType::Json, false)
            .with_description("JSON array of string object keys and integer array indexes."),
        StaticParameter::new("candidate", GqlType::Json, false)
            .with_description("JSON candidate that selected path values must contain."),
        StaticParameter::new("nodes", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Candidate nodes to filter."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn path_value_signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("label", GqlType::String, false).with_description("Node label."),
        StaticParameter::new("property", GqlType::String, false).with_description("Property name."),
        StaticParameter::new("path", GqlType::Json, false)
            .with_description("JSON array of string object keys and integer array indexes."),
        StaticParameter::new("nodes", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Candidate nodes to filter."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum result count."),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn output_columns() -> Vec<ProcedureOutputColumn> {
    NODE_OUTPUTS
        .iter()
        .cloned()
        .map(StaticOutputColumn::into_output_column)
        .collect()
}

pub(super) fn value_output_columns() -> Vec<ProcedureOutputColumn> {
    VALUE_OUTPUTS
        .iter()
        .cloned()
        .map(StaticOutputColumn::into_output_column)
        .collect()
}

pub(super) fn execute_contains(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if args.len() != 5 {
        return Err(invalid_arg(format!(
            "{CONTAINS_PROC_NAME} expects 5 arguments"
        )));
    }

    let label = string_arg(CONTAINS_PROC_NAME, &args[0], "label")?;
    let property = string_arg(CONTAINS_PROC_NAME, &args[1], "property")?;
    let Value::Json(candidate) = &args[2] else {
        return Err(invalid_arg(format!(
            "{CONTAINS_PROC_NAME} candidate must be JSON"
        )));
    };
    let nodes = live_node_list_arg(ctx.snapshot(), CONTAINS_PROC_NAME, &args[3], "nodes")?;
    let k = cardinality_arg(CONTAINS_PROC_NAME, &args[4], "k")?;

    let hits = ctx
        .snapshot()
        .exact_json_contains_candidate_nodes_checked(
            &label,
            &property,
            candidate,
            &nodes,
            k,
            ctx.cancellation_checker(),
        )
        .map_err(|err| json_search_error("JSON candidate containment search", err))?;
    Ok(node_result(hits.into_iter().map(|hit| hit.node_id)))
}

pub(super) fn execute_path_exists(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if args.len() != 5 {
        return Err(invalid_arg(format!(
            "{PATH_EXISTS_PROC_NAME} expects 5 arguments"
        )));
    }

    let label = string_arg(PATH_EXISTS_PROC_NAME, &args[0], "label")?;
    let property = string_arg(PATH_EXISTS_PROC_NAME, &args[1], "property")?;
    let path = path_arg(PATH_EXISTS_PROC_NAME, &args[2])?;
    let nodes = live_node_list_arg(ctx.snapshot(), PATH_EXISTS_PROC_NAME, &args[3], "nodes")?;
    let k = cardinality_arg(PATH_EXISTS_PROC_NAME, &args[4], "k")?;

    let hits = ctx
        .snapshot()
        .exact_json_path_exists_candidate_nodes_checked(
            &label,
            &property,
            &path,
            &nodes,
            k,
            ctx.cancellation_checker(),
        )
        .map_err(|err| json_search_error("JSON candidate path search", err))?;
    Ok(node_result(hits.into_iter().map(|hit| hit.node_id)))
}

pub(super) fn execute_path_contains(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if args.len() != 6 {
        return Err(invalid_arg(format!(
            "{PATH_CONTAINS_PROC_NAME} expects 6 arguments"
        )));
    }

    let label = string_arg(PATH_CONTAINS_PROC_NAME, &args[0], "label")?;
    let property = string_arg(PATH_CONTAINS_PROC_NAME, &args[1], "property")?;
    let path = path_arg(PATH_CONTAINS_PROC_NAME, &args[2])?;
    let Value::Json(candidate) = &args[3] else {
        return Err(invalid_arg(format!(
            "{PATH_CONTAINS_PROC_NAME} candidate must be JSON"
        )));
    };
    let nodes = live_node_list_arg(ctx.snapshot(), PATH_CONTAINS_PROC_NAME, &args[4], "nodes")?;
    let k = cardinality_arg(PATH_CONTAINS_PROC_NAME, &args[5], "k")?;

    let hits = ctx
        .snapshot()
        .exact_json_path_contains_candidate_nodes_checked(
            &label,
            &property,
            JsonPathContainmentCandidateOptions::new(&path, candidate, &nodes, k),
            ctx.cancellation_checker(),
        )
        .map_err(|err| json_search_error("JSON candidate path-containment search", err))?;
    Ok(node_result(hits.into_iter().map(|hit| hit.node_id)))
}

pub(super) fn execute_path_value(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if args.len() != 5 {
        return Err(invalid_arg(format!(
            "{PATH_VALUE_PROC_NAME} expects 5 arguments"
        )));
    }

    let label = string_arg(PATH_VALUE_PROC_NAME, &args[0], "label")?;
    let property = string_arg(PATH_VALUE_PROC_NAME, &args[1], "property")?;
    let path = path_arg(PATH_VALUE_PROC_NAME, &args[2])?;
    let nodes = live_node_list_arg(ctx.snapshot(), PATH_VALUE_PROC_NAME, &args[3], "nodes")?;
    let k = cardinality_arg(PATH_VALUE_PROC_NAME, &args[4], "k")?;

    let hits = ctx
        .snapshot()
        .exact_json_path_value_candidate_nodes_checked(
            &label,
            &property,
            &path,
            &nodes,
            k,
            ctx.cancellation_checker(),
        )
        .map_err(|err| json_search_error("JSON candidate path-value search", err))?;
    Ok(ProcedureResult {
        rows: hits
            .into_iter()
            .map(|hit| vec![Value::NodeRef(hit.node_id), Value::Json(hit.value)])
            .collect(),
    })
}

fn node_result(nodes: impl Iterator<Item = selene_core::NodeId>) -> ProcedureResult {
    ProcedureResult {
        rows: nodes.map(|node_id| vec![Value::NodeRef(node_id)]).collect(),
    }
}
