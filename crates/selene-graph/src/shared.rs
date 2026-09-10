//! Shared graph wrapper implementing lock-free reads and serialized writes.

use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use arc_swap::ArcSwap;
use parking_lot::{Mutex, RwLock};

use selene_core::GraphId;
use selene_persist::{AuditLog, WalConfig, WalWriter};

use crate::committer_batch::CommitBatching;
use crate::core_provider::{CoreProvider, DurableState};
use crate::durable_provider::DurableProvider;
use crate::error::{GraphError, GraphResult};
use crate::graph::SeleneGraph;
use crate::graph_types::GraphTypeDef;
use crate::id_allocator::IdAllocator;
use crate::index_provider::{IndexProvider, ProviderTag};
use crate::vector_index::{VectorIndexMaintenancePolicy, VectorIndexRebuildReport};
use crate::write_txn::WriteTxn;

mod allocation;
pub use allocation::GraphAllocationAuthority;

/// Per-graph shared runtime state.
///
/// Since v1.2 (BRIEF 1) every snapshot publish is funneled through a single
/// per-graph committer thread (`CommitterThread`), which is
/// the **sole writer** of the `snapshot` [`ArcSwap`] cell. `begin_write` hands
/// each [`WriteTxn`] a cheap submit handle; `commit`/`compact` seal-and-submit
/// to the committer and block until it publishes. This single-committer +
/// sole-publisher discipline is what preserves D10 strict-serializability once
/// `seal()` drops the write lock early — it is load-bearing and NOT
/// type-enforced (a second committer or ArcSwap writer would silently break it).
///
/// # Canonical durable round trip
///
/// [`SharedGraph::builder`] + [`with_wal`](SharedGraphBuilder::with_wal) is the
/// only supported way to make a graph durable, and [`SharedGraph::recover`] is
/// the only way to reopen one. An [`IndexProvider`] is *not* a durability seam:
/// its fanout runs after publication and its errors are logged and swallowed,
/// so a WAL written from a provider callback can report a successful commit for
/// data that never reached the disk.
///
/// This example is a doctest so the recipe cannot rot the way a prose one can:
/// it is compiled and run by `cargo test --doc`, and it uses only the public
/// surface an embedder outside the workspace can reach.
///
/// ```
/// use std::fs;
///
/// use selene_core::{GraphId, LabelSet, PropertyMap, Value, db_string};
/// use selene_graph::{DEFAULT_WAL_FILE_NAME, SharedGraph, WalConfig};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = std::env::temp_dir().join(format!(
///     "selene-doc-canonical-persistence-{}",
///     std::process::id()
/// ));
/// let _ = fs::remove_dir_all(&dir);
/// fs::create_dir_all(&dir)?;
/// let graph_id = GraphId::new(1);
///
/// // Create. `with_wal` takes the WAL *file* path, and refuses a directory
/// // that already holds a store — building is not reopening.
/// let shared = SharedGraph::builder(graph_id)
///     .with_wal(dir.join(DEFAULT_WAL_FILE_NAME), WalConfig::default())?
///     .build()?;
///
/// // Commit. `commit()` returns only after the committer has appended the
/// // entry, fsynced it, and published the new snapshot: append -> flush ->
/// // publish -> acknowledge. An `Ok` here means the write is durable.
/// let mut txn = shared.begin_write();
/// {
///     let mut mutator = txn.mutator();
///     mutator.create_node(
///         LabelSet::single(db_string("Memory")?),
///         PropertyMap::from_pairs([(db_string("kind")?, Value::Int(1))])?,
///     )?;
/// }
/// txn.commit()?;
///
/// // Close. Dropping joins the committer thread once every outstanding
/// // transaction handle is gone.
/// drop(shared);
///
/// // Reopen. Recovery replays the WAL over the newest published snapshot.
/// let recovered = SharedGraph::recover(&dir, graph_id)?;
/// assert_eq!(recovered.read().node_count(), 1);
/// drop(recovered);
///
/// # fs::remove_dir_all(&dir)?;
/// # Ok(())
/// # }
/// ```
///
/// # After a durable failure, reopen — do not retry
///
/// A durable error raised after the commit is sealed leaves the outcome
/// genuinely unknown: the entry may or may not have reached the disk. The
/// committer poisons the graph and every later call fails, so a retry loop on
/// the same handle can only spin. Test
/// [`GraphError::requires_reopen`](crate::GraphError::requires_reopen) and go
/// back through [`SharedGraph::recover`], which re-reads the durable state and
/// tells you which side of the commit point the store actually landed on.
pub struct SharedGraph {
    shared: Arc<RwLock<Arc<SeleneGraph>>>,
    snapshot: Arc<ArcSwap<SeleneGraph>>,
    schema_version: Arc<AtomicU64>,
    allocator: Arc<Mutex<IdAllocator>>,
    /// Fixed provider registry, frozen at construction. Shared as one
    /// allocation so `begin_write` hands the registry to each transaction
    /// with a single refcount bump instead of a per-transaction `Vec` clone.
    providers: Arc<[Arc<dyn IndexProvider>]>,
    durable_providers: Vec<Arc<dyn DurableProvider>>,
    /// The single per-graph committer thread; sole publisher of `snapshot`.
    /// Dropped last via [`SharedGraph`]'s implicit drop order, which joins the
    /// thread once every outstanding [`WriteTxn`] submit handle is gone.
    committer: crate::committer::CommitterThread,
    /// Torn WAL tail discarded while recovering this graph, if any.
    ///
    /// Set only by [`SharedGraph::recover`] and its variants; always `None` for
    /// a graph built through [`SharedGraphBuilder`], which refuses a directory
    /// that already holds a store and so has no tail to repair. See
    /// [`SharedGraph::recovery_tail_repair`].
    pub(crate) recovery_tail_repair: Option<selene_persist::WalTailRepair>,
}

