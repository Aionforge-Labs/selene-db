//! Engine-internal per-`GraphId` projection-catalog state.
//!
//! Ported from the historical procedure-pack per-instance catalog state.
//! The catalog is **engine-internal, ephemeral, and
//! per-`GraphId`**: projections are derived state, never persisted and never
//! part of snapshot/recovery. The map is keyed by [`GraphId`] so concurrent
//! algorithm runs against distinct graphs never serialize on each other; a
//! dropped graph's entry is reclaimed by [`AlgorithmCatalogs::forget_graph`].

use std::{collections::HashMap, sync::Arc};

use parking_lot::RwLock;
use selene_algorithms::{GraphProjection, ProjectionCatalog};
use selene_core::GraphId;
use selene_graph::SeleneGraph;

use super::error::algorithm_error;
use crate::ProcedureError;

/// Engine-lifetime native-algorithm projection catalogs, one per `GraphId`.
#[derive(Debug, Default)]
pub(crate) struct AlgorithmCatalogs {
    catalogs: RwLock<HashMap<GraphId, Arc<ProjectionCatalog>>>,
}

impl AlgorithmCatalogs {
    /// Run `f` against the projection catalog for `graph_id`, creating it on
    /// first use. The catalog `Arc` is cloned out of the map before `f` runs so
    /// the map lock is not held across projection work (matching the historical
    /// pack lock discipline — never hold the catalog map lock across a run).
    pub(super) fn with_catalog<R>(
        &self,
        graph_id: GraphId,
        f: impl FnOnce(&ProjectionCatalog) -> Result<R, ProcedureError>,
    ) -> Result<R, ProcedureError> {
        if let Some(catalog) = self.catalogs.read().get(&graph_id).cloned() {
            return f(catalog.as_ref());
        }

        let catalog = {
            let mut catalogs = self.catalogs.write();
            Arc::clone(
                catalogs
                    .entry(graph_id)
                    .or_insert_with(|| Arc::new(ProjectionCatalog::new())),
            )
        };
        f(catalog.as_ref())
    }

    /// Drop the projection catalog for a graph that is no longer live.
    ///
    /// Projections are ephemeral derived state; when a graph is dropped its
    /// catalog must be reclaimed so a later `GraphId` reuse cannot observe a
    /// stale projection. Returns `true` if an entry was present.
    pub(super) fn forget_graph(&self, graph_id: GraphId) -> bool {
        self.catalogs.write().remove(&graph_id).is_some()
    }
}

/// Resolve a fresh, pinned view of `projection_name` for `snapshot` from
/// the per-`GraphId` catalog and run `f` against it.
///
/// Validation and pinning share one catalog lock acquisition. The owned
/// projection reference releases that lock before running the algorithm.
/// The graph write lock MUST NOT be held across this call.
pub(super) fn with_projection<R>(
    catalogs: &AlgorithmCatalogs,
    snapshot: &SeleneGraph,
    projection_name: &str,
    f: impl FnOnce(&GraphProjection) -> Result<R, ProcedureError>,
) -> Result<R, ProcedureError> {
    catalogs.with_catalog(snapshot.graph_id(), |catalog| {
        let projection = catalog
            .resolve(snapshot, projection_name)
            .map_err(algorithm_error)?;
        f(projection.projection())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    use selene_core::GraphId;

    use super::*;

    #[test]
    fn with_catalog_runs_concurrently_against_different_graph_ids() {
        let state = Arc::new(AlgorithmCatalogs::default());
        let active = Arc::new(AtomicUsize::new(0));
        let max_observed = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(4));

        let mut handles = Vec::new();
        for raw_id in 1..=4 {
            let state = Arc::clone(&state);
            let active = Arc::clone(&active);
            let max_observed = Arc::clone(&max_observed);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                state
                    .with_catalog(GraphId::new(raw_id), |_catalog| {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_observed.fetch_max(now, Ordering::SeqCst);
                        barrier.wait();
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .expect("with_catalog closure succeeds");
            }));
        }

        for handle in handles {
            handle.join().expect("thread should not panic");
        }

        let observed = max_observed.load(Ordering::SeqCst);
        assert!(
            observed >= 2,
            "expected at least 2 with_catalog closures executing concurrently against different \
             GraphIds, observed max = {observed}",
        );
    }

    #[test]
    fn forget_graph_reclaims_only_the_target_entry() {
        let state = AlgorithmCatalogs::default();
        state
            .with_catalog(GraphId::new(1), |_| Ok(()))
            .expect("graph 1 catalog");
        state
            .with_catalog(GraphId::new(2), |_| Ok(()))
            .expect("graph 2 catalog");

        assert!(state.forget_graph(GraphId::new(1)));
        assert!(!state.forget_graph(GraphId::new(1)));
        assert!(state.forget_graph(GraphId::new(2)));
    }
}
