//! One full-readiness pipeline; neither selection nor reconstruction publishes a database.

use super::*;
use selene_persist::logical_stream::RecoveryReader;

/// Successful full recovery readiness for one pinned on-disk selection and WAL extent.
/// This is not historical acknowledgment, physical durability, write permission,
/// freshness after selection, or a retention lease. All retained indexes were rebuilt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationReport {
    /// Validated immutable selected manifest basename.
    pub manifest: String,
    /// Exact selected manifest digest from CURRENT.
    pub manifest_digest: [u8; 32],
    /// Selected manifest generation, distinct from transaction sequence.
    pub generation: u64,
    /// Validated required snapshot basename.
    pub snapshot: String,
    /// Complete selected snapshot extent.
    pub snapshot_bytes: u64,
    /// Snapshot-covered complete WAL boundary.
    pub snapshot_position: DurableCommitPosition,
    /// Snapshot-covered complete boundary digest.
    pub snapshot_boundary_digest: [u8; 32],
    /// Validated required WAL basename.
    pub wal: String,
    /// WAL extent captured under the selection epoch; excludes subsequent appends.
    pub captured_wal_bytes: u64,
    /// Complete recovered boundary digest, not a synchronization claim.
    pub boundary_digest: [u8; 32],
    /// Time selecting and leasing the captured artifacts.
    pub selection_elapsed: Duration,
    /// Physical WAL framing and prefix verification time, excluding semantic replay.
    pub framing_elapsed: Duration,
    /// Semantic suffix application time, including retained-state validation work.
    pub semantic_replay_elapsed: Duration,
    /// Work performed. Synchronize time is always zero for read-only verification.
    pub recovery: RecoveryInfo,
    /// All reconstructed graph instances, including empty graphs.
    pub graphs: usize,
    /// Total primary nodes across all reconstructed graphs.
    pub nodes: usize,
    /// Total primary edges across all reconstructed graphs.
    pub edges: usize,
}

pub(super) struct PreparedRecovery {
    pub state: DatabaseState,
    pub replay: ReplayState,
    pub report: VerificationReport,
}