impl SharedGraph {
    /// Construct an empty shared graph.
    #[must_use]
    pub fn new(graph_id: GraphId) -> Self {
        Self::from_graph(SeleneGraph::new(graph_id))
    }

    /// Start building an empty shared graph with optional providers.
    #[must_use]
    pub fn builder(graph_id: GraphId) -> SharedGraphBuilder {
        SharedGraphBuilder::new(graph_id)
    }

    /// Construct shared state from a pre-built graph snapshot.
    ///
    /// The allocator floors are derived from storage length so that stale
    /// `GraphMeta.next_*_id` values cannot allow ID reuse over rows that
    /// already exist (recovery hardening — spec 02 §4 forbids ID reuse).
    ///
    /// # Panics
    ///
    /// Panics if the supplied graph contains more than `u32::MAX` rows in
    /// either store. Selene-graph's row index is `u32` by construction;
    /// `SeleneGraph::new()` always satisfies this, and any caller-built
    /// fixture must too. Use [`SharedGraph::try_from_graph`] for the
    /// fallible variant when validating untrusted snapshots.
    #[must_use]
    pub fn from_graph(graph: SeleneGraph) -> Self {
        Self::try_from_graph(graph).expect("graph store row count exceeds u32::MAX")
    }

    /// Fallible variant of [`SharedGraph::from_graph`]. Returns
    /// [`GraphError::Inconsistent`] when the graph's stores exceed the
    /// `u32` row capacity.
    pub fn try_from_graph(graph: SeleneGraph) -> GraphResult<Self> {
        Self::from_graph_with_core(graph, Vec::new())
    }

    /// Construct shared state from a graph snapshot and fixed provider list.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Provider`] when two providers declare the same
    /// [`ProviderTag`], and [`GraphError::Inconsistent`] when the graph's
    /// stores exceed the `u32` row capacity.
    pub fn from_graph_with_providers(
        graph: SeleneGraph,
        providers: Vec<Arc<dyn IndexProvider>>,
    ) -> GraphResult<Self> {
        Self::from_graph_with_core(graph, providers)
    }

