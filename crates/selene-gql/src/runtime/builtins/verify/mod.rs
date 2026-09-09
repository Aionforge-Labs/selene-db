//! `selene.verify` native built-in.
//!
//! Read-only graph-tier integrity check against graph invariants. This module
//! owns metadata, argument parsing, `verify_snapshot` orchestration and result
//! shaping. Mixed-edge incidence auditing lives in [`adjacency`]; other
//! integrity checks live in [`checks`].

mod adjacency;
mod checks;

use selene_core::{Value, db_string};
use selene_graph::SeleneGraph;

use super::meta::{StaticOutputColumn, StaticParameter};
use crate::procedure_registry::ProcedureError;
use crate::{
    GqlType, GraphContext, ProcedureDefaultValue, ProcedureOutputColumn, ProcedureParameter,
    ProcedureResult,
};

static VERIFY_PARAMS: [StaticParameter; 1] =
    [StaticParameter::new("deep", GqlType::Boolean, false)
        .with_description("Whether to run slower deep integrity checks.")
        .with_default_doc("false")
        .with_default(ProcedureDefaultValue::Boolean(false))];

static VERIFY_OUTPUTS: [StaticOutputColumn; 3] = [
    StaticOutputColumn::new("check", GqlType::String).with_description("Integrity check name."),
    StaticOutputColumn::new("status", GqlType::String)
        .with_description("Integrity check status: ok or inconsistent."),
    StaticOutputColumn::new("detail", GqlType::String)
        .with_description("Human-readable integrity check detail."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    VERIFY_PARAMS
        .iter()
        .cloned()
        .map(StaticParameter::into_parameter)
        .collect()
}

pub(super) fn output_columns() -> Vec<ProcedureOutputColumn> {
    VERIFY_OUTPUTS
        .iter()
        .cloned()
        .map(StaticOutputColumn::into_output_column)
        .collect()
}

pub(super) fn execute(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    let deep = deep_arg(args)?;
    verify_snapshot(ctx.snapshot(), deep)
}

fn deep_arg(args: &[Value]) -> Result<bool, ProcedureError> {
    match args {
        [] => Ok(false),
        [Value::Bool(value)] => Ok(*value),
        [_] => Err(ProcedureError::InvalidArgument {
            detail: "selene.verify deep must be a BOOLEAN".to_owned(),
        }),
        _ => Err(ProcedureError::InvalidArgument {
            detail: "selene.verify expects at most 1 argument".to_owned(),
        }),
    }
}

/// Run the integrity checks against a snapshot.
///
/// Exposed to the test module (and crate-internal callers) so the relocated
/// integrity behavior is exercised exactly as the pack's `verify_snapshot` was.
pub(crate) fn verify_snapshot(
    snapshot: &SeleneGraph,
    deep: bool,
) -> Result<ProcedureResult, ProcedureError> {
    let mut rows = vec![
        check_row(
            "label_index_cardinality",
            checks::check_label_index_cardinality(snapshot),
        )?,
        check_row(
            "property_index_coverage",
            checks::check_property_index_coverage(snapshot),
        )?,
        check_row(
            "adjacency_symmetry",
            adjacency::check_adjacency_symmetry(snapshot),
        )?,
        check_row(
            "edge_endpoint_liveness",
            checks::check_edge_endpoint_liveness(snapshot),
        )?,
    ];
    if deep {
        rows.push(check_row(
            "typed_index_value_range",
            checks::check_typed_index_value_range(snapshot),
        )?);
        rows.push(check_row(
            "roaring_bitmap_density",
            checks::check_roaring_bitmap_density(snapshot),
        )?);
    }
    Ok(ProcedureResult { rows })
}

fn check_row(check: &'static str, result: CheckResult) -> Result<Vec<Value>, ProcedureError> {
    Ok(vec![
        static_string(check)?,
        static_string(if result.issues == 0 {
            "ok"
        } else {
            "inconsistent"
        })?,
        verify_string(&result.detail)?,
    ])
}

fn static_string(value: &'static str) -> Result<Value, ProcedureError> {
    verify_string(value)
}

fn verify_string(value: &str) -> Result<Value, ProcedureError> {
    db_string(value)
        .map(Value::String)
        .map_err(|_err| ProcedureError::Internal {
            detail: "selene.verify result string exceeds the maximum byte length".to_owned(),
        })
}

/// Outcome of one integrity check: a count of detected inconsistencies plus a
/// human-readable summary detail.
struct CheckResult {
    issues: usize,
    detail: String,
}

impl CheckResult {
    fn new(issues: usize, detail: String) -> Self {
        Self { issues, detail }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod mixed_tests;