pub(super) fn prepare(
    recovery: &mut RecoveryReader,
    selection_elapsed: Duration,
) -> StorageResult<PreparedRecovery> {
    let context = recovery.snapshot_context();
    let start = Instant::now();
    if context.publication != context.boundary.sequence {
        return Err(StorageError::stream(
            StoragePhase::Snapshot,
            selene_persist::logical_frame::FrameError::SnapshotBoundary.into(),
        )
        .at(
            recovery.snapshot_name(),
            Some(0),
            Some(context.boundary.sequence),
        ));
    }
    let body = recovery
        .snapshot_body()
        .map_err(|e| StorageError::stream(StoragePhase::Snapshot, e))?;
    let mut replay = ReplayState::from_checkpoint(&body, Limits::default()).map_err(|e| {
        StorageError::codec(StoragePhase::Snapshot, e).at(
            recovery.snapshot_name(),
            Some(selene_persist::logical_snapshot::HEADER_LEN as u64),
            Some(context.boundary.sequence),
        )
    })?;
    drop(body);
    let snapshot_elapsed = start.elapsed();
    let start = Instant::now();
    let mut framing_elapsed = Duration::ZERO;
    let mut semantic_replay_elapsed = Duration::ZERO;
    loop {
        let frame_start = Instant::now();
        let next = recovery
            .next_body()
            .map_err(|e| StorageError::stream(StoragePhase::Replay, e))?;
        framing_elapsed += frame_start.elapsed();
        let Some(body) = next else {
            break;
        };
        let semantic_start = Instant::now();
        replay = replay.apply_body(&body, Limits::default()).map_err(|e| {
            StorageError::codec(StoragePhase::Replay, e).at(
                &recovery.wal_name(),
                Some(recovery.body_offset()),
                Some(recovery.position().sequence),
            )
        })?;
        semantic_replay_elapsed += semantic_start.elapsed();
    }
    recovery
        .finish()
        .map_err(|e| StorageError::stream(StoragePhase::Replay, e))?;
    let wal_elapsed = start.elapsed();
    let start = Instant::now();
    let catalog = replay.catalog().reconstruct().map_err(|e| {
        StorageError::new(StoragePhase::Rebuild, StorageErrorKind::Semantic, e).at(
            recovery.selector().manifest_name(),
            None,
            Some(recovery.position().sequence),
        )
    })?;
    selene_gql::BuiltinProcedureRegistry::new()
        .validate_catalog(&catalog)
        .map_err(|e| {
            StorageError::new(StoragePhase::Rebuild, StorageErrorKind::NativeAdmission, e).at(
                recovery.selector().manifest_name(),
                None,
                Some(recovery.position().sequence),
            )
        })?;
    let runtime = replay.materialize(Limits::default()).map_err(|e| {
        StorageError::codec(StoragePhase::Rebuild, e).at(
            recovery.selector().manifest_name(),
            None,
            Some(recovery.position().sequence),
        )
    })?;
    let water = replay.catalog().high_water();
    let get = |kind| water.get(&kind).copied().unwrap_or(0);
    use selene_catalog::CatalogObjectKind as K;
    if get(K::BindingTable) != 0 {
        return Err(StorageError::codec(
            StoragePhase::Rebuild,
            selene_core::logical::CodecError::Admission(
                "unsupported binding-table allocation domain",
            ),
        )
        .at(
            recovery.selector().manifest_name(),
            None,
            Some(recovery.position().sequence),
        ));
    }
    let graphs = runtime.graphs.len();
    let nodes = runtime.graphs.values().map(|g| g.read().node_count()).sum();
    let edges = runtime.graphs.values().map(|g| g.read().edge_count()).sum();
    let state = DatabaseState {
        publication: recovery.position().sequence,
        catalog: runtime.catalog,
        graphs: runtime
            .graphs
            .into_iter()
            .map(|(id, graph)| {
                Ok((
                    selene_catalog::GraphId::new(id.get()).map_err(|e| {
                        StorageError::new(StoragePhase::Rebuild, StorageErrorKind::InvalidState, e)
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
    };
    let report = VerificationReport {
        manifest: recovery.selector().manifest_name().into(),
        manifest_digest: recovery.selector().digest(),
        generation: recovery.selector().generation().get(),
        snapshot: recovery.snapshot_name().into(),
        snapshot_bytes: recovery.snapshot_bytes(),
        snapshot_position: context.boundary.into(),
        snapshot_boundary_digest: context.boundary.digest,
        wal: recovery.wal_name(),
        captured_wal_bytes: recovery.captured_wal_bytes(),
        boundary_digest: recovery.position().digest,
        selection_elapsed,
        framing_elapsed,
        semantic_replay_elapsed,
        graphs,
        nodes,
        edges,
        recovery: RecoveryInfo {
            snapshot_elapsed,
            wal_elapsed,
            rebuild_elapsed: start.elapsed(),
            synchronize_elapsed: Duration::ZERO,
            verified_prefix_records: recovery.prefix_records(),
            replayed_suffix_records: recovery.suffix_records(),
            rebuilt_indexes: runtime.rebuilt_indexes,
            position: recovery.position().into(),
        },
    };
    Ok(PreparedRecovery {
        state,
        replay,
        report,
    })
}

impl Database {
    /// Verify full recovery readiness from disk without writer admission, file
    /// creation, synchronization, repair, or publication of a Database. May run
    /// while a Database owns LOCK. Rebuilds all retained indexes in temporary memory.
    /// A later commit/checkpoint can advance CURRENT; the report describes only the
    /// leased selection and captured WAL extent, never historical ACK or power loss.
    ///
    /// ```
    /// use selene_db::Database;
    /// # let directory = tempfile::tempdir()?;
    /// let database = Database::create(directory.path())?;
    /// let report = Database::verify(directory.path())?;
    /// assert_eq!(report.recovery.position, database.durable_status().unwrap().position);
    /// assert_eq!(report.graphs, 0);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn verify(path: impl AsRef<Path>) -> StorageResult<VerificationReport> {
        Self::verify_in(&DatabaseDirectory::open(path)?)
    }

    /// Full read-only verification relative to an existing directory capability.
    /// Missing coordination files fail without creating them. Selected artifacts
    /// remain leased throughout reading and reconstruction; temporary runtimes and
    /// leases are dropped on success and error before this method returns.
    pub fn verify_in(directory: &DatabaseDirectory) -> StorageResult<VerificationReport> {
        let start = Instant::now();
        let mut reader =
            RecoveryReader::open(&directory.0, &compatibility()?, Limits::default().bytes)
                .map_err(|e| StorageError::stream(StoragePhase::Select, e))?;
        let prepared = prepare(&mut reader, start.elapsed())?;
        Ok(prepared.report)
    }
}