    /// Construct shared state from a graph snapshot and commit-critical WAL file.
    ///
    /// Since v1.2 (BRIEF 2) the committer is the sole fsync caller, so the WAL is
    /// **always** opened in [`selene_persist::SyncPolicy::OnFlushOnly`] regardless of the
    /// `config.sync_policy` passed (it is overwritten before
    /// [`WalWriter::open`]). This non-builder constructor uses
    /// [`CommitBatching::Off`], so the committer still fsyncs once per commit —
    /// behaviorally identical to BRIEF 1's `EveryN(1)`.
    ///
    /// # The directory must not already hold a store
    ///
    /// `graph` is attached as-is and never reconciled against what is on disk,
    /// so this carries the same hazard as [`SharedGraphBuilder::with_wal`] and
    /// goes through the same guard. Reconstructing a graph *from* an existing
    /// store is [`Self::recover`]; hand-rolled recovery should drive
    /// [`crate::CoreProvider`] and then attach its own writer through the
    /// crate-internal constructor `recover` itself uses.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Persist`] when the WAL cannot be opened,
    /// [`GraphError::ExistingStore`] when the directory already holds a store,
    /// plus the same consistency and provider-registration errors as
    /// [`Self::try_from_graph`].
    pub fn from_graph_with_wal(
        graph: SeleneGraph,
        path: impl AsRef<Path>,
        config: WalConfig,
    ) -> GraphResult<Self> {
        let writer = builder::open_fresh_wal(path.as_ref(), config)?;
        Self::from_graph_with_core_and_durables(
            graph,
            Vec::new(),
            Vec::new(),
            Some(writer),
            None,
            CommitBatching::Off,
        )
    }

    fn from_graph_with_core(
        graph: SeleneGraph,
        providers: Vec<Arc<dyn IndexProvider>>,
    ) -> GraphResult<Self> {
        Self::from_graph_with_core_and_durables(
            graph,
            providers,
            Vec::new(),
            None,
            None,
            CommitBatching::Off,
        )
    }

    pub(crate) fn from_graph_with_core_and_durables(
        graph: SeleneGraph,
        providers: Vec<Arc<dyn IndexProvider>>,
        mut durable_providers: Vec<Arc<dyn DurableProvider>>,
        wal_writer: Option<WalWriter>,
        audit_log: Option<AuditLog>,
        batching: CommitBatching,
    ) -> GraphResult<Self> {
        if audit_log.is_some() && wal_writer.is_none() {
            return Err(GraphError::Inconsistent {
                reason: "audit log configured without a WAL; audit mirroring requires durable WAL \
                         state"
                    .to_owned(),
            });
        }
        let snapshot = Arc::new(ArcSwap::from_pointee(graph.clone()));
        let has_wal = wal_writer.is_some();
        let durable = wal_writer
            .map(DurableState::new)
            .map(|durable| match audit_log {
                Some(audit) => durable.with_audit_log(audit),
                None => Ok(durable),
            })
            .transpose()?;
        let core = CoreProvider::new_for_live_with_wal(Arc::clone(&snapshot), durable);
        let mut all_providers = Vec::with_capacity(providers.len() + 1);
        all_providers.push(Arc::clone(&core) as Arc<dyn IndexProvider>);
        all_providers.extend(providers);
        if has_wal {
            durable_providers.push(Arc::clone(&core) as Arc<dyn DurableProvider>);
        }
        validate_unique_provider_tags(&all_providers)?;
        Self::from_graph_parts_and_snapshot(
            graph,
            core,
            all_providers,
            durable_providers,
            snapshot,
            batching,
        )
    }

