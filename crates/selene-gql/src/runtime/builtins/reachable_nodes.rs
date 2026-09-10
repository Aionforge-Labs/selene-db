//! `selene.reachable_nodes` native built-in.
//!
//! Read-only graph-tier procedure that produces a bounded transitive
//! reachability candidate set from explicit root nodes.

use selene_core::Value;
use selene_graph::{GraphError, ReachabilityDirection, ReachabilityError};

use super::meta::{StaticOutputColumn, StaticParameter};
use super::vector_common::{cardinality_arg, invalid_arg, live_node_list_arg, string_arg};
use crate::procedure_registry::ProcedureError;
use crate::{
    GqlType, GraphContext, ProcedureDefaultValue, ProcedureOutputColumn, ProcedureParameter,
    ProcedureResult,
};

const PROC_NAME: &str = "selene.reachable_nodes";

pub(super) fn signature() -> Vec<ProcedureParameter> {
    [
        StaticParameter::new("roots", GqlType::List(Box::new(GqlType::NodeRef)), false)
            .with_description("Root nodes to preserve at depth 0."),
        StaticParameter::new("edge_label", GqlType::String, false)
            .with_description("Edge label used for reachability traversal."),
        StaticParameter::new("k", GqlType::Integer, false)
            .with_description("Maximum reachable node count."),
        StaticParameter::new("max_depth", GqlType::Integer, true)
            .with_description("Maximum traversal depth, or NULL for unbounded traversal.")
            .with_default_doc("NULL (unbounded)")
            .with_default(ProcedureDefaultValue::Null),
        StaticParameter::new("direction", GqlType::String, false)
            .with_description("Traversal direction: outgoing, incoming, or both.")
            .with_default_doc("outgoing")
            .with_default(ProcedureDefaultValue::String("outgoing")),
    ]
    .into_iter()
    .map(StaticParameter::into_parameter)
    .collect()
}

pub(super) fn output_columns() -> Vec<ProcedureOutputColumn> {
    [
        StaticOutputColumn::new("node_id", GqlType::NodeRef).with_description("Reachable node id."),
        StaticOutputColumn::new("depth", GqlType::Uint64)
            .with_description("Minimum hop depth from any root."),
    ]
    .into_iter()
    .map(StaticOutputColumn::into_output_column)
    .collect()
}

pub(super) fn execute(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if !(3..=5).contains(&args.len()) {
        return Err(invalid_arg(format!(
            "{PROC_NAME} expects 3, 4, or 5 arguments"
        )));
    }
    let roots = live_node_list_arg(ctx.snapshot(), PROC_NAME, &args[0], "roots")?;
    let edge_label = string_arg(PROC_NAME, &args[1], "edge_label")?;
    let k = cardinality_arg(PROC_NAME, &args[2], "k")?;
    let max_depth = args.get(3).map(max_depth_arg).transpose()?.flatten();
    let direction = args
        .get(4)
        .map(direction_arg)
        .transpose()?
        .unwrap_or(ReachabilityDirection::Outgoing);

    let rows = ctx
        .snapshot()
        .reachable_nodes_checked(
            &roots,
            &edge_label,
            direction,
            max_depth,
            k,
            ctx.cancellation_checker(),
        )
        .map_err(reachability_error)?
        .into_iter()
        .map(|hit| {
            vec![
                Value::NodeRef(hit.node_id),
                Value::Uint(usize_to_u64_saturating(hit.depth)),
            ]
        })
        .collect();
    Ok(ProcedureResult { rows })
}

fn max_depth_arg(value: &Value) -> Result<Option<usize>, ProcedureError> {
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    Ok(Some(cardinality_arg(PROC_NAME, value, "max_depth")?))
}

fn direction_arg(value: &Value) -> Result<ReachabilityDirection, ProcedureError> {
    let direction = string_arg(PROC_NAME, value, "direction")?;
    let raw = direction.as_str();
    match raw.to_ascii_lowercase().as_str() {
        "outgoing" | "out" => Ok(ReachabilityDirection::Outgoing),
        "incoming" | "in" => Ok(ReachabilityDirection::Incoming),
        "both" | "any" => Ok(ReachabilityDirection::Both),
        _ => Err(invalid_arg(format!(
            "unknown reachability direction '{raw}'; expected outgoing, incoming, or both"
        ))),
    }
}

fn reachability_error(error: ReachabilityError) -> ProcedureError {
    match error {
        ReachabilityError::Cancelled => ProcedureError::Cancelled,
        ReachabilityError::Timeout { elapsed } => ProcedureError::Timeout { elapsed },
        ReachabilityError::NodeScanBudgetExceeded { limit, scanned } => {
            ProcedureError::NodeScanBudgetExceeded { limit, scanned }
        }
        ReachabilityError::Graph(GraphError::Inconsistent { reason }) => ProcedureError::Internal {
            detail: format!("graph inconsistency during reachability traversal: {reason}"),
        },
        ReachabilityError::Graph(other) => ProcedureError::Internal {
            detail: format!("unexpected graph error during reachability traversal: {other}"),
        },
    }
}

fn usize_to_u64_saturating(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_inconsistency_maps_to_stable_internal_context() {
        let error = reachability_error(ReachabilityError::Graph(GraphError::Inconsistent {
            reason: "node mapping drift".to_owned(),
        }));

        assert!(matches!(
            error,
            ProcedureError::Internal { detail }
                if detail == "graph inconsistency during reachability traversal: node mapping drift"
        ));
    }

    #[test]
    fn unexpected_graph_error_maps_defensively_to_internal() {
        let error = reachability_error(ReachabilityError::Graph(GraphError::RowSpaceExhausted {
            kind: "node",
            rows: u64::from(u32::MAX),
            max_rows: u64::from(u32::MAX) - 1,
        }));

        assert!(matches!(
            error,
            ProcedureError::Internal { detail }
                if detail.starts_with("unexpected graph error during reachability traversal:")
        ));
    }
}
