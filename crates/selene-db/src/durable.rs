//! Public format-2 lifecycle, separate from the infallible memory builder.

use crate::{
    Database, DatabaseConfig, DurableCommitPosition,
    database::{DatabaseInner, DatabaseState, GraphInstance, HighWaterMarks},
    transaction::MutationCoordinator,
};
use arc_swap::ArcSwap;
use selene_core::logical::Limits;
use selene_graph::logical_transaction::{ReplayState, encode_checkpoint};
use selene_persist::{
    StoreDirectory,
    control::{CompatibilityIdentity, EmptyStoreControl},
    logical_stream::{LogicalWal, ReopeningWal},
};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

mod error;
pub use error::{StorageError, StorageErrorKind, StoragePhase};
type StorageResult<T> = std::result::Result<T, StorageError>;

/// Facade-owned retained directory authority. Its locator is diagnostic only.
/// Containing directories must already exist. Native Linux/macOS only.
#[derive(Clone, Debug)]
pub struct DatabaseDirectory(pub(crate) StoreDirectory);
impl DatabaseDirectory {
    /// Anchor an existing directory once; subsequent operations do not resolve the path.
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        StoreDirectory::open(path.as_ref())
            .map(Self)
            .map_err(|e| StorageError::persist(StoragePhase::Anchor, e))
    }
    /// Use an already-open directory; `locator` never grants I/O authority.
    pub fn from_file(file: File, locator: impl Into<PathBuf>) -> StorageResult<Self> {
        StoreDirectory::from_file(file, locator)
            .map(Self)
            .map_err(|e| StorageError::persist(StoragePhase::Anchor, e))
    }
}

/// One established durable boundary; not a retry token or retention lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableStatus {
    /// Last synchronized complete record, with durable store/epoch identity.
    pub position: DurableCommitPosition,
    /// Digest qualifying that complete record boundary.
    pub digest: [u8; 32],
    /// Whether uncertainty prohibits more writes and checkpoints on this owner.
    pub fenced: bool,
}

/// Exact outcome of successful immutable checkpoint selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointOutcome {
    /// New immutable control manifest generation.
    pub generation: u64,
    /// Selected snapshot name, diagnostic only.
    pub snapshot: String,
    /// Complete snapshot file bytes.
    pub bytes: u64,
    /// Complete snapshot digest.
    pub digest: [u8; 32],
    /// Exact WAL position covered by the checkpoint.
    pub position: DurableCommitPosition,
    /// Digest of the covered WAL boundary.
    pub boundary_digest: [u8; 32],
    /// Pinned outer publication ordinal.
    pub publication: u64,
    /// Measured time holding the serial write reservation, excluding acquisition wait.
    pub write_reservation_elapsed: Duration,
}
impl From<selene_persist::logical_stream::CheckpointInfo> for CheckpointOutcome {
    fn from(info: selene_persist::logical_stream::CheckpointInfo) -> Self {
        Self {
            generation: info.generation,
            snapshot: info.name,
            bytes: info.bytes,
            digest: info.digest,
            position: info.boundary.into(),
            boundary_digest: info.boundary.digest,
            publication: info.publication,
            write_reservation_elapsed: Duration::ZERO,
        }
    }
}

/// Observed recovery work. Prior acknowledgment cannot be inferred from complete bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryInfo {
    /// Selected snapshot loading and isolated semantic reconstruction time.
    pub snapshot_elapsed: Duration,
    /// Full retained-prefix verification plus semantic suffix replay time.
    pub wal_elapsed: Duration,
    /// Native catalog validation and eager all-index/runtime reconstruction time.
    pub rebuild_elapsed: Duration,
    /// Final complete-tail synchronization and writer establishment time.
    pub synchronize_elapsed: Duration,
    /// Verified prefix records; this first implementation scans the retained prefix.
    pub verified_prefix_records: u64,
    /// Whole suffix records semantically applied, including complete unacknowledged records.
    pub replayed_suffix_records: u64,
    /// All retained registered indexes rebuilt before the Database was returned.
    pub rebuilt_indexes: usize,
    /// Recovered and explicitly synchronized full-record boundary.
    pub position: DurableCommitPosition,
}

