//! Database ownership, immutable outer state, and construction.

use std::{
    collections::BTreeMap,
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use arc_swap::ArcSwap;
#[cfg(test)]
use parking_lot::Mutex;
use parking_lot::RwLock;
use selene_catalog::{
    CatalogDescriptor, CatalogGeneration, CatalogId, CatalogName, CatalogSnapshot,
    CatalogSnapshotBuilder, CreationMetadata, DirectoryId, GraphId, GraphTypeId,
};
use selene_gql::BuiltinProcedureRegistry;
use selene_graph::{GraphTypeDef, SharedGraph};

use crate::{
    AuthorizationDecision, AuthorizationRequest, Catalog, CatalogReadSnapshot, DatabaseConfig,
    DatabaseId, Error, GraphDescriptor, ObjectPath, Principal, Result, SchemaDescriptor, Session,
    SessionContext, SessionOptions,
    session_context::SessionContextParts,
    transaction::{MutationCoordinator, TransactionId},
};

const CATALOG_NAME: &str = "selene";

/// Embedded database ownership root.
///
/// Cloning a database clones its internal [`Arc`]. Sessions clone the same
/// ownership root and remain usable after every `Database` handle is dropped.
#[derive(Clone)]
pub struct Database {
    pub(crate) inner: Arc<DatabaseInner>,
}

impl Database {
    /// Start an in-memory database build with the default configuration.
    #[must_use]
    pub fn builder() -> DatabaseBuilder {
        DatabaseBuilder::new()
    }

    /// Open a movable, lifetime-free session selected to the graph at `path`.
    ///
    /// The session records the current stable graph identity. Dropping or
    /// replacing that graph invalidates the session, including when the same
    /// path is recreated later.
    ///
    /// # Errors
    ///
    /// Returns a structured catalog error when the path, its parents, or its
    /// object kind do not identify a current graph.
    pub fn session(&self, path: &ObjectPath) -> Result<Session> {
        self.session_with_options(path, SessionOptions::default())
    }

    /// Open a session with embedder-provided principal and policy hooks.
    ///
    /// Principal resolution runs before one catalog snapshot is loaded. Current
    /// and home references are copied from that snapshot, which is released
    /// before policy evaluation. Neither hook runs under a catalog lifecycle
    /// lock or graph request lease.
    ///
    /// # Errors
    ///
    /// Returns a structured facade diagnostic for principal resolution,
    /// authorization denial, invalid home declarations, or an invalid current
    /// graph reference.
    pub fn session_with_options(
        &self,
        path: &ObjectPath,
        options: SessionOptions,
    ) -> Result<Session> {
        let SessionOptions {
            authorization_id,
            principal_provider,
            authorization_policy,
        } = options;
        let principal = match authorization_id.as_ref() {
            Some(id) => principal_provider
                .resolve(id)
                .map_err(Error::principal_provider_failure)?
                .ok_or_else(Error::principal_not_found)
                .map(Some)?,
            None => None,
        };

        let snapshot = self.catalog().snapshot();
        let current_graph = snapshot.resolve_graph(path)?;
        let current_schema = snapshot
            .resolve_schema(&current_graph.path.schema_path())
            .map_err(Error::invalid_session_reference)?;
        let (home_schema, home_graph) = resolve_principal_homes(&snapshot, principal.as_ref())?;
        let catalog_generation = snapshot.generation();
        drop(snapshot);

        let request = AuthorizationRequest::new(
            authorization_id.as_ref(),
            principal.as_ref(),
            home_schema.as_ref(),
            home_graph.as_ref(),
            &current_schema,
            &current_graph,
        );
        match authorization_policy
            .authorize(&request)
            .map_err(Error::authorization_policy_failure)?
        {
            AuthorizationDecision::Allow => {}
            AuthorizationDecision::Deny => return Err(Error::authorization_denied()),
        }

        let context = SessionContext::new(SessionContextParts {
            authorization_id,
            principal,
            home_schema,
            home_graph,
            current_schema,
            current_graph,
            catalog_generation,
        });
        Ok(Session::new(Arc::clone(&self.inner), context))
    }

    /// Open the database-owned catalog lifecycle service.
    #[must_use]
    pub fn catalog(&self) -> Catalog {
        Catalog::new(Arc::clone(&self.inner))
    }

    /// Borrow the configuration used to build this database.
    #[must_use]
    pub fn config(&self) -> &DatabaseConfig {
        &self.inner.config
    }
}

fn resolve_principal_homes(
    snapshot: &CatalogReadSnapshot,
    principal: Option<&Principal>,
) -> Result<(Option<SchemaDescriptor>, Option<GraphDescriptor>)> {
    let Some(principal) = principal else {
        return Ok((None, None));
    };
    let home_schema_path = principal.home_schema();
    let home_graph_path = principal.home_graph();
    if home_graph_path.is_some() && home_schema_path.is_none() {
        return Err(Error::invalid_principal_home(
            "a principal home graph requires a home schema",
        ));
    }
    if let (Some(schema), Some(graph)) = (home_schema_path, home_graph_path)
        && graph.schema_path() != *schema
    {
        return Err(Error::invalid_principal_home(
            "the principal home graph is outside the home schema",
        ));
    }

    let home_schema = home_schema_path
        .map(|path| {
            snapshot.resolve_schema(path).map_err(|source| {
                Error::invalid_principal_home_source(
                    "the principal home schema is not a current schema",
                    source,
                )
            })
        })
        .transpose()?;
    let home_graph = home_graph_path
        .map(|path| {
            snapshot.resolve_graph(path).map_err(|source| {
                Error::invalid_principal_home_source(
                    "the principal home graph is not a current graph",
                    source,
                )
            })
        })
        .transpose()?;
    if let (Some(schema), Some(graph)) = (&home_schema, &home_graph)
        && graph.path.schema_path() != schema.path
    {
        return Err(Error::invalid_principal_home(
            "the resolved principal home graph is outside the home schema",
        ));
    }
    Ok((home_schema, home_graph))
}

/// Construction funnel for [`Database`].
#[derive(Clone, Debug, Default)]
pub struct DatabaseBuilder {
    config: DatabaseConfig,
}

impl DatabaseBuilder {
    /// Create a builder for the supported in-memory mode.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a builder from an explicit configuration.
    #[must_use]
    pub const fn from_config(config: DatabaseConfig) -> Self {
        Self { config }
    }

    /// Borrow the configuration this builder will consume.
    #[must_use]
    pub const fn config(&self) -> &DatabaseConfig {
        &self.config
    }

    /// Build a database.
    ///
    /// The current builder has no fallible configuration or I/O path. It panics
    /// only if this process exhausts the non-wrapping facade [`DatabaseId`]
    /// allocation domain.
    #[must_use]
    pub fn build(self) -> Database {
        Database {
            inner: Arc::new(DatabaseInner::new(self.config)),
        }
    }
}

pub(crate) struct DatabaseInner {
    pub(crate) database_id: DatabaseId,
    pub(crate) config: DatabaseConfig,
    pub(crate) state: ArcSwap<DatabaseState>,
    pub(crate) transactions: MutationCoordinator,
    next_transaction_id: AtomicU64,
    pub(crate) procedures: BuiltinProcedureRegistry,
    #[cfg(test)]
    pub(crate) failure: Mutex<Option<crate::catalog::FailurePoint>>,
    #[cfg(test)]
    pub(crate) drop_blocked: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    #[cfg(test)]
    pub(crate) before_implicit_commit: Mutex<Option<ImplicitCommitPause>>,
    #[cfg(test)]
    pub(crate) replacement_graph_constructions: std::sync::atomic::AtomicUsize,
}

impl DatabaseInner {
    fn new(config: DatabaseConfig) -> Self {
        Self {
            database_id: DatabaseId::allocate(),
            config,
            state: ArcSwap::from(Arc::new(DatabaseState {
                publication: 0,
                catalog: initial_snapshot(),
                graphs: BTreeMap::new(),
                graph_types: BTreeMap::new(),
                high_water: HighWaterMarks::initial(),
            })),
            transactions: MutationCoordinator::new(),
            next_transaction_id: AtomicU64::new(1),
            procedures: BuiltinProcedureRegistry::new(),
            #[cfg(test)]
            failure: Mutex::new(None),
            #[cfg(test)]
            drop_blocked: Mutex::new(None),
            #[cfg(test)]
            before_implicit_commit: Mutex::new(None),
            #[cfg(test)]
            replacement_graph_constructions: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn allocate_transaction_id(&self) -> Result<TransactionId> {
        let current = self
            .next_transaction_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| match next {
                0 => None,
                u64::MAX => Some(0),
                value => Some(value + 1),
            })
            .map_err(|_| Error::transaction_id_exhausted())?;
        let value = NonZeroU64::new(current).ok_or_else(Error::transaction_id_exhausted)?;
        Ok(TransactionId::from_nonzero(value))
    }

    #[cfg(test)]
    pub(crate) fn set_next_transaction_id(&self, next: u64) {
        self.next_transaction_id.store(next, Ordering::Relaxed);
    }
}

/// One-shot, per-database rendezvous after implicit staging, outside the reservation.
#[cfg(test)]
pub(crate) struct ImplicitCommitPause {
    pub(crate) staged: std::sync::mpsc::Sender<crate::Transaction>,
    pub(crate) resume: std::sync::mpsc::Receiver<()>,
}

/// Test-only per-thread count of active graph request leases.
#[cfg(test)]
pub(crate) struct GraphRequestDepth;

#[cfg(test)]
impl GraphRequestDepth {
    thread_local! {
        static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    pub(crate) fn enter() -> Self {
        Self::DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }

    pub(crate) fn current() -> usize {
        Self::DEPTH.with(std::cell::Cell::get)
    }
}

#[cfg(test)]
impl Drop for GraphRequestDepth {
    fn drop(&mut self) {
        Self::DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

pub(crate) struct DatabaseState {
    pub(crate) publication: u64,
    pub(crate) catalog: CatalogSnapshot,
    pub(crate) graphs: BTreeMap<GraphId, Arc<GraphInstance>>,
    pub(crate) graph_types: BTreeMap<GraphTypeId, Arc<GraphTypeDef>>,
    pub(crate) high_water: HighWaterMarks,
}

pub(crate) struct GraphInstance {
    pub(crate) graph: SharedGraph,
    pub(crate) lifecycle: RwLock<()>,
}

impl GraphInstance {
    pub(crate) fn new(graph: SharedGraph) -> Self {
        Self {
            graph,
            lifecycle: RwLock::new(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HighWaterMarks {
    pub(crate) schema: u64,
    pub(crate) graph: u64,
    pub(crate) graph_type: u64,
}

impl HighWaterMarks {
    const fn initial() -> Self {
        Self {
            schema: 0,
            graph: 0,
            graph_type: 0,
        }
    }
}

fn initial_snapshot() -> CatalogSnapshot {
    let generation = CatalogGeneration::new(1).expect("initial generation is nonzero");
    let creation = CreationMetadata::new(generation, None);
    let catalog_id = CatalogId::new(1).expect("catalog ID is nonzero");
    let root_id = DirectoryId::new(1).expect("directory ID is nonzero");
    let catalog = CatalogDescriptor::catalog(
        catalog_id,
        CatalogName::regular(CATALOG_NAME).expect("catalog name is valid"),
        generation,
        creation.clone(),
    )
    .expect("catalog descriptor is valid");
    let root = CatalogDescriptor::root_directory(root_id, catalog_id, generation, creation.clone())
        .expect("root descriptor is valid");
    CatalogSnapshotBuilder::new(generation, catalog, root)
        .expect("catalog and root are related")
        .build()
        .expect("initial catalog snapshot is complete")
}
