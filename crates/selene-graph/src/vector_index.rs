//! Built-in vector row-set indexes for node properties.
//!
//! Vector indexes are durable registrations plus derived in-memory accelerators
//! over primary node values. Every kind keeps a row bitmap for alive nodes whose
//! `(label, property)` value is a vector with the declared dimension; ANN kinds
//! also maintain a derived search accelerator. Registration and live
//! maintenance are strict so search cannot hide dimensionality or metric drift;
//! lower-layer recovery rebuild remains lenient for corrupted/legacy state, but
//! any skipped value makes the accelerator unavailable for query use. Facade
//! recovery is strict. Exact search always reads primary graph values.

use std::mem::size_of;

use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;
#[path = "vector_index/build.rs"]
mod build;
#[path = "vector_index/config.rs"]
mod config;
#[path = "vector_index/hnsw.rs"]
mod hnsw;
#[path = "vector_index/ivf.rs"]
mod ivf;
#[path = "vector_index/ivf_adapter.rs"]
mod ivf_adapter;
#[path = "vector_index/maintenance.rs"]
mod maintenance;
#[path = "vector_index/memory.rs"]
mod memory;
#[path = "vector_index/rebuild.rs"]
mod rebuild;
#[path = "vector_index/search_hit.rs"]
mod search_hit;
#[path = "vector_index/turbo_quant.rs"]
mod turbo_quant;
#[path = "vector_index/turbo_quant_adapter.rs"]
mod turbo_quant_adapter;

use selene_core::{DbString, HnswIndexConfig, IvfIndexConfig, VectorMetric, VectorValue};
use serde::{Deserialize, Serialize};

use crate::error::{GraphError, GraphResult};
use crate::graph::VectorIndexEntry;
pub(crate) use build::{
    build_vector_index_lenient_with_configs, build_vector_index_with_configs,
    maintain_vector_indexes_strict, rebuild_vector_indexes, rebuild_vector_indexes_strict,
};
use config::{hnsw_config_for_kind, ivf_config_for_kind};
pub(crate) use hnsw::HnswSearchScratch;
use hnsw::HnswVectorIndex;
use ivf::IvfVectorIndex;
pub use maintenance::VectorIndexValueError;
pub(crate) use maintenance::{apply_node_create, apply_node_delete, apply_node_update};
pub use memory::{
    IVF_REBUILD_MIN_PENDING_RETRAIN_ENTRIES, IVF_REBUILD_PENDING_RETRAIN_BASIS_POINTS,
    VectorIndexMemoryUsage,
};
pub use rebuild::{
    VectorIndexMaintenancePolicy, VectorIndexRebuildEntry, VectorIndexRebuildReport,
};
pub(crate) use search_hit::VectorIndexSearchHit;
use search_hit::{hnsw_hits, ivf_hits};
use turbo_quant::TurboQuantVectorIndex;

type VectorIndexMap = FxHashMap<(DbString, DbString), VectorIndexEntry>;

/// Vector index algorithm kind.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Serialize,
)]
pub enum VectorIndexKind {
    /// Exact row-set index over full-precision vectors stored on graph rows.
    Flat,
    /// Approximate HNSW index using squared Euclidean distance.
    HnswSquaredEuclidean,
    /// Approximate HNSW index using cosine distance.
    HnswCosine,
    /// Approximate HNSW index using negative inner product distance.
    HnswNegativeInnerProduct,
    /// Approximate IVF index using squared Euclidean distance.
    IvfSquaredEuclidean,
    /// Approximate IVF index using cosine distance.
    IvfCosine,
    /// Approximate IVF index using negative inner product distance.
    IvfNegativeInnerProduct,
    /// Compressed TurboQuant candidate index using cosine distance.
    TurboQuantCosine,
}

/// Optional ANN construction config for one vector-index registration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VectorIndexConfig {
    /// HNSW construction config for HNSW vector indexes.
    pub hnsw: Option<HnswIndexConfig>,
    /// IVF construction config for IVF vector indexes.
    pub ivf: Option<IvfIndexConfig>,
}