    pub(crate) fn from_graph_parts_and_snapshot(
        graph: SeleneGraph,
        core: Arc<CoreProvider>,
        providers: Vec<Arc<dyn IndexProvider>>,
        durable_providers: Vec<Arc<dyn DurableProvider>>,
        snapshot: Arc<ArcSwap<SeleneGraph>>,
        batching: CommitBatching,
    ) -> GraphResult<Self> {
        for properties in graph
            .node_store
            .properties
            .iter()
            .chain(graph.edge_store.properties.iter())
        {
            properties.validate_stored_values()?;
        }
        validate_unique_provider_tags(&providers)?;
        // Freeze the registry into one shared allocation: the committer and
        // every `begin_write` transaction clone the `Arc`, not the `Vec`.
        let providers: Arc<[Arc<dyn IndexProvider>]> = providers.into();
        let mut graph = graph;
        // Attaching detached/scratch ownership establishes an independent
        // runtime even when GraphId and generation happen to match its source.
        graph.remint_layout();
        rebuild_derived_state(&mut graph)?;
        crate::property_index::rebuild_property_indexes(&mut graph)?;
        crate::property_index::rebuild_edge_property_indexes(&mut graph)?;
        crate::composite_property_index::rebuild_composite_property_indexes(&mut graph)?;
        crate::vector_index::rebuild_vector_indexes(&mut graph)?;
        crate::text_index::rebuild_text_indexes(&mut graph)?;
        if let Some(type_def) = graph.meta.bound_type.as_deref() {
            // Why: GraphMeta is publicly constructible, so SharedGraph::from_graph
            // can land a malformed bound_type that bypassed builder().bound_to()'s
            // validate(). Re-check self-consistency here so every constructor
            // arrives at the same closed-graph admissibility contract.
            type_def.validate_ref()?;
            crate::type_validator::validate_entity_state(&graph, type_def)?;
        }

        let node_floor = (graph.node_store.labels.len() as u64).saturating_add(1);
        let edge_floor = (graph.edge_store.label.len() as u64).saturating_add(1);
        let allocator = IdAllocator::from_meta_with_floors(&graph.meta, node_floor, edge_floor);

        // Debug-only structural net on the snapshot-load / recovery path: the
        // rebuild_* helpers above re-derive all indexes from columns, so a
        // rebuild bug would otherwise surface only as silent query
        // corruption. Highest-value placement — verify the rebuilt snapshot
        // before it is ever published. Compiled out in release builds.
        #[cfg(debug_assertions)]
        if let Err(reason) = graph.assert_indexes_consistent() {
            return Err(GraphError::Inconsistent {
                reason: format!("rebuilt snapshot failed index consistency check: {reason}"),
            });
        }

        let graph = Arc::new(graph);
        snapshot.store(Arc::clone(&graph));
        let shared = Arc::new(RwLock::new(graph));
        let schema_version = Arc::new(AtomicU64::new(0));
        let allocator = Arc::new(Mutex::new(allocator));
        // Spawn the single per-graph committer thread. It captures clones of
        // every handle it needs to publish + compact; it is the sole writer of
        // `snapshot`. All commit/compact/index-DDL publishes route through it.
        let committer =
            crate::committer::CommitterThread::spawn(crate::committer::CommitterHandles {
                snapshot: Arc::clone(&snapshot),
                schema_version: Arc::clone(&schema_version),
                providers: Arc::clone(&providers),
                core,
                durable_providers: durable_providers.clone(),
                batching,
            });
        Ok(Self {
            shared,
            snapshot,
            schema_version,
            allocator,
            providers,
            durable_providers,
            committer,
            // Recovery is the only path that can repair a tail, and it stamps
            // this after construction.
            recovery_tail_repair: None,
        })
    }

    /// The torn WAL tail discarded while recovering this graph, if any.
    ///
    /// `Ok` from [`SharedGraph::recover`] does not mean nothing was lost. A WAL
    /// whose final frame is short, corrupt, or zero-filled was never
    /// acknowledged to any caller, so discarding exactly it is correct crash
    /// recovery — but a commit that a client believed had been submitted may
    /// have been in it. Recovery previously reported that only through a
    /// `tracing::warn!`, which an embedder with no subscriber never sees.
    ///
    /// `None` means the WAL was intact, and it is also what every non-recovery
    /// construction path returns: a builder-created graph refuses a directory
    /// that already holds a store, so it has no tail to repair. Callers that
    /// need to distinguish "nothing lost" from "not a recovered graph" should
    /// track which constructor they used.
    ///
    /// The value describes this graph's reopen and does not change as the graph
    /// runs. It is a report, not an error: recovery succeeded either way.
    #[must_use]
    pub fn recovery_tail_repair(&self) -> Option<selene_persist::WalTailRepair> {
        self.recovery_tail_repair
    }

    /// Load the current immutable snapshot without taking the write lock.
    #[must_use]
    pub fn read(&self) -> Arc<SeleneGraph> {
        self.snapshot.load_full()
    }

