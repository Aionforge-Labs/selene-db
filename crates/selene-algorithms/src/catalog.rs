//! Named projection cache with generation-based staleness detection.
//!
//! [`ProjectionCatalog`] lets embedders amortize CSR build cost across
//! repeated algorithm runs against the same logical subgraph. Each entry pairs
//! a built [`GraphProjection`] with the [`ProjectionConfig`] that produced it,
//! so [`ProjectionCatalog::ensure_fresh`] can rebuild from the stored recipe
//! when the underlying graph generation advances.
//!
//! ## Catalog projections are always unscoped
//!
//! The stored [`ProjectionConfig`] is the *complete* rebuild recipe, and it
//! carries no scope bitmap. Accepting a scope at registration would silently
//! widen the projection on the first stale rebuild, so the catalog refuses
//! the parameter entirely: [`ProjectionCatalog::project`] always builds
//! unscoped (spec 16 §3 E06). Callers needing a scoped, point-in-time view
//! build one directly via [`GraphProjection::build`] and manage its lifetime
//! themselves.
//!
//! ## Concurrency
//!
//! The catalog is `Send + Sync`. Reads (`get`, `len`, `is_empty`, `contains`,
//! `names`) acquire a `parking_lot::RwLock` read guard; mutations (`project`,
//! `drop_projection`, and the rebuild branch of `ensure_fresh`) acquire the
//! write guard. [`ProjectionRef`] owns an `Arc` clone of the selected projection,
//! so the catalog guard is released before algorithm execution and catalog
//! mutations are not blocked by long-running readers (spec 16 §3 E07).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use selene_graph::{CandidateSet, Node, SeleneGraph};

use crate::error::AlgorithmsError;
use crate::projection::{GraphProjection, ProjectionConfig};

/// A cached projection paired with the config that produced it.
///
/// Storing the config alongside the projection lets `ensure_fresh` rebuild
/// from the original recipe without forcing callers to re-pass it.
#[derive(Debug)]
struct CatalogEntry {
    projection: Arc<GraphProjection>,
    config: ProjectionConfig,
    // An empty candidate set retains identity, generation and private workspace
    // binding without an O(nodes) freshness scan or any exposed physical rows.
    snapshot: CandidateSet<Node>,
}

impl CatalogEntry {
    fn is_current(&self, snapshot: &SeleneGraph) -> bool {
        // Graph-owned algebra validates both private identity tokens. The sets
        // are empty, so this performs no graph scan and exposes no row handles.
        snapshot
            .intersect_candidates(&self.snapshot, &self.snapshot)
            .is_ok()
    }
}

/// Named cache of [`GraphProjection`]s with generation-based staleness
/// detection.
///
/// Use [`ProjectionCatalog::project`] to register a projection under a name,
/// [`ProjectionCatalog::ensure_fresh`] to refresh it against the current
/// snapshot generation, and [`ProjectionCatalog::get`] for read-locked access.
#[derive(Debug)]
pub struct ProjectionCatalog {
    entries: RwLock<HashMap<String, CatalogEntry>>,
}

impl Default for ProjectionCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectionCatalog {
    /// Construct an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Build (or rebuild) a named projection from `config`.
    ///
    /// Returns `(node_count, edge_count)`. Overwrites any existing projection
    /// of the same name — equivalent to `drop_projection(&config.name)` then
    /// build, but in one atomic write-lock acquisition.
    ///
    /// The projection is always **unscoped**: `config` is the complete recipe
    /// that [`Self::ensure_fresh`] rebuilds from, and it cannot retain a scope
    /// bitmap across rebuilds (spec 16 §3 E06). Scoped views go through
    /// [`GraphProjection::build`] directly, outside the catalog.
    pub fn project(
        &self,
        snapshot: &SeleneGraph,
        config: &ProjectionConfig,
    ) -> Result<(usize, usize), AlgorithmsError> {
        let projection = GraphProjection::build(snapshot, config, None)?;
        let node_count = projection.node_count();
        let edge_count = projection.edge_count();
        self.entries.write().insert(
            config.name.clone(),
            CatalogEntry {
                projection: Arc::new(projection),
                config: config.clone(),
                snapshot: snapshot.bind_node_candidates([])?,
            },
        );
        Ok((node_count, edge_count))
    }