impl VectorIndexConfig {
    /// Construct a vector-index config from optional ANN-specific configs.
    #[must_use]
    pub const fn new(hnsw: Option<HnswIndexConfig>, ivf: Option<IvfIndexConfig>) -> Self {
        Self { hnsw, ivf }
    }

    /// Construct a vector-index config with HNSW settings only.
    #[must_use]
    pub const fn hnsw(config: HnswIndexConfig) -> Self {
        Self {
            hnsw: Some(config),
            ivf: None,
        }
    }

    /// Construct a vector-index config with IVF settings only.
    #[must_use]
    pub const fn ivf(config: IvfIndexConfig) -> Self {
        Self {
            hnsw: None,
            ivf: Some(config),
        }
    }
}

impl VectorIndexKind {
    /// Return the HNSW metric for approximate vector-index kinds.
    #[must_use]
    pub const fn hnsw_metric(self) -> Option<VectorMetric> {
        match self {
            Self::Flat => None,
            Self::HnswSquaredEuclidean => Some(VectorMetric::SquaredEuclidean),
            Self::HnswCosine => Some(VectorMetric::Cosine),
            Self::HnswNegativeInnerProduct => Some(VectorMetric::NegativeInnerProduct),
            Self::IvfSquaredEuclidean
            | Self::IvfCosine
            | Self::IvfNegativeInnerProduct
            | Self::TurboQuantCosine => None,
        }
    }

    /// Return the IVF metric for inverted-file vector-index kinds.
    #[must_use]
    pub const fn ivf_metric(self) -> Option<VectorMetric> {
        match self {
            Self::Flat
            | Self::HnswSquaredEuclidean
            | Self::HnswCosine
            | Self::HnswNegativeInnerProduct
            | Self::TurboQuantCosine => None,
            Self::IvfSquaredEuclidean => Some(VectorMetric::SquaredEuclidean),
            Self::IvfCosine => Some(VectorMetric::Cosine),
            Self::IvfNegativeInnerProduct => Some(VectorMetric::NegativeInnerProduct),
        }
    }

    /// Return the ANN metric for approximate vector-index kinds.
    #[must_use]
    pub const fn ann_metric(self) -> Option<VectorMetric> {
        match self {
            Self::Flat => None,
            Self::HnswSquaredEuclidean | Self::IvfSquaredEuclidean => {
                Some(VectorMetric::SquaredEuclidean)
            }
            Self::HnswCosine | Self::IvfCosine => Some(VectorMetric::Cosine),
            Self::HnswNegativeInnerProduct | Self::IvfNegativeInnerProduct => {
                Some(VectorMetric::NegativeInnerProduct)
            }
            Self::TurboQuantCosine => Some(VectorMetric::Cosine),
        }
    }
}

/// Built-in vector index state for one `(label, property)` registration.
#[derive(Clone, Debug)]
pub struct VectorIndex {
    kind: VectorIndexKind,
    dimension: u32,
    complete: bool,
    hnsw_config: Option<HnswIndexConfig>,
    ivf_config: Option<IvfIndexConfig>,
    rows: RoaringBitmap,
    hnsw: Option<HnswVectorIndex>,
    ivf: Option<IvfVectorIndex>,
    turbo_quant: Option<TurboQuantVectorIndex>,
}

impl VectorIndex {
    /// Construct an empty vector index.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::VectorIndexInvalidDimension`] when `dimension` is
    /// zero.
    pub fn new(kind: VectorIndexKind, dimension: u32) -> GraphResult<Self> {
        Self::new_with_configs(kind, dimension, None, None)
    }

    /// Construct an empty vector index with optional HNSW configuration.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::VectorIndexInvalidDimension`] when `dimension` is
    /// zero, or [`GraphError::VectorIndexInvalidHnswConfig`] when the supplied
    /// HNSW parameters are invalid for the chosen kind.
    pub fn new_with_hnsw_config(
        kind: VectorIndexKind,
        dimension: u32,
        hnsw_config: Option<HnswIndexConfig>,
    ) -> GraphResult<Self> {
        Self::new_with_configs(kind, dimension, hnsw_config, None)
    }

