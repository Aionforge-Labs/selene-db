//! `selene.vector_index_stats` native built-in.
//!
//! Read-only graph-tier procedure exposing vector-index cardinality and memory
//! accounting. This stays on the implementation-defined `CALL selene.*` surface
//! instead of changing ISO catalog statement row shapes.

use selene_core::{DbString, Value, db_string};
use selene_graph::{HnswIndexConfig, IvfIndexConfig, VectorIndexKind, VectorIndexMemoryUsage};

use super::meta::{StaticOutputColumn, StaticParameter};
use crate::procedure_registry::ProcedureError;
use crate::{GqlType, GraphContext, ProcedureOutputColumn, ProcedureParameter, ProcedureResult};

const PROC_NAME: &str = "selene.vector_index_stats";

static VECTOR_INDEX_STATS_OUTPUTS: [StaticOutputColumn; 43] = [
    StaticOutputColumn::new("name", GqlType::String).with_description("Catalog index name."),
    StaticOutputColumn::new("label", GqlType::String).with_description("Indexed node label."),
    StaticOutputColumn::new("property", GqlType::String).with_description("Indexed property."),
    StaticOutputColumn::new("kind", GqlType::String).with_description("Vector index kind."),
    StaticOutputColumn::new("dimension", GqlType::Uint64)
        .with_description("Required vector dimensionality."),
    StaticOutputColumn::new("indexed_rows", GqlType::Uint64)
        .with_description("Live indexed row count."),
    StaticOutputColumn::new("row_bitmap_bytes", GqlType::Uint64)
        .with_description("Estimated row-bitmap heap bytes."),
    StaticOutputColumn::new("row_bitmap_serialized_bytes", GqlType::Uint64)
        .with_description("Serialized row-bitmap bytes."),
    StaticOutputColumn::new("hnsw_index_bytes", GqlType::Uint64)
        .with_description("Estimated HNSW-owned heap bytes."),
    StaticOutputColumn::new("hnsw_referenced_vector_bytes", GqlType::Uint64)
        .with_description("Vector component bytes reachable through HNSW entries."),
    StaticOutputColumn::new("hnsw_entries", GqlType::Uint64)
        .with_description("Total HNSW entries including stale entries."),
    StaticOutputColumn::new("hnsw_live_entries", GqlType::Uint64)
        .with_description("Live HNSW row entries."),
    StaticOutputColumn::new("hnsw_deleted_entries", GqlType::Uint64)
        .with_description("Stale deleted HNSW entries."),
    StaticOutputColumn::new("hnsw_link_count", GqlType::Uint64)
        .with_description("Stored directed HNSW links."),
    StaticOutputColumn::new("hnsw_level_zero_link_count", GqlType::Uint64)
        .with_description("Stored directed HNSW links in the level-0 layer."),
    StaticOutputColumn::new("hnsw_upper_layer_link_count", GqlType::Uint64)
        .with_description("Stored directed HNSW links above level 0."),
    StaticOutputColumn::new("hnsw_max_layer_count", GqlType::Uint64)
        .with_description("Maximum HNSW layer count attached to an entry."),
    StaticOutputColumn::new("hnsw_max_links_per_layer", GqlType::Uint64)
        .with_description("Maximum directed HNSW links stored in one entry layer."),
    StaticOutputColumn::new("hnsw_average_links_per_entry_basis_points", GqlType::Uint64)
        .with_description("Average directed HNSW links per entry scaled by 10,000."),
    StaticOutputColumn::new("ivf_index_bytes", GqlType::Uint64)
        .with_description("Estimated IVF-owned heap bytes."),
    StaticOutputColumn::new("ivf_referenced_vector_bytes", GqlType::Uint64)
        .with_description("Vector component bytes reachable through IVF entries."),
    StaticOutputColumn::new("ivf_entries", GqlType::Uint64)
        .with_description("Total IVF entries including stale entries."),
    StaticOutputColumn::new("ivf_live_entries", GqlType::Uint64)
        .with_description("Live IVF row entries."),
    StaticOutputColumn::new("ivf_deleted_entries", GqlType::Uint64)
        .with_description("Stale deleted IVF entries."),
    StaticOutputColumn::new("ivf_centroids", GqlType::Uint64)
        .with_description("Trained IVF centroid count."),
    StaticOutputColumn::new("ivf_list_count", GqlType::Uint64)
        .with_description("IVF inverted-list count."),
    StaticOutputColumn::new("ivf_non_empty_list_count", GqlType::Uint64)
        .with_description("IVF inverted lists with at least one assigned live entry."),
    StaticOutputColumn::new("ivf_max_list_len", GqlType::Uint64)
        .with_description("Maximum assigned live entries in one IVF inverted list."),
    StaticOutputColumn::new("ivf_average_list_len_basis_points", GqlType::Uint64)
        .with_description("Average IVF assigned entries per list scaled by 10,000."),
    StaticOutputColumn::new("ivf_assigned_entries", GqlType::Uint64)
        .with_description("Live IVF entries assigned to inverted lists."),
    StaticOutputColumn::new("ivf_pending_retrain_entries", GqlType::Uint64)
        .with_description("Live IVF entries inserted or replaced after centroid training."),
    StaticOutputColumn::new("ivf_pending_retrain_basis_points", GqlType::Uint64).with_description(
        "Pending IVF retrain entries divided by live IVF entries, scaled by 10,000.",
    ),
    StaticOutputColumn::new("ivf_rebuild_recommended", GqlType::Boolean)
        .with_description("Whether IVF drift diagnostics recommend a maintenance rebuild."),
    StaticOutputColumn::new("estimated_index_bytes", GqlType::Uint64)
        .with_description("Estimated index-owned bytes."),
    StaticOutputColumn::new("estimated_reachable_bytes", GqlType::Uint64)
        .with_description("Estimated bytes reachable from the index."),
    StaticOutputColumn::new("turbo_quant_index_bytes", GqlType::Uint64)
        .with_description("Estimated TurboQuant-owned heap bytes."),
    StaticOutputColumn::new("turbo_quant_referenced_vector_bytes", GqlType::Uint64)
        .with_description("Vector component bytes reachable through TurboQuant entries."),
    StaticOutputColumn::new("turbo_quant_entries", GqlType::Uint64)
        .with_description("Total TurboQuant entries including stale entries."),
    StaticOutputColumn::new("turbo_quant_live_entries", GqlType::Uint64)
        .with_description("Live TurboQuant row entries."),
    StaticOutputColumn::new("turbo_quant_deleted_entries", GqlType::Uint64)
        .with_description("Stale deleted TurboQuant entries."),
    StaticOutputColumn::new("turbo_quant_code_bytes", GqlType::Uint64)
        .with_description("Packed TurboQuant coordinate-code bytes."),
    StaticOutputColumn::new("turbo_quant_codebook_bytes", GqlType::Uint64)
        .with_description("TurboQuant scalar codebook bytes."),
    StaticOutputColumn::new("turbo_quant_calibration_bytes", GqlType::Uint64)
        .with_description("TurboQuant per-dimension calibration bytes."),
];

