//! Shared candidate-filter helpers for retrieval candidate producers.

use selene_core::{DbString, NodeId, Value};
use selene_graph::{CandidateSet, Edge, Node, SeleneGraph};

use super::meta::StaticParameter;
use super::vector_common::{invalid_arg, string_arg};
use crate::procedure_registry::ProcedureError;
use crate::runtime::property_filter_rows::{
    edge_candidates_with_property_any, node_candidates_with_property_any,
};
use crate::{GqlType, ProcedureDefaultValue, ProcedureParameter};

fn graph_error(proc_name: &'static str, err: impl std::fmt::Display) -> ProcedureError {
    ProcedureError::Internal {
        detail: format!("{proc_name}: {err}"),
    }
}

enum EdgeFilterEndpoint {
    Source,
    Target,
    Both,
}

/// Append the legacy node-property filter parameter pair.
pub(super) fn append_node_filter_parameters(params: &mut Vec<ProcedureParameter>) {
    params.push(
        StaticParameter::new("filter_property", GqlType::String, true)
            .with_description("Indexed scalar node property used to admit matching nodes.")
            .with_default_doc("NULL (no property filter)")
            .with_default(ProcedureDefaultValue::Null)
            .into_parameter(),
    );
    params.push(
        StaticParameter::new(
            "filter_values",
            GqlType::List(Box::new(GqlType::AnyProperty)),
            true,
        )
        .with_description("Indexed scalar values admitted by filter_property.")
        .with_default_doc("NULL (no property filter)")
        .with_default(ProcedureDefaultValue::Null)
        .into_parameter(),
    );
}

/// Append the edge-property endpoint filter parameter group.
pub(super) fn append_edge_filter_parameters(params: &mut Vec<ProcedureParameter>) {
    params.push(
        StaticParameter::new("edge_filter_label", GqlType::String, true)
            .with_description("Indexed edge label used to admit incident nodes.")
            .with_default_doc("NULL (no edge filter)")
            .with_default(ProcedureDefaultValue::Null)
            .into_parameter(),
    );
    params.push(
        StaticParameter::new("edge_filter_property", GqlType::String, true)
            .with_description("Indexed scalar edge property used to admit incident nodes.")
            .with_default_doc("NULL (no edge filter)")
            .with_default(ProcedureDefaultValue::Null)
            .into_parameter(),
    );
    params.push(
        StaticParameter::new(
            "edge_filter_values",
            GqlType::List(Box::new(GqlType::AnyProperty)),
            true,
        )
        .with_description("Indexed scalar values admitted by edge_filter_property.")
        .with_default_doc("NULL (no edge filter)")
        .with_default(ProcedureDefaultValue::Null)
        .into_parameter(),
    );
    params.push(
        StaticParameter::new("edge_filter_endpoint", GqlType::String, true)
            .with_description("Endpoint admitted from matching edges: source, target, or both.")
            .with_default_doc("NULL (no edge filter)")
            .with_default(ProcedureDefaultValue::Null)
            .into_parameter(),
    );
}

/// Resolve optional node- and edge-property filters into a candidate node set.
pub(super) fn optional_filter_candidates(
    proc_name: &'static str,
    snapshot: &SeleneGraph,
    label: &DbString,
    node_property: &Value,
    node_values: &Value,
    edge_filter: Option<(&Value, &Value, &Value, &Value)>,
) -> Result<Option<CandidateSet<Node>>, ProcedureError> {
    let mut candidates =
        optional_node_filter_candidates(proc_name, snapshot, label, node_property, node_values)?;
    if let Some((edge_label, edge_property, edge_values, endpoint)) = edge_filter {
        let edge_candidates = optional_edge_filter_candidates(
            proc_name,
            snapshot,
            label,
            edge_label,
            edge_property,
            edge_values,
            endpoint,
        )?;
        candidates = match (candidates, edge_candidates) {
            (Some(c), Some(ec)) => Some(
                snapshot
                    .intersect_candidates(&c, &ec)
                    .map_err(|e| graph_error(proc_name, e))?,
            ),
            (Some(c), None) => Some(c),
            (None, Some(ec)) => Some(ec),
            (None, None) => None,
        };
    }
    Ok(candidates)
}

