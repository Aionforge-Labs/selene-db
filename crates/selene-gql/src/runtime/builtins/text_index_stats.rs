//! `selene.text_index_stats` native built-in.
//!
//! Read-only graph-tier procedure exposing maintained BM25 text-index
//! cardinality and memory accounting.

use std::mem::size_of;

use selene_core::{DbString, Value, db_string};
use selene_graph::{TextIndexMemoryUsage, TextIndexStats};

use super::meta::{StaticOutputColumn, StaticParameter};
use crate::procedure_registry::ProcedureError;
use crate::{GqlType, GraphContext, ProcedureOutputColumn, ProcedureParameter, ProcedureResult};

const PROC_NAME: &str = "selene.text_index_stats";

static TEXT_INDEX_STATS_OUTPUTS: [StaticOutputColumn; 16] = [
    StaticOutputColumn::new("name", GqlType::String).with_description("Catalog index name."),
    StaticOutputColumn::new("label", GqlType::String).with_description("Indexed node label."),
    StaticOutputColumn::new("property", GqlType::String).with_description("Indexed property."),
    StaticOutputColumn::new("indexed_rows", GqlType::Uint64)
        .with_description("Live indexed row count."),
    StaticOutputColumn::new("documents", GqlType::Uint64)
        .with_description("String documents with at least one token."),
    StaticOutputColumn::new("distinct_terms", GqlType::Uint64)
        .with_description("Distinct indexed terms."),
    StaticOutputColumn::new("postings", GqlType::Uint64)
        .with_description("Term-document postings."),
    StaticOutputColumn::new("total_document_len", GqlType::Uint64)
        .with_description("Total indexed token count."),
    StaticOutputColumn::new("row_bitmap_bytes", GqlType::Uint64)
        .with_description("Estimated row-bitmap heap bytes."),
    StaticOutputColumn::new("row_bitmap_serialized_bytes", GqlType::Uint64)
        .with_description("Serialized row-bitmap bytes."),
    StaticOutputColumn::new("document_length_bytes", GqlType::Uint64)
        .with_description("Estimated document-length map bytes."),
    StaticOutputColumn::new("document_term_bytes", GqlType::Uint64)
        .with_description("Estimated commit-maintenance document-term bytes."),
    StaticOutputColumn::new("terms_table_bytes", GqlType::Uint64)
        .with_description("Estimated postings hash-table bytes."),
    StaticOutputColumn::new("term_bytes", GqlType::Uint64)
        .with_description("Estimated indexed term string bytes."),
    StaticOutputColumn::new("posting_bytes", GqlType::Uint64)
        .with_description("Estimated posting vector bytes."),
    StaticOutputColumn::new("estimated_index_bytes", GqlType::Uint64)
        .with_description("Estimated index-owned bytes."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    let params: [StaticParameter; 0] = [];
    params
        .into_iter()
        .map(StaticParameter::into_parameter)
        .collect()
}

pub(super) fn output_columns() -> Vec<ProcedureOutputColumn> {
    TEXT_INDEX_STATS_OUTPUTS
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
        .iter_text_index_entries()
        .map(|(label, property, stats, usage, explicit_name)| StatsRow {
            name: render_text_index_name(label.clone(), property.clone(), explicit_name),
            label,
            property,
            stats,
            usage,
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        left.label
            .as_str()
            .cmp(right.label.as_str())
            .then_with(|| left.property.as_str().cmp(right.property.as_str()))
    });

    let rows = rows
        .into_iter()
        .map(StatsRow::into_values)
        .collect::<Result<Vec<_>, ProcedureError>>()?;
    Ok(ProcedureResult { rows })
}

struct StatsRow {
    name: String,
    label: DbString,
    property: DbString,
    stats: TextIndexStats,
    usage: TextIndexMemoryUsage,
}

impl StatsRow {
    fn into_values(self) -> Result<Vec<Value>, ProcedureError> {
        Ok(vec![
            string(&self.name)?,
            Value::String(self.label),
            Value::String(self.property),
            Value::Uint(self.stats.indexed_rows),
            Value::Uint(usize_to_u64_saturating(self.stats.documents)),
            Value::Uint(usize_to_u64_saturating(self.stats.distinct_terms)),
            Value::Uint(usize_to_u64_saturating(self.stats.postings)),
            Value::Uint(self.stats.total_document_len),
            Value::Uint(usize_to_u64_saturating(self.usage.row_bitmap_bytes)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.row_bitmap_serialized_bytes,
            )),
            Value::Uint(usize_to_u64_saturating(self.usage.document_length_bytes)),
            Value::Uint(usize_to_u64_saturating(self.usage.document_term_bytes)),
            Value::Uint(usize_to_u64_saturating(self.usage.terms_table_bytes)),
            Value::Uint(usize_to_u64_saturating(self.usage.term_bytes)),
            Value::Uint(usize_to_u64_saturating(self.usage.posting_bytes)),
            Value::Uint(usize_to_u64_saturating(self.usage.estimated_index_bytes)),
        ])
    }
}

fn render_text_index_name(
    label: DbString,
    property: DbString,
    explicit: Option<DbString>,
) -> String {
    explicit
        .map(|name| name.as_str().to_owned())
        .unwrap_or_else(|| {
            selene_catalog::generated_index_name(
                selene_catalog::IndexFamily::Text,
                label.as_str(),
                std::iter::once(property.as_str()),
            )
        })
}

fn string(value: &str) -> Result<Value, ProcedureError> {
    let string = db_string(value).map_err(|_| ProcedureError::Internal {
        detail: "string construction failed during selene.text_index_stats".to_owned(),
    })?;
    Ok(Value::String(string))
}

const fn usize_to_u64_saturating(value: usize) -> u64 {
    if size_of::<usize>() <= size_of::<u64>() {
        value as u64
    } else if value > u64::MAX as usize {
        u64::MAX
    } else {
        value as u64
    }
}
