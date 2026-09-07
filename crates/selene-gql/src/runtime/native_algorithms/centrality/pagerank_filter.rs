//! Edge-property result filters for `algo.pagerank`.

use std::collections::BTreeSet;

use selene_core::{DbString, NodeId, Value};
use selene_graph::SeleneGraph;

use crate::procedure_registry::ProcedureError;
use crate::runtime::native_algorithms::error::invalid_argument;

const PAGERANK_PROC: &str = "algo.pagerank";

#[derive(Debug)]
pub(super) struct PageRankEdgeFilter {
    label: DbString,
    property: DbString,
    values: Vec<Value>,
    endpoint: EdgeFilterEndpoint,
}

#[derive(Clone, Copy, Debug)]
enum EdgeFilterEndpoint {
    Source,
    Target,
    Both,
}

pub(super) fn nullable_edge_filter(
    args: &[Value],
) -> Result<Option<PageRankEdgeFilter>, ProcedureError> {
    if args.len() <= 10 {
        return Ok(None);
    }
    if args.len() != 14 {
        return Err(invalid_argument(format!(
            "{PAGERANK_PROC} edge filter requires edge_filter_label, edge_filter_property, edge_filter_values, and edge_filter_endpoint"
        )));
    }
    match (&args[10], &args[11], &args[12], &args[13]) {
        (Value::Null, Value::Null, Value::Null, Value::Null) => Ok(None),
        (Value::String(label), Value::String(property), Value::List(values), endpoint) => {
            Ok(Some(PageRankEdgeFilter {
                label: label.clone(),
                property: property.clone(),
                values: values.clone(),
                endpoint: edge_endpoint_arg(endpoint)?,
            }))
        }
        _ => Err(invalid_argument(format!(
            "{PAGERANK_PROC} edge filter arguments must all be NULL or all be supplied, with edge_filter_values as LIST<VALUE> and endpoint as source, target, or both"
        ))),
    }
}

pub(super) fn resolve_edge_result_nodes(
    snapshot: &SeleneGraph,
    filter: &PageRankEdgeFilter,
) -> Result<BTreeSet<NodeId>, ProcedureError> {
    let edge_candidates = crate::runtime::property_filter_rows::edge_candidates_with_property_any(
        snapshot,
        &filter.label,
        &filter.property,
        &filter.values,
    )
    .map_err(|err| invalid_argument(format!("{PAGERANK_PROC}: {err}")))?
    .ok_or_else(|| {
        invalid_argument(format!(
            "{PAGERANK_PROC} edge_filter_property must name an indexed scalar edge property and edge_filter_values must match that index kind"
        ))
    })?;
    let mut nodes = BTreeSet::new();
    for edge_id in edge_candidates.iter() {
        let (source, target) =
            snapshot
                .edge_endpoints(edge_id)
                .ok_or_else(|| ProcedureError::Internal {
                    detail: format!(
                        "{PAGERANK_PROC} indexed edge filter edge {edge_id} has no endpoints"
                    ),
                })?;
        match filter.endpoint {
            EdgeFilterEndpoint::Source => {
                nodes.insert(source);
            }
            EdgeFilterEndpoint::Target => {
                nodes.insert(target);
            }
            EdgeFilterEndpoint::Both => {
                nodes.insert(source);
                nodes.insert(target);
            }
        }
    }
    Ok(nodes)
}

pub(super) fn intersect_result_nodes(
    left: Option<BTreeSet<NodeId>>,
    right: BTreeSet<NodeId>,
) -> Option<BTreeSet<NodeId>> {
    match left {
        Some(left) => Some(left.intersection(&right).copied().collect()),
        None => Some(right),
    }
}

fn edge_endpoint_arg(value: &Value) -> Result<EdgeFilterEndpoint, ProcedureError> {
    let Value::String(raw) = value else {
        return Err(invalid_argument(format!(
            "{PAGERANK_PROC} edge_filter_endpoint must be source, target, or both"
        )));
    };
    match raw.as_str() {
        "source" | "from" | "outgoing" | "out" => Ok(EdgeFilterEndpoint::Source),
        "target" | "to" | "incoming" | "in" => Ok(EdgeFilterEndpoint::Target),
        "both" | "either" | "any" => Ok(EdgeFilterEndpoint::Both),
        other => Err(invalid_argument(format!(
            "{PAGERANK_PROC} unknown edge_filter_endpoint '{other}'; expected source, target, or both"
        ))),
    }
}