pub(super) fn signature() -> Vec<ProcedureParameter> {
    let params: [StaticParameter; 0] = [];
    params
        .into_iter()
        .map(StaticParameter::into_parameter)
        .collect()
}

pub(super) fn output_columns() -> Vec<ProcedureOutputColumn> {
    VECTOR_INDEX_STATS_OUTPUTS
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

    let snapshot = ctx.snapshot();
    let mut rows = snapshot
        // Physical diagnostics intentionally inspect retained backing regardless
        // of catalog query eligibility. The existing metadata-only entry methods
        // expose counts/configuration, not an unchecked query or row API.
        .vector_index.iter()
        .map(
            |((label, property), entry)| {
                let usage = entry.memory_usage();
                let name = render_vector_index_name(label.clone(), property.clone(), entry.name.clone());
                let dimension = entry.dimension();
                let kind = render_vector_index_kind(entry.kind(), dimension, entry.hnsw_config(), entry.ivf_config());
                StatsRow {
                    label: label.clone(),
                    property: property.clone(),
                    name,
                    kind,
                    dimension,
                    usage,
                }
            },
        )
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        left.label
            .as_str()
            .cmp(right.label.as_str())
            .then_with(|| left.property.as_str().cmp(right.property.as_str()))
            .then_with(|| left.kind.cmp(&right.kind))
    });

    let rows = rows
        .into_iter()
        .map(StatsRow::into_values)
        .collect::<Result<Vec<_>, ProcedureError>>()?;
    Ok(ProcedureResult { rows })
}

struct StatsRow {
    label: DbString,
    property: DbString,
    name: String,
    kind: String,
    dimension: u32,
    usage: VectorIndexMemoryUsage,
}