    /// Durably checkpoint the current ordered graph/provider state and rotate
    /// the owned WAL through the MANIFEST protocol.
    ///
    /// Checkpointing is a hard boundary in the single committer's publish
    /// order. Every lower commit is flushed, published, and applied to
    /// providers before snapshot encoding starts; no higher commit is appended
    /// until rotation completes. Readers remain lock-free and continue to
    /// observe the last published snapshot while encoding runs.
    ///
    /// The snapshot directory and sequence come from the graph's owned
    /// `wal.log`, and durability barriers are mandatory. Use
    /// [`CheckpointConfig`](crate::CheckpointConfig) only to select section
    /// compression. After every provider section is prepared, the checkpoint
    /// reserves a fresh physical WAL sequence with a typed watermark and
    /// rotates at that sequence. The watermark is not a graph commit and does
    /// not advance graph or provider generation.
    ///
    /// # Panics
    ///
    /// Panics when called re-entrantly from a committer-thread provider
    /// callback; a nested checkpoint would wait behind that same callback.
    ///
    /// # Errors
    ///
    /// Returns an error when the graph has no owned WAL, it uses a non-default
    /// filename, a provider cannot encode the ordered generation, or
    /// watermark/snapshot/WAL rotation fails. **Test
    /// [`GraphError::requires_reopen`] to tell the two classes apart** — do not
    /// infer it from the error variant, and do not assume a failure left the
    /// handle usable.
    ///
    /// *Retryable* (`requires_reopen() == false`): provider and
    /// snapshot-preparation failures, which happen before the MANIFEST protocol
    /// begins and consume no WAL sequence, plus a verified artifact collision
    /// that leaves the active epoch unchanged.
    ///
    /// *Requires reopen* (`requires_reopen() == true`, reported as
    /// [`GraphError::IndeterminateOutcome`]): any other error or panic once WAL
    /// rotation starts. By then the watermark record has consumed a physical
    /// sequence and the persistence API can no longer prove which side of the
    /// MANIFEST commit point was reached, so the new epoch may or may not be
    /// published. This includes a MANIFEST ahead of the owned writer: accepting
    /// another commit could reuse a sequence already covered by that snapshot.
    /// The source error's text is preserved in the reason.
    ///
    /// On the second class: quiesce, drop this handle, reopen through
    /// [`SharedGraph::recover`], inspect durable state, and only then retry.
    /// Every later call on the poisoned handle — commit, compact, or another
    /// checkpoint — fails the same way.
    pub fn checkpoint(
        &self,
        config: crate::CheckpointConfig,
    ) -> GraphResult<crate::CheckpointOutcome> {
        reject_provider_callback_reentry("SharedGraph::checkpoint()");
        let committer = self.committer.handle();
        let (seal_seq, generation) = {
            let guard = self.shared.write();
            let generation = guard.meta.generation;
            let seal_seq = committer.next_seal_seq();
            (seal_seq, generation)
        };
        committer.submit_checkpoint(seal_seq, generation, config)
    }

    /// Return compaction pressure for the current published snapshot.
    ///
    /// This is a lock-free read of row counts and liveness counters. It does not
    /// compact, rebuild indexes, or take the writer lock.
    #[must_use]
    pub fn compaction_stats(&self) -> crate::compaction::CompactionStats {
        self.read().compaction_stats()
    }