    /// Ensure the named projection exists and is fresh against `snapshot`.
    ///
    /// - If absent: returns [`AlgorithmsError::NoSuchProjection`].
    /// - If present and the full snapshot identity matches: no-op.
    /// - Otherwise: rebuild from the stored config, including for distinct
    ///   detached workspaces with equal numeric generations.
    ///   Because catalog projections are unscoped by construction (spec 16
    ///   §3 E06), the rebuild reproduces exactly what [`Self::project`]
    ///   registered, evaluated against the fresh snapshot.
    pub fn ensure_fresh(&self, snapshot: &SeleneGraph, name: &str) -> Result<(), AlgorithmsError> {
        self.resolve(snapshot, name).map(|_| ())
    }

    /// Resolve and pin a projection for exactly this immutable snapshot.
    ///
    /// Validation includes graph identity and private workspace/layout identity,
    /// not just a numeric generation (detached transactions can share one).
    /// The returned reference is captured under the same lock as validation, so
    /// another reader cannot substitute a different snapshot before it is pinned.
    pub fn resolve(
        &self,
        snapshot: &SeleneGraph,
        name: &str,
    ) -> Result<ProjectionRef, AlgorithmsError> {
        // Phase 1: fast-path read-lock check.
        {
            let guard = self.entries.read();
            match guard.get(name) {
                None => {
                    return Err(AlgorithmsError::NoSuchProjection {
                        name: name.to_string(),
                    });
                }
                Some(entry) if entry.is_current(snapshot) => {
                    return Ok(ProjectionRef {
                        projection: Arc::clone(&entry.projection),
                    });
                }
                Some(_) => {
                    // stale; fall through to rebuild under the write lock.
                }
            }
        }

        // Phase 2: write-lock rebuild path.
        let mut guard = self.entries.write();
        // Re-check existence + freshness under write lock: another writer
        // may have inserted (or refreshed) the entry between our Phase 1
        // read drop and the Phase 2 write acquisition.
        let entry = guard
            .get(name)
            .ok_or_else(|| AlgorithmsError::NoSuchProjection {
                name: name.to_string(),
            })?;
        if entry.is_current(snapshot) {
            return Ok(ProjectionRef {
                projection: Arc::clone(&entry.projection),
            });
        }
        let config = entry.config.clone();
        let projection = Arc::new(GraphProjection::build(snapshot, &config, None)?);
        guard.insert(
            name.to_string(),
            CatalogEntry {
                projection: Arc::clone(&projection),
                config,
                snapshot: snapshot.bind_node_candidates([])?,
            },
        );
        Ok(ProjectionRef { projection })
    }

    /// Access a named projection. Returns `None` when absent.
    ///
    /// The returned [`ProjectionRef`] owns an `Arc` clone of the projection, so
    /// the catalog read lock is released before the caller runs an algorithm or
    /// invokes another catalog operation.
    pub fn get(&self, name: &str) -> Option<ProjectionRef> {
        self.entries.read().get(name).map(|entry| ProjectionRef {
            projection: Arc::clone(&entry.projection),
        })
    }

    /// Remove the named projection. Returns `true` if it existed.
    pub fn drop_projection(&self, name: &str) -> bool {
        self.entries.write().remove(name).is_some()
    }

    /// Number of registered projections.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// True when no projections are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// True when the named projection is registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.read().contains_key(name)
    }

    /// Snapshot of all registered projection names. Returns an owned `Vec`
    /// so the read lock releases before the caller iterates.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.entries.read().keys().cloned().collect()
    }
}

/// Owned reference to a single projection in a [`ProjectionCatalog`].
pub struct ProjectionRef {
    projection: Arc<GraphProjection>,
}

impl std::fmt::Debug for ProjectionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionRef")
            .field("name", &self.projection.name())
            .finish_non_exhaustive()
    }
}

impl ProjectionRef {
    /// Borrow the underlying [`GraphProjection`].
    #[must_use]
    pub fn projection(&self) -> &GraphProjection {
        &self.projection
    }
}

impl std::ops::Deref for ProjectionRef {
    type Target = GraphProjection;
    fn deref(&self) -> &Self::Target {
        self.projection()
    }
}
