//! Native pathfinding procedures (`algo.dijkstra`, `algo.sssp`, `algo.apsp`).
//!
//! Ported from the historical procedure-pack pathfinding adapter. Argument
//! parsing, output columns, the empty-result-on-no-path behavior for `dijkstra`,
//! and the per-procedure pathfinding error messages are preserved verbatim.

use selene_algorithms::{ApspConfig, apsp_with_checker, dijkstra_with_checker, sssp_with_checker};
use selene_core::{CancellationChecker, NodeId, Value};
use selene_graph::SeleneGraph;

use super::args::{expect_arity, required_node_ref, required_nonnegative_usize, required_string};
use super::error::pathfinding_error;
use super::meta::{output, parameter};
use super::parallel::parse_parallelism;
use super::state::{AlgorithmCatalogs, with_projection};
use crate::procedure_registry::ProcedureError;
use crate::{GqlType, ProcedureOutputColumn, ProcedureParameter, ProcedureResult};

const DIJKSTRA_PROC: &str = "algo.dijkstra";
const SSSP_PROC: &str = "algo.sssp";
const APSP_PROC: &str = "algo.apsp";

pub(super) fn dijkstra_signature() -> Vec<ProcedureParameter> {
    vec![
        parameter("projection_name", GqlType::String, false),
        parameter("from", GqlType::NodeRef, false),
        parameter("to", GqlType::NodeRef, false),
    ]
}

pub(super) fn sssp_signature() -> Vec<ProcedureParameter> {
    vec![
        parameter("projection_name", GqlType::String, false),
        parameter("source", GqlType::NodeRef, false),
    ]
}

pub(super) fn apsp_signature() -> Vec<ProcedureParameter> {
    vec![
        parameter("projection_name", GqlType::String, false),
        parameter("max_nodes", GqlType::Integer, false),
        parameter("parallelism", GqlType::Integer, true),
    ]
}

pub(super) fn dijkstra_columns() -> Vec<ProcedureOutputColumn> {
    vec![
        output("cost", GqlType::Float),
        output("path", GqlType::List(Box::new(GqlType::NodeRef))),
        output("length", GqlType::Uint64),
    ]
}

pub(super) fn sssp_columns() -> Vec<ProcedureOutputColumn> {
    vec![
        output("target_node", GqlType::NodeRef),
        output("cost", GqlType::Float),
    ]
}

pub(super) fn apsp_columns() -> Vec<ProcedureOutputColumn> {
    vec![
        output("source_node", GqlType::NodeRef),
        output("target_node", GqlType::NodeRef),
        output("cost", GqlType::Float),
    ]
}

pub(super) fn dijkstra(
    catalogs: &AlgorithmCatalogs,
    snapshot: &SeleneGraph,
    args: &[Value],
    checker: CancellationChecker<'_>,
) -> Result<ProcedureResult, ProcedureError> {
    let (projection_name, from, to) = parse_dijkstra_args(args)?;
    crate::runtime::reference_access::require_live_nodes(snapshot, [&from, &to].into_iter())?;
    with_projection(catalogs, snapshot, &projection_name, |projection| {
        let Some(result) = dijkstra_with_checker(projection, from, to, checker)
            .map_err(|error| pathfinding_error(DIJKSTRA_PROC, error))?
        else {
            return Ok(ProcedureResult { rows: Vec::new() });
        };
        let length = result.nodes.len() as u64;
        let path = result.nodes.into_iter().map(Value::NodeRef).collect();
        Ok(ProcedureResult {
            rows: vec![vec![
                Value::Float(result.cost),
                Value::List(path),
                Value::Uint(length),
            ]],
        })
    })
}

pub(super) fn sssp(
    catalogs: &AlgorithmCatalogs,
    snapshot: &SeleneGraph,
    args: &[Value],
    checker: CancellationChecker<'_>,
) -> Result<ProcedureResult, ProcedureError> {
    let (projection_name, source) = parse_sssp_args(args)?;
    crate::runtime::reference_access::require_live_nodes(snapshot, std::iter::once(&source))?;
    with_projection(catalogs, snapshot, &projection_name, |projection| {
        let rows = sssp_with_checker(projection, source, checker)
            .map_err(|error| pathfinding_error(SSSP_PROC, error))?
            .into_iter()
            .map(|(target_node, cost)| vec![Value::NodeRef(target_node), Value::Float(cost)])
            .collect();
        Ok(ProcedureResult { rows })
    })
}

pub(super) fn apsp(
    catalogs: &AlgorithmCatalogs,
    snapshot: &SeleneGraph,
    args: &[Value],
    checker: CancellationChecker<'_>,
) -> Result<ProcedureResult, ProcedureError> {
    let (projection_name, config) = parse_apsp_args(args)?;
    with_projection(catalogs, snapshot, &projection_name, |projection| {
        let rows = apsp_with_checker(projection, config, checker)
            .map_err(|error| pathfinding_error(APSP_PROC, error))?
            .into_iter()
            .map(|(source_node, target_node, cost)| {
                vec![
                    Value::NodeRef(source_node),
                    Value::NodeRef(target_node),
                    Value::Float(cost),
                ]
            })
            .collect();
        Ok(ProcedureResult { rows })
    })
}