impl Database {
    /// Actual instance storage mode. Durable I/O is selected only by explicit
    /// fallible create/open, never by an ignored infallible-builder setting.
    pub fn open_mode(&self) -> crate::OpenMode {
        if self.durable_status().is_some() {
            crate::OpenMode::Durable
        } else {
            crate::OpenMode::InMemory
        }
    }
    /// Strictly create a format-2 database in an existing empty directory.
    /// Refuses existing/foreign artifacts; never overwrites, migrates or opens existing data.
    /// All database, catalog and session owning handles retain the permanent writer lease.
    ///
    /// ```
    /// use selene_db::{CreatePolicy, Database, ObjectPath, SchemaPath};
    /// # let directory = tempfile::tempdir()?;
    /// let database = Database::create(directory.path())?;
    /// database.catalog().create_schema(&SchemaPath::regular("selene", "memory")?, CreatePolicy::Strict)?;
    /// let path = ObjectPath::regular("selene", "memory", "data")?;
    /// database.catalog().create_graph(&path, None, CreatePolicy::Strict)?;
    /// let session = database.session(&path)?;
    /// session.execute("INSERT (:Item {n: 1})")?;
    /// database.checkpoint()?;
    /// drop(session);
    /// drop(database);
    /// let reopened = Database::open(directory.path())?;
    /// assert_eq!(reopened.session(&path)?.execute("MATCH (n:Item) RETURN n")?.row_count(), Some(1));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn create(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::create_in(&DatabaseDirectory::open(path)?)
    }

    /// Strict durable creation relative to retained directory authority.
    /// A failed bootstrap may leave unselected artifacts; retry never adopts them.
    pub fn create_in(directory: &DatabaseDirectory) -> StorageResult<Self> {
        let mut inner = DatabaseInner::new(DatabaseConfig::default());
        let records = crate::CatalogReadSnapshot {
            state: inner.state.load_full(),
        }
        .logical_catalog()
        .map_err(|e| StorageError::new(StoragePhase::Create, StorageErrorKind::InvalidState, e))?;
        let body = encode_checkpoint(
            &records,
            &inner.state.load().graph_types,
            &[],
            Limits::default(),
        )
        .map_err(|e| StorageError::codec(StoragePhase::Create, e))?;
        let replay = ReplayState::from_checkpoint(&body, Limits::default())
            .map_err(|e| StorageError::codec(StoragePhase::Create, e))?;
        let control = EmptyStoreControl::create_empty(&directory.0, compatibility()?)
            .map_err(|e| StorageError::persist(StoragePhase::Create, e))?;
        let mut wal = LogicalWal::create(control)
            .map_err(|e| StorageError::stream(StoragePhase::Create, e))?;
        wal.checkpoint(&body, 0)
            .map_err(|e| StorageError::stream(StoragePhase::Checkpoint, e))?;
        inner.transactions = MutationCoordinator::with_wal(wal, replay);
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Open an existing format-2 database, non-destructively and all-or-error.
    /// Eagerly rebuilds every supported retained index before returning. It never
    /// repairs a damaged tail or returns partial/background-ready state.
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::open_in(&DatabaseDirectory::open(path)?)
    }

    /// Open via a retained directory. Sessions retaining a prior owner cause contention.
    /// A fresh process-local DatabaseId is allocated; durable StoreId/epoch are preserved.
    pub fn open_in(directory: &DatabaseDirectory) -> StorageResult<Self> {
        let mut recovery =
            ReopeningWal::open(&directory.0, &compatibility()?, Limits::default().bytes)
                .map_err(|e| StorageError::stream(StoragePhase::Select, e))?;
        let context = recovery.snapshot_context();
        let start = Instant::now();
        if context.publication != context.boundary.sequence {
            return Err(StorageError::invalid(
                StoragePhase::Snapshot,
                "facade publication/WAL sequence mismatch",
            ));
        }
        let body = recovery
            .snapshot_body()
            .map_err(|e| StorageError::stream(StoragePhase::Snapshot, e))?;
        let mut replay = ReplayState::from_checkpoint(&body, Limits::default())
            .map_err(|e| StorageError::codec(StoragePhase::Snapshot, e))?;
        drop(body);
        let snapshot_elapsed = start.elapsed();
        let start = Instant::now();
        while let Some(body) = recovery
            .next_body()
            .map_err(|e| StorageError::stream(StoragePhase::Replay, e))?
        {
            replay = replay
                .apply_body(&body, Limits::default())
                .map_err(|e| StorageError::codec(StoragePhase::Replay, e))?;
        }
        let wal_elapsed = start.elapsed();
        let start = Instant::now();
        let mut inner = DatabaseInner::new(DatabaseConfig::default());
        let catalog = replay.catalog().reconstruct().map_err(|e| {
            StorageError::new(StoragePhase::Rebuild, StorageErrorKind::InvalidState, e)
        })?;
        inner.procedures.validate_catalog(&catalog).map_err(|e| {
            StorageError::new(StoragePhase::Rebuild, StorageErrorKind::InvalidState, e)
        })?;
        let runtime = replay
            .materialize(Limits::default())
            .map_err(|e| StorageError::codec(StoragePhase::Rebuild, e))?;
        let water = replay.catalog().high_water();
        let get = |kind| water.get(&kind).copied().unwrap_or(0);
        use selene_catalog::CatalogObjectKind as K;
        // The current facade has no binding-table storage/allocation authority.
        if get(K::BindingTable) != 0 {
            return Err(StorageError::invalid(
                StoragePhase::Rebuild,
                "unsupported binding-table allocation domain",
            ));
        }
        inner.state = ArcSwap::from(Arc::new(DatabaseState {
            publication: recovery.position().sequence,
            catalog: runtime.catalog,
            graphs: runtime
                .graphs
                .into_iter()
                .map(|(id, graph)| {
                    Ok((
                        selene_catalog::GraphId::new(id.get()).map_err(|e| {
                            StorageError::new(
                                StoragePhase::Rebuild,
                                StorageErrorKind::InvalidState,
                                e,
                            )
                        })?,
                        Arc::new(GraphInstance::new(graph)),
                    ))
                })
                .collect::<StorageResult<_>>()?,
            graph_types: runtime.graph_types,
            high_water: HighWaterMarks {
                schema: get(K::Schema),
                graph: get(K::Graph),
                graph_type: get(K::GraphType),
                index: get(K::Index),
                constraint: get(K::Constraint),
                procedure: get(K::Procedure),
            },
        }));
        let rebuild_elapsed = start.elapsed();
        let start = Instant::now();
        let mut info = RecoveryInfo {
            snapshot_elapsed,
            wal_elapsed,
            rebuild_elapsed,
            synchronize_elapsed: Duration::ZERO,
            verified_prefix_records: recovery.prefix_records(),
            replayed_suffix_records: recovery.suffix_records(),
            rebuilt_indexes: runtime.rebuilt_indexes,
            position: recovery.position().into(),
        };
        let wal = recovery
            .finish()
            .map_err(|e| StorageError::stream(StoragePhase::Synchronize, e))?;
        info.synchronize_elapsed = start.elapsed();
        inner.recovery = Some(info);
        inner.transactions = MutationCoordinator::with_wal(wal, replay);
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Hold the serial write reservation through full image encoding and durable selection.
    /// Held immutable reader views stay valid; foreground writes wait. No rotation/prune.
    /// A fenced/uncertain owner cannot checkpoint its stale semantic preflight candidate.
    pub fn checkpoint(&self) -> StorageResult<CheckpointOutcome> {
        self.inner.checkpoint()
    }

    /// Current durable status, or None for the unchanged memory-only builder.
    pub fn durable_status(&self) -> Option<DurableStatus> {
        self.inner.durable_status()
    }
    /// Work performed by this instance's successful open (None after memory build/create).
    pub fn recovery_info(&self) -> Option<RecoveryInfo> {
        self.inner.recovery
    }
}

fn compatibility() -> StorageResult<CompatibilityIdentity> {
    let mut hash = [0; 32];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&selene_profile::PROFILE_HASH[i * 2..i * 2 + 2], 16).map_err(
            |e| StorageError::new(StoragePhase::Select, StorageErrorKind::Compatibility, e),
        )?;
    }
    let unicode = selene_catalog::CATALOG_UNICODE_VERSION;
    let collation = match selene_profile::annex_b_by_id("ID022").map(|r| &r.decision) {
        Some(selene_profile::AnnexBDecision::Selected {
            value: selene_profile::AnnexBValue::Identifier(name),
            ..
        }) => *name,
        _ => {
            return Err(StorageError::invalid(
                StoragePhase::Select,
                "production collation identity unavailable",
            ));
        }
    };
    CompatibilityIdentity::new(
        selene_profile::PROFILE_ID,
        selene_profile::PROFILE_FORMAT_VERSION,
        hash,
        [unicode.0.into(), unicode.1.into(), unicode.2.into()],
        collation,
        1,
    )
    .map_err(|e| StorageError::persist(StoragePhase::Select, e))
}

#[cfg(test)]
mod tests;
