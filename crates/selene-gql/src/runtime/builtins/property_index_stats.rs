//! `selene.property_index_stats` native built-in.
//!
//! Read-only graph-tier procedure making property-index drift diagnosable
//! (#1102).
//!
//! A typed property index cannot key a value whose variant does not match its
//! registered kind, and an open graph accepts a value of any variant. Since
//! #1099 a single unkeyable live row makes the index decline *every* probe, so
//! queries silently fall back to a scan. That is correct — the same rows come
//! back either way — but until now it was invisible: the index stays registered
//! and listed by `SHOW INDEXES`, and `selene.verify` deliberately audits
//! through the drift-ignoring probe, so it reports a clean bill of health.
//!
//! The `name` column is rendered with the same helpers `SHOW INDEXES` uses, so
//! the two surfaces join on it rather than each inventing a spelling.
//! Counts and `answers_probes` diagnose physical drift, not catalog activation.
//! Retained inactive/failed backing remains auditable but cannot admit queries.

use selene_core::{DbString, Value, db_string};
use selene_graph::PropertyIndexStatsRow;

use super::meta::{StaticOutputColumn, StaticParameter};
use crate::procedure_registry::ProcedureError;
use crate::runtime::pipeline::catalog_index::{
    render_composite_index_name, render_index_kind, render_index_name,
};
use crate::{GqlType, GraphContext, ProcedureOutputColumn, ProcedureParameter, ProcedureResult};

const PROC_NAME: &str = "selene.property_index_stats";

static PROPERTY_INDEX_STATS_OUTPUTS: [StaticOutputColumn; 8] = [
    StaticOutputColumn::new("name", GqlType::String)
        .with_description("Catalog index name, as SHOW INDEXES renders it."),
    StaticOutputColumn::new("entity", GqlType::String)
        .with_description("Indexed entity family: NODE or EDGE."),
    StaticOutputColumn::new("label", GqlType::String).with_description("Indexed label."),
    StaticOutputColumn::new("properties", GqlType::String)
        .with_description("Indexed properties in declaration order, comma-separated."),
    StaticOutputColumn::new("kind", GqlType::String)
        .with_description("Registered typed-index kind per property, comma-separated."),
    StaticOutputColumn::new("indexed_rows", GqlType::Uint64)
        .with_description("Rows the index can answer from."),
    StaticOutputColumn::new("drifted_rows", GqlType::Uint64)
        .with_description("Live rows the index cannot key, and therefore omits."),
    StaticOutputColumn::new("answers_probes", GqlType::Boolean).with_description(
        "False while drifted_rows > 0: the index is registered but every probe declines and \
         queries against it run as scans.",
    ),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    let params: [StaticParameter; 0] = [];
    params
        .into_iter()
        .map(StaticParameter::into_parameter)
        .collect()
}

pub(super) fn output_columns() -> Vec<ProcedureOutputColumn> {
    PROPERTY_INDEX_STATS_OUTPUTS
        .iter()
        .cloned()
        .map(StaticOutputColumn::into_output_column)
        .collect()
}

pub(super) fn execute(
    ctx: &GraphContext<'_>,
    args: &[Value],
) -> Result<ProcedureResult, ProcedureError> {
    if !args.is_empty() {
        return Err(ProcedureError::InvalidArgument {
            detail: format!("{PROC_NAME} expects zero arguments"),
        });
    }

    let mut rows = ctx
        .snapshot()
        .iter_property_index_stats()
        .collect::<Vec<_>>();
    // Deterministic order: entity, then label, then properties. Callers diff
    // this output across time to spot a newly demoted index, which a hash-map
    // iteration order would make impossible.
    rows.sort_by(|left, right| {
        left.entity
            .as_str()
            .cmp(right.entity.as_str())
            .then_with(|| left.label.as_str().cmp(right.label.as_str()))
            .then_with(|| join(&left.properties).cmp(&join(&right.properties)))
    });

    let rows = rows
        .into_iter()
        .map(into_values)
        .collect::<Result<Vec<_>, ProcedureError>>()?;
    Ok(ProcedureResult { rows })
}

fn into_values(row: PropertyIndexStatsRow) -> Result<Vec<Value>, ProcedureError> {
    let answers_probes = row.answers_probes();
    let name = render_name(&row);
    let kinds = row
        .kinds
        .iter()
        .map(|kind| render_index_kind(*kind))
        .collect::<Vec<_>>()
        .join(",");
    Ok(vec![
        string(&name)?,
        string(row.entity.as_str())?,
        Value::String(row.label),
        string(&join(&row.properties))?,
        string(&kinds)?,
        Value::Uint(row.indexed_rows),
        Value::Uint(row.drifted_rows),
        Value::Bool(answers_probes),
    ])
}

/// Render through the `SHOW INDEXES` helpers so the two surfaces agree.
///
/// A composite registration renders under the composite spelling even when it
/// happens to declare one property, because that is the spelling the catalog
/// gave it.
fn render_name(row: &PropertyIndexStatsRow) -> String {
    match (row.composite, row.properties.as_slice()) {
        (false, [property]) => {
            render_index_name(row.label.clone(), property.clone(), row.name.clone())
        }
        (_, properties) => {
            render_composite_index_name(row.label.clone(), properties, row.name.clone())
        }
    }
}

fn join(properties: &[DbString]) -> String {
    properties
        .iter()
        .map(DbString::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

fn string(value: &str) -> Result<Value, ProcedureError> {
    let string = db_string(value).map_err(|_| ProcedureError::Internal {
        detail: format!("string construction failed during {PROC_NAME}"),
    })?;
    Ok(Value::String(string))
}