fn optional_node_filter_candidates(
    proc_name: &'static str,
    snapshot: &SeleneGraph,
    label: &DbString,
    property: &Value,
    values: &Value,
) -> Result<Option<CandidateSet<Node>>, ProcedureError> {
    match (property, values) {
        (Value::Null, Value::Null) => Ok(None),
        (Value::Null, _) | (_, Value::Null) => Err(invalid_arg(format!(
            "{proc_name} filter_property and filter_values must both be NULL or both be supplied"
        ))),
        (_, Value::List(values)) => {
            let property = string_arg(proc_name, property, "filter_property")?;
            node_candidates_with_property_any(snapshot, label, &property, values)
                .map_err(|e| graph_error(proc_name, e))?
                .map(Some)
                .ok_or_else(|| {
                    invalid_arg(format!(
                        "{proc_name} filter_property must name an indexed scalar node property and filter_values must match that index kind"
                    ))
                })
        }
        (_, _) => Err(invalid_arg(format!(
            "{proc_name} filter_values must be a LIST<VALUE> or NULL"
        ))),
    }
}

fn optional_edge_filter_candidates(
    proc_name: &'static str,
    snapshot: &SeleneGraph,
    candidate_label: &DbString,
    edge_label: &Value,
    edge_property: &Value,
    edge_values: &Value,
    endpoint: &Value,
) -> Result<Option<CandidateSet<Node>>, ProcedureError> {
    match (edge_label, edge_property, edge_values, endpoint) {
        (Value::Null, Value::Null, Value::Null, Value::Null) => Ok(None),
        (Value::String(_), Value::String(_), Value::List(values), Value::String(_)) => {
            let label = string_arg(proc_name, edge_label, "edge_filter_label")?;
            let property = string_arg(proc_name, edge_property, "edge_filter_property")?;
            let endpoint = edge_endpoint_arg(proc_name, endpoint)?;
            let edge_candidates = edge_candidates_with_property_any(snapshot, &label, &property, values)
                .map_err(|e| graph_error(proc_name, e))?
                .ok_or_else(|| {
                    invalid_arg(format!(
                        "{proc_name} edge_filter_property must name an indexed scalar edge property and edge_filter_values must match that index kind"
                    ))
                })?;
            edge_candidates_to_node_candidates(
                proc_name,
                snapshot,
                candidate_label,
                &edge_candidates,
                endpoint,
            )
            .map(Some)
        }
        (_, _, Value::List(_), _) => Err(invalid_arg(format!(
            "{proc_name} edge_filter_label, edge_filter_property, edge_filter_values, and edge_filter_endpoint must all be NULL or all be supplied"
        ))),
        _ => Err(invalid_arg(format!(
            "{proc_name} edge_filter_values must be a LIST<VALUE> and edge_filter_endpoint must be source, target, or both"
        ))),
    }
}

fn edge_candidates_to_node_candidates(
    proc_name: &'static str,
    snapshot: &SeleneGraph,
    candidate_label: &DbString,
    edge_candidates: &CandidateSet<Edge>,
    endpoint: EdgeFilterEndpoint,
) -> Result<CandidateSet<Node>, ProcedureError> {
    let mut matching = Vec::new();
    let check_node = |node: NodeId, list: &mut Vec<NodeId>| {
        if snapshot
            .node_labels(node)
            .is_some_and(|labels| labels.contains(candidate_label))
        {
            list.push(node);
        }
    };
    for edge_id in edge_candidates.iter() {
        let (source, target) =
            snapshot
                .edge_endpoints(edge_id)
                .ok_or_else(|| ProcedureError::Internal {
                    detail: format!(
                        "{proc_name} indexed edge filter edge {edge_id} has no endpoints"
                    ),
                })?;
        match endpoint {
            EdgeFilterEndpoint::Source => check_node(source, &mut matching),
            EdgeFilterEndpoint::Target => check_node(target, &mut matching),
            EdgeFilterEndpoint::Both => {
                check_node(source, &mut matching);
                check_node(target, &mut matching);
            }
        }
    }
    snapshot
        .bind_node_candidates(matching)
        .map_err(|e| graph_error(proc_name, e))
}

fn edge_endpoint_arg(
    proc_name: &'static str,
    value: &Value,
) -> Result<EdgeFilterEndpoint, ProcedureError> {
    let raw = string_arg(proc_name, value, "edge_filter_endpoint")?;
    match raw.as_str() {
        "source" | "from" | "outgoing" | "out" => Ok(EdgeFilterEndpoint::Source),
        "target" | "to" | "incoming" | "in" => Ok(EdgeFilterEndpoint::Target),
        "both" | "either" | "any" => Ok(EdgeFilterEndpoint::Both),
        other => Err(invalid_arg(format!(
            "{proc_name} unknown edge_filter_endpoint '{other}'; expected source, target, or both"
        ))),
    }
}
