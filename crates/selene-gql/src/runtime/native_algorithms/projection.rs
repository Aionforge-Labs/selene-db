//! Native projection-catalog procedures (`algo.projection_*`).
//!
//! Ported from the historical procedure-pack projection adapter. Row shapes,
//! output columns, and arity are preserved verbatim; the only change is that
//! catalog state is resolved from the registry-owned per-`GraphId`
//! [`AlgorithmCatalogs`] instead of the pack's per-instance catalog state.

use std::sync::Arc;

use selene_algorithms::{GraphProjection, ProjectionConfig};
use selene_core::Value;
use selene_graph::SeleneGraph;

use super::args::{expect_arity, nullable_db_string, nullable_db_string_list, required_string};
use super::error::algorithm_error;
use super::meta::{output, parameter};
use super::state::AlgorithmCatalogs;
use crate::procedure_registry::ProcedureError;
use crate::{GqlType, ProcedureOutputColumn, ProcedureParameter, ProcedureResult};

const BUILD_PROC: &str = "algo.projection_build";
const GET_PROC: &str = "algo.projection_get";
const DROP_PROC: &str = "algo.projection_drop";
const LIST_PROC: &str = "algo.projection_list";

pub(super) fn build_signature() -> Vec<ProcedureParameter> {
    vec![
        parameter("name", GqlType::String, false),
        parameter(
            "node_labels",
            GqlType::List(Box::new(GqlType::String)),
            true,
        ),
        parameter(
            "edge_labels",
            GqlType::List(Box::new(GqlType::String)),
            true,
        ),
        parameter("weight_property", GqlType::String, true),
    ]
}

pub(super) fn get_signature() -> Vec<ProcedureParameter> {
    vec![parameter("name", GqlType::String, false)]
}

pub(super) fn drop_signature() -> Vec<ProcedureParameter> {
    vec![parameter("name", GqlType::String, false)]
}

pub(super) fn list_signature() -> Vec<ProcedureParameter> {
    Vec::new()
}

pub(super) fn projection_output_columns() -> Vec<ProcedureOutputColumn> {
    vec![
        output("name", GqlType::String),
        output("generation", GqlType::Uint64),
        output("node_count", GqlType::Uint64),
        output("edge_count", GqlType::Uint64),
    ]
}

pub(super) fn empty_columns() -> Vec<ProcedureOutputColumn> {
    Vec::new()
}

pub(super) fn build(
    catalogs: &AlgorithmCatalogs,
    snapshot: &SeleneGraph,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    expect_arity(BUILD_PROC, args, 4)?;
    let config = ProjectionConfig {
        name: required_string(BUILD_PROC, args, 0, "name")?,
        node_labels: nullable_db_string_list(BUILD_PROC, args, 1, "node_labels")?,
        edge_labels: nullable_db_string_list(BUILD_PROC, args, 2, "edge_labels")?,
        weight_property: nullable_db_string(BUILD_PROC, args, 3, "weight_property")?,
    };
    catalogs.with_catalog(snapshot.graph_id(), |catalog| {
        catalog
            .project(snapshot, &config)
            .map_err(algorithm_error)?;
        Ok(unit_result())
    })
}

pub(super) fn get(
    catalogs: &AlgorithmCatalogs,
    snapshot: &SeleneGraph,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    expect_arity(GET_PROC, args, 1)?;
    let name = required_string(GET_PROC, args, 0, "name")?;
    catalogs.with_catalog(snapshot.graph_id(), |catalog| {
        let projection = catalog.resolve(snapshot, &name).map_err(algorithm_error)?;
        Ok(ProcedureResult {
            rows: vec![projection_row(projection.projection())],
        })
    })
}

pub(super) fn drop_projection(
    catalogs: &AlgorithmCatalogs,
    snapshot: &SeleneGraph,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    expect_arity(DROP_PROC, args, 1)?;
    let name = required_string(DROP_PROC, args, 0, "name")?;
    catalogs.with_catalog(snapshot.graph_id(), |catalog| {
        catalog.drop_projection(&name);
        Ok(unit_result())
    })
}

pub(super) fn list(
    catalogs: &AlgorithmCatalogs,
    snapshot: &SeleneGraph,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    expect_arity(LIST_PROC, args, 0)?;
    catalogs.with_catalog(snapshot.graph_id(), |catalog| {
        let mut names = catalog.names();
        names.sort();
        let snapshots: Vec<ProjectionSnapshot> = names
            .iter()
            .map(|name| {
                let projection = catalog.resolve(snapshot, name).map_err(algorithm_error)?;
                Ok(ProjectionSnapshot::from(projection.projection()))
            })
            .collect::<Result<_, ProcedureError>>()?;
        let rows = snapshots.into_iter().map(projection_snapshot_row).collect();
        Ok(ProcedureResult { rows })
    })
}

struct ProjectionSnapshot {
    name: Arc<str>,
    generation: u64,
    node_count: u64,
    edge_count: u64,
}

impl From<&GraphProjection> for ProjectionSnapshot {
    fn from(projection: &GraphProjection) -> Self {
        Self {
            name: Arc::<str>::from(projection.name()),
            generation: projection.generation(),
            node_count: projection.node_count() as u64,
            edge_count: projection.edge_count() as u64,
        }
    }
}

fn projection_snapshot_row(snapshot: ProjectionSnapshot) -> Vec<Value> {
    vec![
        // The projection name was already a validated catalog identifier, so
        // it is within the IL013 byte cap and constructs infallibly.
        Value::String(
            selene_core::db_string(snapshot.name.as_ref())
                .expect("projection name within IL013 cap"),
        ),
        Value::Uint(snapshot.generation),
        Value::Uint(snapshot.node_count),
        Value::Uint(snapshot.edge_count),
    ]
}

fn projection_row(projection: &GraphProjection) -> Vec<Value> {
    projection_snapshot_row(ProjectionSnapshot::from(projection))
}

fn unit_result() -> ProcedureResult {
    ProcedureResult {
        rows: vec![Vec::new()],
    }
}