    /// Construct an empty vector index with optional ANN construction config.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::VectorIndexInvalidDimension`] when `dimension` is
    /// zero, [`GraphError::VectorIndexInvalidHnswConfig`] when supplied HNSW
    /// parameters are invalid for the chosen kind, or
    /// [`GraphError::VectorIndexInvalidIvfConfig`] when supplied IVF parameters
    /// are invalid for the chosen kind.
    pub fn new_with_configs(
        kind: VectorIndexKind,
        dimension: u32,
        hnsw_config: Option<HnswIndexConfig>,
        ivf_config: Option<IvfIndexConfig>,
    ) -> GraphResult<Self> {
        ensure_dimension(dimension)?;
        let hnsw_config = hnsw_config_for_kind(kind, hnsw_config)?;
        let ivf_config = ivf_config_for_kind(kind, ivf_config)?;
        let hnsw = kind.hnsw_metric().map(|metric| {
            HnswVectorIndex::with_config(metric, hnsw_config.expect("HNSW kind stores config"))
        });
        let ivf = kind
            .ivf_metric()
            .map(|metric| IvfVectorIndex::with_config(metric, ivf_config));
        let turbo_quant = match kind {
            VectorIndexKind::TurboQuantCosine => Some(TurboQuantVectorIndex::new(dimension)?),
            _ => None,
        };
        Ok(Self {
            kind,
            dimension,
            complete: true,
            hnsw_config,
            ivf_config,
            rows: RoaringBitmap::new(),
            hnsw,
            ivf,
            turbo_quant,
        })
    }

    /// Return the vector index algorithm kind.
    #[must_use]
    pub const fn kind(&self) -> VectorIndexKind {
        self.kind
    }

    /// Return the required vector dimensionality.
    #[must_use]
    pub const fn dimension(&self) -> u32 {
        self.dimension
    }

    pub(crate) const fn is_complete(&self) -> bool {
        self.complete
    }

    /// Return the HNSW construction config, if this is an HNSW index.
    #[must_use]
    pub const fn hnsw_config(&self) -> Option<HnswIndexConfig> {
        self.hnsw_config
    }

    /// Return the IVF construction config, if this is a configured IVF index.
    #[must_use]
    pub const fn ivf_config(&self) -> Option<IvfIndexConfig> {
        self.ivf_config
    }

    /// Return the number of indexed rows.
    #[must_use]
    pub fn cardinality(&self) -> u64 {
        self.rows.len()
    }

    /// Borrow the indexed row bitmap.
    #[must_use]
    pub const fn rows(&self) -> &RoaringBitmap {
        &self.rows
    }

    /// Return true when this index has an ANN graph.
    #[must_use]
    pub const fn is_hnsw(&self) -> bool {
        self.kind.hnsw_metric().is_some()
    }

    /// Return true when this index has an IVF accelerator.
    #[must_use]
    pub const fn is_ivf(&self) -> bool {
        self.kind.ivf_metric().is_some()
    }

    /// Return true when this index has a TurboQuant compressed accelerator.
    #[must_use]
    pub const fn is_turbo_quant(&self) -> bool {
        matches!(self.kind, VectorIndexKind::TurboQuantCosine)
    }

    /// Return the HNSW metric, if this is an HNSW index.
    #[must_use]
    pub const fn hnsw_metric(&self) -> Option<VectorMetric> {
        self.kind.hnsw_metric()
    }

    /// Return the ANN metric, if this is an approximate index.
    #[must_use]
    pub const fn ann_metric(&self) -> Option<VectorMetric> {
        self.kind.ann_metric()
    }