    /// Compact the live graph in place: reclaim every dead / hole row, renumber
    /// rows dense, and atomically republish the result so the RAM held by deleted
    /// rows is reclaimed immediately (BRIEF-Item-4c — the live-densify half of
    /// snapshot-time compaction).
    ///
    /// This is pure space reclamation: it changes only the internal row layout,
    /// never external `NodeId`/`EdgeId`, properties, or labels, so it emits **no**
    /// [`selene_core::Change`] and writes **no** WAL entry. Durability
    /// comes from the next snapshot, which encodes the now-dense live graph (the
    /// CORE provider reads the same `snapshot` cell this method publishes into). A
    /// crash before that snapshot simply reloads the pre-compaction state and
    /// recompacts later — compaction can never lose data. A coordinated
    /// [`Self::checkpoint`] reserves its own physical WAL watermark, so it can
    /// durably publish this new row layout without waiting for a user mutation.
    ///
    /// The dense graph is built under the write lock on the calling thread
    /// (seal-and-handover, exactly like a commit), and is allocated a publish
    /// `seal_seq` under that same lock; the single committer then swaps it into
    /// the published `snapshot` cell strictly in `seal_seq` order. So compaction
    /// serializes with writers exactly like a commit and can never be reordered
    /// ahead of an earlier-sealed commit (which would let that commit's stale,
    /// non-dense frozen snapshot clobber the dense one). Lock-free readers keep
    /// observing the old snapshot until the dense graph is published. The
    /// monotonic allocator high-water marks are preserved (the live allocator is
    /// untouched, and [`compact_core`](crate::compact_core) carries `GraphMeta`
    /// verbatim — and the allocator is kept in sync with `GraphMeta` on every
    /// commit), so no external id is ever reused after a later recovery.
    ///
    /// # Panics
    ///
    /// Panics when called re-entrantly from a committer-thread provider
    /// callback; nested maintenance would wait behind that same callback.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError`] if the graph's id↔row mapping is corrupt or the
    /// recompacted graph fails its consistency check (see
    /// [`compact_core`](crate::compact_core)).
    pub fn compact(&self) -> GraphResult<crate::CompactionReport> {
        reject_provider_callback_reentry("SharedGraph::compact()");
        // Seal-and-handover for compaction (v1.2 BRIEF 1, P1 fix): build the
        // dense graph HERE, on the caller thread, under the write lock — exactly
        // like a commit seals under the lock — then hand the committer a
        // pre-built dense snapshot to publish in seal_seq order. This keeps the
        // committer off the write lock entirely (no deadlock surface) and, more
        // importantly, ties compaction's publish position to a seal_seq taken
        // under the same lock as commits, so a compact can never be reordered
        // ahead of an earlier-sealed commit (which would otherwise let an
        // earlier commit's stale, non-dense frozen snapshot clobber the dense
        // one in the published cell).
        //
        // Ordering under the lock is load-bearing for the reorder buffer's
        // gap-free invariant: densify FIRST (the only fallible step), and only
        // THEN allocate the seal_seq, so a failed compaction consumes no
        // sequence number (which would otherwise wedge the committer waiting for
        // a seq that never arrives).
        let committer = self.committer.handle();
        let (seal_seq, dense, report) = {
            let mut guard = self.shared.write();
            let compacted = crate::compaction::compact_core(&guard)?;
            let dense = Arc::new(compacted.graph);
            // Allocate the publish-order key under the lock, after the fallible
            // densify, so seal_seq order == lock-acquisition order and no seq is
            // ever burned by a failed compaction.
            let seal_seq = committer.next_seal_seq();
            *guard = Arc::clone(&dense);
            (seal_seq, dense, compacted.report)
            // Lock released here, before the (blocking) enqueue + recv — the
            // committer never needs the write lock, but releasing here also
            // means a compactor never holds the lock while blocked on the
            // committer.
        };
        committer.submit_compact(seal_seq, dense, report)
    }

    /// Rebuild every registered vector index from primary node values.
    ///
    /// HNSW indexes retain stale deleted entries after vector update/delete so
    /// in-flight search can still traverse the neighbor graph safely. This
    /// maintenance path reclaims those stale entries by rebuilding only the
    /// derived vector-index state; it does not change graph data, emit
    /// [`selene_core::Change`], write a WAL entry, bump schema epoch, or
    /// notify providers. The HNSW graph is derived, not durable: snapshots and
    /// recovery persist only vector-index registrations plus primary values, so
    /// a reopen rebuilds the index from that authoritative state.
    ///
    /// The rebuild is strict on live data: if an indexed row no longer satisfies
    /// the registered vector dimension/metric invariant, this method returns an
    /// error instead of silently dropping the row from the index.
    ///
    /// # Panics
    ///
    /// Panics when called re-entrantly from a committer-thread provider
    /// callback; nested maintenance would wait behind that same callback.
    pub fn rebuild_vector_indexes(&self) -> GraphResult<VectorIndexRebuildReport> {
        reject_provider_callback_reentry("SharedGraph::rebuild_vector_indexes()");
        let committer = self.committer.handle();
        let (seal_seq, rebuilt, report) = {
            let mut guard = self.shared.write();
            let mut rebuilt = guard.as_ref().clone();
            let report = crate::vector_index::rebuild_vector_indexes_strict(&mut rebuilt)?;
            let rebuilt = Arc::new(rebuilt);
            let seal_seq = committer.next_seal_seq();
            *guard = Arc::clone(&rebuilt);
            (seal_seq, rebuilt, report)
        };
        committer.submit_vector_index_rebuild(seal_seq, rebuilt, report)
    }

