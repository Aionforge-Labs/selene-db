//! Vector-index build and rebuild helpers.

use selene_core::DbString;

use crate::error::{GraphError, GraphResult};
use crate::graph::VectorIndexEntry;

use super::{
    VectorIndex, VectorIndexConfig, VectorIndexKind, VectorIndexMaintenancePolicy, VectorIndexMap,
    VectorIndexMemoryUsage, VectorIndexRebuildEntry, VectorIndexRebuildReport,
    hnsw::HnswSearchScratch,
    maintenance::{admit, index_rejection, is_null, warn_rejected},
};

struct VectorIndexRegistration {
    label: DbString,
    property: DbString,
    kind: VectorIndexKind,
    dimension: u32,
    config: VectorIndexConfig,
    name: Option<DbString>,
    before: VectorIndexMemoryUsage,
}

/// Build a vector index strictly with optional ANN construction config.
pub(crate) fn build_vector_index_with_configs(
    graph: &crate::SeleneGraph,
    label: DbString,
    property: DbString,
    kind: VectorIndexKind,
    dimension: u32,
    config: VectorIndexConfig,
) -> GraphResult<VectorIndex> {
    build_vector_index_inner(
        graph,
        label,
        property,
        kind,
        dimension,
        config,
        BuildPolicy::Strict,
    )
}

/// Build a vector index leniently with optional ANN construction config.
pub(crate) fn build_vector_index_lenient_with_configs(
    graph: &crate::SeleneGraph,
    label: DbString,
    property: DbString,
    kind: VectorIndexKind,
    dimension: u32,
    config: VectorIndexConfig,
) -> GraphResult<VectorIndex> {
    build_vector_index_inner(
        graph,
        label,
        property,
        kind,
        dimension,
        config,
        BuildPolicy::Lenient,
    )
}

/// Rebuild every registered vector index from node columns.
pub(crate) fn rebuild_vector_indexes(graph: &mut crate::SeleneGraph) -> GraphResult<()> {
    rebuild_vector_indexes_inner(graph, BuildPolicy::Lenient, RebuildSelection::All).map(|_| ())
}

/// Strictly rebuild every registered vector index from node columns.
pub(crate) fn rebuild_vector_indexes_strict(
    graph: &mut crate::SeleneGraph,
) -> GraphResult<VectorIndexRebuildReport> {
    rebuild_vector_indexes_inner(graph, BuildPolicy::Strict, RebuildSelection::All)
}

/// Strictly maintain vector indexes under a caller-supplied policy.
pub(crate) fn maintain_vector_indexes_strict(
    graph: &mut crate::SeleneGraph,
    policy: VectorIndexMaintenancePolicy,
) -> GraphResult<VectorIndexRebuildReport> {
    rebuild_vector_indexes_inner(
        graph,
        BuildPolicy::Strict,
        RebuildSelection::Recommended(policy),
    )
}

fn rebuild_vector_indexes_inner(
    graph: &mut crate::SeleneGraph,
    policy: BuildPolicy,
    selection: RebuildSelection,
) -> GraphResult<VectorIndexRebuildReport> {
    let mut registrations: Vec<VectorIndexRegistration> = graph
        .vector_index
        .iter()
        .map(|((label, property), entry)| VectorIndexRegistration {
            label: label.clone(),
            property: property.clone(),
            kind: entry.kind(),
            dimension: entry.dimension(),
            config: VectorIndexConfig::new(entry.hnsw_config(), entry.ivf_config()),
            name: entry.name.clone(),
            before: entry.memory_usage(),
        })
        .collect();
    selection.filter_and_order(&mut registrations);
    let mut rebuilt = match selection {
        RebuildSelection::All => VectorIndexMap::default(),
        RebuildSelection::Recommended(_) => graph.vector_index.clone(),
    };
    let mut entries = Vec::with_capacity(registrations.len());
    for registration in registrations {
        let index = build_vector_index_inner(
            graph,
            registration.label.clone(),
            registration.property.clone(),
            registration.kind,
            registration.dimension,
            registration.config,
            policy,
        )?;
        let after = index.memory_usage();
        let key = (registration.label.clone(), registration.property.clone());
        rebuilt.insert(key, VectorIndexEntry::new(index, registration.name.clone()));
        entries.push(VectorIndexRebuildEntry {
            label: registration.label,
            property: registration.property,
            name: registration.name,
            kind: registration.kind,
            dimension: registration.dimension,
            hnsw_config: registration.config.hnsw,
            ivf_config: registration.config.ivf,
            before: registration.before,
            after,
        });
    }
    graph.vector_index = rebuilt;
    Ok(VectorIndexRebuildReport::new(entries))
}