fn parse_dijkstra_args(args: &[Value]) -> Result<(String, NodeId, NodeId), ProcedureError> {
    expect_arity(DIJKSTRA_PROC, args, 3)?;
    let projection_name = required_string(DIJKSTRA_PROC, args, 0, "projection_name")?;
    let from = required_node_ref(DIJKSTRA_PROC, args, 1, "from")?;
    let to = required_node_ref(DIJKSTRA_PROC, args, 2, "to")?;
    Ok((projection_name, from, to))
}

fn parse_sssp_args(args: &[Value]) -> Result<(String, NodeId), ProcedureError> {
    expect_arity(SSSP_PROC, args, 2)?;
    let projection_name = required_string(SSSP_PROC, args, 0, "projection_name")?;
    let source = required_node_ref(SSSP_PROC, args, 1, "source")?;
    Ok((projection_name, source))
}

fn parse_apsp_args(args: &[Value]) -> Result<(String, ApspConfig), ProcedureError> {
    expect_arity(APSP_PROC, args, 3)?;
    let projection_name = required_string(APSP_PROC, args, 0, "projection_name")?;
    let max_nodes = required_nonnegative_usize(APSP_PROC, args, 1, "max_nodes")?;
    let parallelism = parse_parallelism(APSP_PROC, &args[2])?;
    Ok((
        projection_name,
        ApspConfig {
            max_nodes,
            parallelism,
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use selene_algorithms::Parallelism;
    use selene_core::{NodeId, Value, db_string};

    use super::*;

    fn projection_name() -> Value {
        Value::String(db_string("p").expect("test string fits DB string cap"))
    }

    #[test]
    fn dijkstra_args_accept_projection_and_node_refs() {
        let (projection, from, to) = parse_dijkstra_args(&[
            projection_name(),
            Value::NodeRef(NodeId::new(1)),
            Value::NodeRef(NodeId::new(2)),
        ])
        .expect("dijkstra args parse");

        assert_eq!(projection, "p");
        assert_eq!(from, NodeId::new(1));
        assert_eq!(to, NodeId::new(2));
    }

    #[test]
    fn sssp_args_reject_integer_source() {
        let err = parse_sssp_args(&[projection_name(), Value::Int(1)])
            .expect_err("source must be NodeRef");

        assert!(matches!(err, ProcedureError::InvalidArgument { .. }));
    }

    #[test]
    fn apsp_args_reject_negative_max_nodes() {
        let err = parse_apsp_args(&[projection_name(), Value::Int(-1), Value::Null])
            .expect_err("negative rejected");

        assert!(matches!(err, ProcedureError::InvalidArgument { .. }));
    }

    #[test]
    fn apsp_args_accept_unsigned_max_nodes() {
        let (projection, config) =
            parse_apsp_args(&[projection_name(), Value::Uint(12), Value::Null])
                .expect("unsigned max_nodes parses");

        assert_eq!(projection, "p");
        assert_eq!(config.max_nodes, 12);
        assert_eq!(config.parallelism, Parallelism::Auto);
    }

    #[test]
    fn apsp_args_parse_parallelism_null_zero_and_thread_count() {
        let (_, auto) = parse_apsp_args(&[projection_name(), Value::Int(12), Value::Null])
            .expect("NULL parallelism parses");
        let (_, sequential) = parse_apsp_args(&[projection_name(), Value::Int(12), Value::Int(0)])
            .expect("zero parallelism parses");
        let (_, threaded) = parse_apsp_args(&[projection_name(), Value::Int(12), Value::Uint(4)])
            .expect("uint parallelism parses");

        assert_eq!(auto.parallelism, Parallelism::Auto);
        assert_eq!(sequential.parallelism, Parallelism::Sequential);
        assert_eq!(
            threaded.parallelism,
            Parallelism::Threads(NonZeroUsize::new(4).unwrap())
        );
    }

    #[test]
    fn apsp_args_reject_negative_parallelism() {
        let err = parse_apsp_args(&[projection_name(), Value::Int(12), Value::Int(-1)])
            .expect_err("negative parallelism rejected");

        let ProcedureError::InvalidArgument { detail } = err else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        assert!(detail.contains("parallelism"));
    }

    #[test]
    fn apsp_args_reject_parallelism_above_adapter_cap() {
        let err = parse_apsp_args(&[projection_name(), Value::Int(12), Value::Uint(1025)])
            .expect_err("oversized parallelism rejected");

        let ProcedureError::InvalidArgument { detail } = err else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        assert!(detail.contains("1024"));
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn apsp_args_reject_unsigned_max_nodes_overflow() {
        let err = parse_apsp_args(&[projection_name(), Value::Uint(u64::MAX), Value::Null])
            .expect_err("oversized unsigned max_nodes rejected");

        let ProcedureError::InvalidArgument { detail } = err else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        assert_eq!(detail, "algo.apsp: max_nodes is too large");
    }
}