    /// Rebuild only vector indexes whose diagnostics recommend maintenance.
    ///
    /// This is the bounded maintenance variant for IVF drift: it uses each index's current
    /// [`ivf_rebuild_recommended`](crate::vector_index::VectorIndexMemoryUsage::ivf_rebuild_recommended)
    /// value to decide whether to rebuild that derived index. Indexes that do not recommend rebuild
    /// are left untouched, and a no-op call returns an empty report without publishing a maintenance
    /// item.
    ///
    /// The rebuild is strict on live data for selected indexes, matching
    /// [`Self::rebuild_vector_indexes`].
    ///
    /// # Panics
    ///
    /// Panics when called re-entrantly from a committer-thread provider
    /// callback; nested maintenance would wait behind that same callback.
    pub fn rebuild_recommended_vector_indexes(&self) -> GraphResult<VectorIndexRebuildReport> {
        self.maintain_vector_indexes(VectorIndexMaintenancePolicy::recommended())
    }

    /// Maintain recommended vector indexes under a caller-supplied policy.
    ///
    /// This is the explicit orchestration API for amortized vector-index maintenance. It rebuilds
    /// only indexes whose diagnostics currently recommend maintenance and applies the policy cap
    /// after ordering recommended indexes by pending IVF retrain pressure. It remains a
    /// maintenance-tier operation: reads never trigger it, and a no-op call returns an empty report
    /// without publishing a derived-state replacement.
    ///
    /// The rebuild is strict on live data for selected indexes, matching
    /// [`Self::rebuild_vector_indexes`].
    ///
    /// # Panics
    ///
    /// Panics when called re-entrantly from a committer-thread provider
    /// callback; nested maintenance would wait behind that same callback.
    pub fn maintain_vector_indexes(
        &self,
        policy: VectorIndexMaintenancePolicy,
    ) -> GraphResult<VectorIndexRebuildReport> {
        reject_provider_callback_reentry("SharedGraph::maintain_vector_indexes()");
        let committer = self.committer.handle();
        let (seal_seq, rebuilt, report) = {
            let mut guard = self.shared.write();
            let mut rebuilt = guard.as_ref().clone();
            let report = crate::vector_index::maintain_vector_indexes_strict(&mut rebuilt, policy)?;
            if report.entries.is_empty() {
                return Ok(report);
            }
            let rebuilt = Arc::new(rebuilt);
            let seal_seq = committer.next_seal_seq();
            *guard = Arc::clone(&rebuilt);
            (seal_seq, rebuilt, report)
        };
        committer.submit_vector_index_rebuild(seal_seq, rebuilt, report)
    }

    /// Return the runtime schema-version epoch used for plan-cache invalidation.
    ///
    /// The epoch starts at zero for each [`SharedGraph`] instance and advances
    /// only after a successful commit whose change set contains
    /// [`selene_core::Change::SchemaChanged`].
    #[must_use]
    pub fn schema_version(&self) -> u64 {
        self.schema_version.load(Ordering::Acquire)
    }

    /// Return the bound graph type, if this is a closed graph.
    #[must_use]
    pub fn graph_type(&self) -> Option<Arc<GraphTypeDef>> {
        self.read().meta.bound_type.as_ref().map(Arc::clone)
    }