    /// Return an estimated memory usage snapshot for this index.
    #[must_use]
    pub fn memory_usage(&self) -> VectorIndexMemoryUsage {
        let row_bitmap_bytes = roaring_heap_bytes(&self.rows);
        let row_bitmap_serialized_bytes = self.rows.serialized_size();
        let hnsw = self
            .hnsw
            .as_ref()
            .map(HnswVectorIndex::memory_usage)
            .unwrap_or_default();
        let ivf = self
            .ivf
            .as_ref()
            .map(IvfVectorIndex::memory_usage)
            .unwrap_or_default();
        let turbo_quant = self
            .turbo_quant
            .as_ref()
            .map(TurboQuantVectorIndex::memory_usage)
            .unwrap_or_default();
        let estimated_index_bytes = size_of::<Self>()
            .saturating_add(row_bitmap_bytes)
            .saturating_add(hnsw.estimated_heap_bytes)
            .saturating_add(ivf.estimated_heap_bytes)
            .saturating_add(turbo_quant.estimated_heap_bytes);
        VectorIndexMemoryUsage {
            indexed_rows: self.cardinality(),
            row_bitmap_bytes,
            row_bitmap_serialized_bytes,
            hnsw_index_bytes: hnsw.estimated_heap_bytes,
            hnsw_referenced_vector_bytes: hnsw.referenced_vector_bytes,
            hnsw_entries: hnsw.entries,
            hnsw_live_entries: hnsw.live_entries,
            hnsw_deleted_entries: hnsw.deleted_entries,
            hnsw_link_count: hnsw.link_count,
            hnsw_level_zero_link_count: hnsw.level_zero_link_count,
            hnsw_upper_layer_link_count: hnsw.upper_layer_link_count,
            hnsw_max_layer_count: hnsw.max_layer_count,
            hnsw_max_links_per_layer: hnsw.max_links_per_layer,
            hnsw_average_links_per_entry_basis_points: hnsw.average_links_per_entry_basis_points,
            ivf_index_bytes: ivf.estimated_heap_bytes,
            ivf_referenced_vector_bytes: ivf.referenced_vector_bytes,
            ivf_entries: ivf.entries,
            ivf_live_entries: ivf.live_entries,
            ivf_deleted_entries: ivf.deleted_entries,
            ivf_centroids: ivf.centroids,
            ivf_list_count: ivf.list_count,
            ivf_non_empty_list_count: ivf.non_empty_list_count,
            ivf_max_list_len: ivf.max_list_len,
            ivf_average_list_len_basis_points: ivf.average_list_len_basis_points,
            ivf_assigned_entries: ivf.assigned_entries,
            ivf_pending_retrain_entries: ivf.pending_retrain_entries,
            turbo_quant_index_bytes: turbo_quant.estimated_heap_bytes,
            turbo_quant_referenced_vector_bytes: turbo_quant.referenced_vector_bytes,
            turbo_quant_entries: turbo_quant.entries,
            turbo_quant_live_entries: turbo_quant.live_entries,
            turbo_quant_deleted_entries: turbo_quant.deleted_entries,
            turbo_quant_code_bytes: turbo_quant.code_bytes,
            turbo_quant_codebook_bytes: turbo_quant.codebook_bytes,
            turbo_quant_calibration_bytes: turbo_quant.calibration_bytes,
            estimated_index_bytes,
            estimated_reachable_bytes: estimated_index_bytes
                .saturating_add(hnsw.referenced_vector_bytes)
                .saturating_add(ivf.referenced_vector_bytes)
                .saturating_add(turbo_quant.referenced_vector_bytes),
        }
    }

    pub(crate) fn insert_value(&mut self, row: u32, vector: &VectorValue) -> GraphResult<()> {
        let mut scratch = HnswSearchScratch::default();
        self.insert_value_with_scratch(row, vector, &mut scratch)
    }

    pub(crate) fn insert_value_with_scratch(
        &mut self,
        row: u32,
        vector: &VectorValue,
        scratch: &mut HnswSearchScratch,
    ) -> GraphResult<()> {
        self.rows.insert(row);
        if let Some(hnsw) = &mut self.hnsw {
            hnsw.insert_with_scratch(row, vector.clone(), scratch)?;
        }
        if let Some(ivf) = &mut self.ivf {
            ivf.insert(row, vector.clone())?;
        }
        if let Some(turbo_quant) = &mut self.turbo_quant {
            turbo_quant.insert(row, vector)?;
        }
        Ok(())
    }