impl StatsRow {
    fn into_values(self) -> Result<Vec<Value>, ProcedureError> {
        Ok(vec![
            string(&self.name)?,
            Value::String(self.label),
            Value::String(self.property),
            string(&self.kind)?,
            Value::Uint(u64::from(self.dimension)),
            Value::Uint(self.usage.indexed_rows),
            Value::Uint(usize_to_u64_saturating(self.usage.row_bitmap_bytes)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.row_bitmap_serialized_bytes,
            )),
            Value::Uint(usize_to_u64_saturating(self.usage.hnsw_index_bytes)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.hnsw_referenced_vector_bytes,
            )),
            Value::Uint(usize_to_u64_saturating(self.usage.hnsw_entries)),
            Value::Uint(usize_to_u64_saturating(self.usage.hnsw_live_entries)),
            Value::Uint(usize_to_u64_saturating(self.usage.hnsw_deleted_entries)),
            Value::Uint(usize_to_u64_saturating(self.usage.hnsw_link_count)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.hnsw_level_zero_link_count,
            )),
            Value::Uint(usize_to_u64_saturating(
                self.usage.hnsw_upper_layer_link_count,
            )),
            Value::Uint(usize_to_u64_saturating(self.usage.hnsw_max_layer_count)),
            Value::Uint(usize_to_u64_saturating(self.usage.hnsw_max_links_per_layer)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.hnsw_average_links_per_entry_basis_points,
            )),
            Value::Uint(usize_to_u64_saturating(self.usage.ivf_index_bytes)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.ivf_referenced_vector_bytes,
            )),
            Value::Uint(usize_to_u64_saturating(self.usage.ivf_entries)),
            Value::Uint(usize_to_u64_saturating(self.usage.ivf_live_entries)),
            Value::Uint(usize_to_u64_saturating(self.usage.ivf_deleted_entries)),
            Value::Uint(usize_to_u64_saturating(self.usage.ivf_centroids)),
            Value::Uint(usize_to_u64_saturating(self.usage.ivf_list_count)),
            Value::Uint(usize_to_u64_saturating(self.usage.ivf_non_empty_list_count)),
            Value::Uint(usize_to_u64_saturating(self.usage.ivf_max_list_len)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.ivf_average_list_len_basis_points,
            )),
            Value::Uint(usize_to_u64_saturating(self.usage.ivf_assigned_entries)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.ivf_pending_retrain_entries,
            )),
            Value::Uint(usize_to_u64_saturating(
                self.usage.ivf_pending_retrain_basis_points(),
            )),
            Value::Bool(self.usage.ivf_rebuild_recommended()),
            Value::Uint(usize_to_u64_saturating(self.usage.estimated_index_bytes)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.estimated_reachable_bytes,
            )),
            Value::Uint(usize_to_u64_saturating(self.usage.turbo_quant_index_bytes)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.turbo_quant_referenced_vector_bytes,
            )),
            Value::Uint(usize_to_u64_saturating(self.usage.turbo_quant_entries)),
            Value::Uint(usize_to_u64_saturating(self.usage.turbo_quant_live_entries)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.turbo_quant_deleted_entries,
            )),
            Value::Uint(usize_to_u64_saturating(self.usage.turbo_quant_code_bytes)),
            Value::Uint(usize_to_u64_saturating(
                self.usage.turbo_quant_codebook_bytes,
            )),
            Value::Uint(usize_to_u64_saturating(
                self.usage.turbo_quant_calibration_bytes,
            )),
        ])
    }
}

fn render_vector_index_name(
    label: DbString,
    property: DbString,
    explicit: Option<DbString>,
) -> String {
    explicit
        .map(|name| name.as_str().to_owned())
        .unwrap_or_else(|| {
            selene_catalog::generated_index_name(
                selene_catalog::IndexFamily::Vector,
                label.as_str(),
                std::iter::once(property.as_str()),
            )
        })
}

fn render_vector_index_kind(
    kind: VectorIndexKind,
    dimension: u32,
    hnsw_config: Option<HnswIndexConfig>,
    ivf_config: Option<IvfIndexConfig>,
) -> String {
    match kind {
        VectorIndexKind::Flat => format!("vector_flat({dimension})"),
        VectorIndexKind::HnswSquaredEuclidean => {
            render_hnsw_kind("vector_hnsw_squared_euclidean", dimension, hnsw_config)
        }
        VectorIndexKind::HnswCosine => {
            render_hnsw_kind("vector_hnsw_cosine", dimension, hnsw_config)
        }
        VectorIndexKind::HnswNegativeInnerProduct => {
            render_hnsw_kind("vector_hnsw_negative_inner_product", dimension, hnsw_config)
        }
        VectorIndexKind::IvfSquaredEuclidean => {
            render_ivf_kind("vector_ivf_squared_euclidean", dimension, ivf_config)
        }
        VectorIndexKind::IvfCosine => render_ivf_kind("vector_ivf_cosine", dimension, ivf_config),
        VectorIndexKind::IvfNegativeInnerProduct => {
            render_ivf_kind("vector_ivf_negative_inner_product", dimension, ivf_config)
        }
        VectorIndexKind::TurboQuantCosine => format!("vector_turbo_quant_cosine({dimension})"),
    }
}

fn render_hnsw_kind(
    name: &'static str,
    dimension: u32,
    hnsw_config: Option<HnswIndexConfig>,
) -> String {
    let config = hnsw_config.unwrap_or_default();
    if config.is_default() {
        format!("{name}({dimension})")
    } else {
        format!(
            "{name}({dimension},m={},ef_construction={})",
            config.max_neighbors, config.ef_construction
        )
    }
}

fn render_ivf_kind(
    name: &'static str,
    dimension: u32,
    ivf_config: Option<IvfIndexConfig>,
) -> String {
    if let Some(config) = ivf_config {
        format!(
            "{name}({dimension},target_centroids={})",
            config.target_centroids
        )
    } else {
        format!("{name}({dimension})")
    }
}

fn usize_to_u64_saturating(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn string(value: &str) -> Result<Value, ProcedureError> {
    db_string(value)
        .map(Value::String)
        .map_err(|_err| ProcedureError::Internal {
            detail: "string construction failed during selene.vector_index_stats".to_owned(),
        })
}