    /// Return true when this graph is bound to a closed graph type.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.read().meta.bound_type.is_some()
    }

    /// Look up a registered provider by tag.
    #[must_use]
    pub fn index_provider_by_tag(&self, tag: ProviderTag) -> Option<Arc<dyn IndexProvider>> {
        self.providers
            .iter()
            .find_map(|provider| (provider.provider_tag() == tag).then(|| Arc::clone(provider)))
    }

    /// Borrow the fixed provider registry for executor procedure contexts.
    #[must_use]
    pub fn index_providers(&self) -> &[Arc<dyn IndexProvider>] {
        &self.providers
    }

    /// Borrow the fixed commit-critical durable provider registry.
    #[must_use]
    pub fn durable_providers(&self) -> &[Arc<dyn DurableProvider>] {
        &self.durable_providers
    }

    /// Begin a write transaction by acquiring the single graph write lock.
    ///
    /// Concurrent writers from other threads queue normally on the write
    /// lock; the engine does **not** panic legitimate concurrent writes during
    /// another committer-thread provider callback.
    ///
    /// Since v1.2 (BRIEF 1) the actual snapshot publish happens on the single
    /// committer thread, not here: `WriteTxn::commit` seals under this lock,
    /// releases it, and hands the frozen bundle to the committer. Commit fan-out
    /// and coordinated checkpoint serialization run on the **committer
    /// thread**, so the re-entrancy guard below protects that thread (sound with
    /// exactly one committer — see `reentry.rs` and the v1.2 design §7.7).
    ///
    /// # Panics
    ///
    /// Panics when called from inside an [`IndexProvider`] callback on the
    /// committer thread. Re-entrant graph operations from a provider callback
    /// are unsupported because their work would wait behind the callback that
    /// initiated them. The enclosing callback boundary catches the panic; the
    /// nested mutation does not run.
    ///
    /// Cross-thread re-entry — a provider spawning a worker thread that
    /// calls `begin_write` and waiting for it — is **documented misuse**
    /// rather than a detectable footgun (the engine cannot trace causal
    /// thread ancestry). See the module docs in `reentry.rs` and the
    /// `IndexProvider` rustdoc for the contract.
    #[must_use]
    #[tracing::instrument(name = "selene.graph.begin_write", skip(self))]
    pub fn begin_write(&self) -> WriteTxn<'_> {
        reject_provider_callback_reentry("SharedGraph::begin_write()");
        WriteTxn::new(
            self.shared.write(),
            self.committer.handle(),
            self.allocator.lock(),
            Arc::clone(&self.providers),
        )
    }

    #[cfg(test)]
    pub(crate) fn locked_arc_ptr_for_test(&self) -> *const SeleneGraph {
        let guard = self.shared.read();
        Arc::as_ptr(&*guard)
    }

    /// Read the generation of the **live RwLock graph** (`*shared`), as opposed
    /// to the published `ArcSwap` snapshot. Used by divergence tests to assert
    /// the two never disagree after a failed / cancelled commit (the P0
    /// WAL-failure + cancel rollback invariants).
    #[cfg(test)]
    pub(crate) fn locked_generation_for_test(&self) -> u64 {
        self.shared.read().meta.generation
    }

    /// Submit an already-[`seal`](crate::WriteTxn::seal)ed commit straight to the
    /// committer, blocking until it is durable + visible. Test-only seam for
    /// exercising the BRIEF-117 cancellation cut-line (which has no production
    /// producer yet) without re-entering `commit_with_principal`.
    #[cfg(test)]
    pub(crate) fn submit_sealed_for_test(
        &self,
        sealed: crate::write_txn::SealedCommit,
    ) -> GraphResult<crate::CommitOutcome> {
        self.committer.handle().submit_commit(sealed)
    }

    /// Enqueue a sealed commit and return its reply receiver without waiting.
    #[cfg(test)]
    pub(crate) fn submit_sealed_async_for_test(
        &self,
        sealed: crate::write_txn::SealedCommit,
    ) -> GraphResult<std::sync::mpsc::Receiver<GraphResult<crate::CommitOutcome>>> {
        self.committer.handle().submit_commit_async_for_test(sealed)
    }
}

pub(crate) fn reject_provider_callback_reentry(operation: &str) {
    assert!(
        !crate::reentry::in_fanout(),
        "selene-graph: {operation} called from within a provider callback on the committer \
         thread; re-entrant graph operations from a provider callback are not supported. \
         The enclosing callback boundary will catch this panic; the nested operation does \
         not run."
    );
}

mod builder;
mod index_ddl;
mod rebuild;
pub use builder::SharedGraphBuilder;
pub(crate) use rebuild::{rebuild_derived_state, validate_unique_provider_tags};

#[cfg(test)]
mod compaction_tests;
#[cfg(test)]
#[path = "shared_property_tests.rs"]
mod property_tests;
#[cfg(test)]
mod tests;