    pub(crate) fn finish_bulk_load(&mut self) -> GraphResult<()> {
        if let Some(hnsw) = &mut self.hnsw {
            hnsw.finish_bulk_load();
        }
        if let Some(ivf) = &mut self.ivf {
            ivf.finish_bulk_load()?;
        }
        if let Some(turbo_quant) = &mut self.turbo_quant {
            turbo_quant.finish_bulk_load()?;
        }
        Ok(())
    }

    pub(crate) fn remove_row(&mut self, row: u32) {
        self.rows.remove(row);
        if let Some(hnsw) = &mut self.hnsw {
            hnsw.remove(row);
        }
        if let Some(ivf) = &mut self.ivf {
            ivf.remove(row);
        }
        if let Some(turbo_quant) = &mut self.turbo_quant {
            turbo_quant.remove(row);
        }
    }

    pub(crate) fn rows_eq(&self, reference: &Self) -> bool {
        self.kind == reference.kind
            && self.dimension == reference.dimension
            && self.hnsw_config == reference.hnsw_config
            && self.ivf_config == reference.ivf_config
            && self.rows == reference.rows
    }

    pub(crate) fn ann_search(
        &self,
        query: &VectorValue,
        k: usize,
        search_width: usize,
    ) -> Option<selene_core::CoreResult<Vec<VectorIndexSearchHit>>> {
        if let Some(hnsw) = &self.hnsw {
            return Some(hnsw.search(query, k, search_width).map(hnsw_hits));
        }
        if let Some(ivf) = &self.ivf {
            return Some(ivf.search(query, k, search_width).map(ivf_hits));
        }
        None
    }

    pub(crate) fn ann_search_with_scratch(
        &self,
        query: &VectorValue,
        k: usize,
        search_width: usize,
        scratch: &mut HnswSearchScratch,
    ) -> Option<selene_core::CoreResult<Vec<VectorIndexSearchHit>>> {
        if let Some(hnsw) = &self.hnsw {
            return Some(
                hnsw.search_with_scratch(query, k, search_width, scratch)
                    .map(hnsw_hits),
            );
        }
        if let Some(ivf) = &self.ivf {
            return Some(ivf.search(query, k, search_width).map(ivf_hits));
        }
        None
    }

    pub(crate) fn ann_search_in_rows_with_scratch(
        &self,
        query: &VectorValue,
        k: usize,
        search_width: usize,
        allowed_rows: &RoaringBitmap,
        scratch: &mut HnswSearchScratch,
    ) -> Option<selene_core::CoreResult<Vec<VectorIndexSearchHit>>> {
        if let Some(hnsw) = &self.hnsw {
            return Some(
                hnsw.search_in_rows_with_scratch(query, k, search_width, allowed_rows, scratch)
                    .map(hnsw_hits),
            );
        }
        if let Some(ivf) = &self.ivf {
            return Some(
                ivf.search_in_rows(query, k, search_width, allowed_rows)
                    .map(ivf_hits),
            );
        }
        None
    }
}

fn roaring_heap_bytes(rows: &RoaringBitmap) -> usize {
    let statistics = rows.statistics();
    u64_to_usize_saturating(
        statistics
            .n_bytes_array_containers
            .saturating_add(statistics.n_bytes_run_containers)
            .saturating_add(statistics.n_bytes_bitset_containers),
    )
}

fn ensure_dimension(dimension: u32) -> GraphResult<()> {
    if dimension == 0 || dimension as usize > selene_core::MAX_VECTOR_DIMENSION {
        Err(GraphError::VectorIndexInvalidDimension { dimension })
    } else {
        Ok(())
    }
}

fn u64_to_usize_saturating(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(test)]
#[path = "vector_index/config_tests.rs"]
mod config_tests;

#[cfg(test)]
#[path = "vector_index/tests.rs"]
mod tests;