fn build_vector_index_inner(
    graph: &crate::SeleneGraph,
    label: DbString,
    property: DbString,
    kind: VectorIndexKind,
    dimension: u32,
    config: VectorIndexConfig,
    policy: BuildPolicy,
) -> GraphResult<VectorIndex> {
    let mut index = VectorIndex::new_with_configs(kind, dimension, config.hnsw, config.ivf)?;
    let mut hnsw_scratch = HnswSearchScratch::default();
    for row_index in 0..graph.node_store.labels.len() {
        let row = u32::try_from(row_index).map_err(|_| GraphError::Inconsistent {
            reason: format!(
                "node store row index {row_index} exceeds u32::MAX; selene-graph caps rows at u32::MAX"
            ),
        })?;
        if !graph.node_store.is_alive(row) {
            continue;
        }
        let Some(labels) = graph.node_store.labels.get(row_index) else {
            continue;
        };
        if !labels.contains(&label) {
            continue;
        }
        let Some(props) = graph.node_store.properties.get(row_index) else {
            continue;
        };
        let Some(value) = props.get(&property) else {
            continue;
        };
        if is_null(value) {
            continue;
        }
        match admit(value, kind, dimension) {
            Ok(vector) => {
                if let Err(err) = index.insert_value_with_scratch(row, vector, &mut hnsw_scratch) {
                    match policy {
                        BuildPolicy::Strict => return Err(err),
                        BuildPolicy::Lenient => {
                            index.complete = false;
                            tracing::warn!(
                                row,
                                error = %err,
                                "skipped vector-index HNSW update during lenient rebuild"
                            );
                        }
                    }
                }
            }
            Err(err) => match policy {
                BuildPolicy::Strict => {
                    return Err(index_rejection(
                        label.clone(),
                        property.clone(),
                        dimension,
                        err,
                    ));
                }
                BuildPolicy::Lenient => {
                    index.complete = false;
                    warn_rejected("rebuild", label.clone(), property.clone(), row, &err);
                }
            },
        }
    }
    index.finish_bulk_load()?;
    Ok(index)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildPolicy {
    Strict,
    Lenient,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RebuildSelection {
    All,
    Recommended(VectorIndexMaintenancePolicy),
}

impl RebuildSelection {
    fn filter_and_order(self, registrations: &mut Vec<VectorIndexRegistration>) {
        if let Self::Recommended(policy) = self {
            registrations.retain(|registration| registration.before.ivf_rebuild_recommended());
            registrations.sort_by(compare_recommended_registrations);
            if let Some(max) = policy.max_indexes_per_run {
                registrations.truncate(max.get());
            }
        }
    }
}

fn compare_recommended_registrations(
    left: &VectorIndexRegistration,
    right: &VectorIndexRegistration,
) -> std::cmp::Ordering {
    right
        .before
        .ivf_pending_retrain_basis_points()
        .cmp(&left.before.ivf_pending_retrain_basis_points())
        .then_with(|| {
            right
                .before
                .ivf_pending_retrain_entries
                .cmp(&left.before.ivf_pending_retrain_entries)
        })
        .then_with(|| left.label.as_str().cmp(right.label.as_str()))
        .then_with(|| left.property.as_str().cmp(right.property.as_str()))
}
