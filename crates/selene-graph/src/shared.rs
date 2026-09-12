//! Shared in-memory graph with lock-free reads and serialized publication.

#[cfg(debug_assertions)]
use crate::GraphError;
use crate::{
    GraphResult, GraphTypeDef, IndexProvider, ProviderTag, SeleneGraph,
    VectorIndexMaintenancePolicy, VectorIndexRebuildReport, WriteTxn, id_allocator::IdAllocator,
};
use arc_swap::ArcSwap;
use parking_lot::{Mutex, RwLock};
use selene_core::GraphId;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

mod allocation;
pub use allocation::GraphAllocationAuthority;
mod builder;
mod candidates;
mod index_ddl;
mod rebuild;
pub use builder::SharedGraphBuilder;
pub(crate) use rebuild::{rebuild_derived_state, validate_unique_provider_tags};

/// Memory-only graph runtime for native/internal graph use.
///
/// Writers seal under one write lock; one committer publishes immutable snapshots
/// in seal order, even when queue arrival order differs. Observer failures cannot
/// undo publication. This is not a persisted database constructor: durable
/// create/open/checkpoint/verify belong to the `selene-db` facade authority.
pub struct SharedGraph {
    shared: Arc<RwLock<Arc<SeleneGraph>>>,
    snapshot: Arc<ArcSwap<SeleneGraph>>,
    schema_version: Arc<AtomicU64>,
    allocator: Arc<Mutex<IdAllocator>>,
    providers: Arc<[Arc<dyn IndexProvider>]>,
    // Dropped last; closes the queue and joins the sole publisher.
    committer: crate::committer::CommitterThread,
}

impl SharedGraph {
    /// Construct an empty in-memory shared graph.
    #[must_use]
    pub fn new(graph_id: GraphId) -> Self {
        Self::from_graph(SeleneGraph::new(graph_id))
    }

    /// Start building an in-memory graph with optional observers.
    #[must_use]
    pub fn builder(graph_id: GraphId) -> SharedGraphBuilder {
        SharedGraphBuilder::new(graph_id)
    }

    /// Construct from a snapshot, rebuilding indexes and establishing new layout ownership.
    ///
    /// # Panics
    /// Panics for inconsistent data, invalid types/indexes, or excessive row counts.
    /// Use [`Self::try_from_graph`] for fallible admission.
    #[must_use]
    pub fn from_graph(graph: SeleneGraph) -> Self {
        Self::try_from_graph(graph).expect("invalid graph snapshot")
    }

    /// Validate and construct memory-only shared state from a snapshot.
    pub fn try_from_graph(graph: SeleneGraph) -> GraphResult<Self> {
        Self::from_graph_with_providers(graph, Vec::new())
    }

    /// Construct with fixed observers and private first-party catalog candidate state.
    /// Validates data and unique tags; caller observers cannot override catalog rules.
    pub fn from_graph_with_providers(
        mut graph: SeleneGraph,
        mut providers: Vec<Arc<dyn IndexProvider>>,
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
        graph.remint_layout();
        rebuild_derived_state(&mut graph)?;
        crate::property_index::rebuild_property_indexes(&mut graph)?;
        crate::property_index::rebuild_edge_property_indexes(&mut graph)?;
        crate::composite_property_index::rebuild_composite_property_indexes(&mut graph)?;
        crate::vector_index::rebuild_vector_indexes(&mut graph)?;
        crate::text_index::rebuild_text_indexes(&mut graph)?;
        if let Some(provider) = candidates::prepare(&graph)? {
            providers.push(provider);
            validate_unique_provider_tags(&providers)?;
        }
        let providers: Arc<[Arc<dyn IndexProvider>]> = providers.into();
        if let Some(type_def) = graph.meta.bound_type.as_deref() {
            type_def.validate_ref()?;
            crate::type_validator::validate_entity_state(&graph, type_def)?;
        }
        let node_floor = (graph.node_store.labels.len() as u64).saturating_add(1);
        let edge_floor = (graph.edge_store.label.len() as u64).saturating_add(1);
        let allocator = IdAllocator::from_meta_with_floors(&graph.meta, node_floor, edge_floor);
        #[cfg(debug_assertions)]
        if let Err(reason) = graph.assert_indexes_consistent() {
            return Err(GraphError::Inconsistent {
                reason: format!("rebuilt snapshot failed index consistency check: {reason}"),
            });
        }
        let graph = Arc::new(graph);
        let snapshot = Arc::new(ArcSwap::from(Arc::clone(&graph)));
        let shared = Arc::new(RwLock::new(graph));
        let schema_version = Arc::new(AtomicU64::new(0));
        let allocator = Arc::new(Mutex::new(allocator));
        let committer =
            crate::committer::CommitterThread::spawn(crate::committer::CommitterHandles {
                snapshot: Arc::clone(&snapshot),
                schema_version: Arc::clone(&schema_version),
                providers: Arc::clone(&providers),
            });
        Ok(Self {
            shared,
            snapshot,
            schema_version,
            allocator,
            providers,
            committer,
        })
    }

    /// Load the current immutable snapshot without taking the write lock.
    #[must_use]
    pub fn read(&self) -> Arc<SeleneGraph> {
        self.snapshot.load_full()
    }

    /// Return lock-free compaction pressure without triggering maintenance.
    #[must_use]
    pub fn compaction_stats(&self) -> crate::compaction::CompactionStats {
        self.read().compaction_stats()
    }

    /// Reclaim dead/hole rows without changing stable identity or allocation floors.
    ///
    /// Build under the write lock before allocating a seal sequence: a failed
    /// compaction cannot leave a sequence gap. Publication uses the same ordered
    /// queue as mutations, never a second snapshot writer. Held snapshots stay valid.
    /// Panics on same-thread observer re-entry; cross-thread callback waits are unsupported.
    pub fn compact(&self) -> GraphResult<crate::CompactionReport> {
        reject_provider_callback_reentry("SharedGraph::compact()");
        let committer = self.committer.handle();
        let (seal_seq, dense, report) = {
            let mut guard = self.shared.write();
            let compacted = crate::compaction::compact_core(&guard)?;
            let dense = Arc::new(compacted.graph);
            let seal_seq = committer.next_seal_seq();
            *guard = Arc::clone(&dense);
            (seal_seq, dense, compacted.report)
        };
        committer.submit_compact(seal_seq, dense, report)
    }

    /// Rebuild every registered vector index strictly from primary values.
    /// Changes only derived state, not generation, schema epoch or observer state.
    /// Panics on same-thread observer re-entry.
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
    pub fn rebuild_recommended_vector_indexes(&self) -> GraphResult<VectorIndexRebuildReport> {
        self.maintain_vector_indexes(VectorIndexMaintenancePolicy::recommended())
    }

    /// Apply explicit bounded vector maintenance; reads never trigger it.
    /// A no-op publishes nothing. Panics on same-thread observer re-entry.
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

    /// Schema-version epoch, advanced strictly after a schema-changing publication.
    #[must_use]
    pub fn schema_version(&self) -> u64 {
        self.schema_version.load(Ordering::Acquire)
    }
    /// Return the bound graph type, if closed.
    #[must_use]
    pub fn graph_type(&self) -> Option<Arc<GraphTypeDef>> {
        self.read().meta.bound_type.as_ref().map(Arc::clone)
    }
    /// Whether the graph is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.read().meta.bound_type.is_some()
    }
    /// Look up a fixed observer by tag.
    #[must_use]
    pub fn index_provider_by_tag(&self, tag: ProviderTag) -> Option<Arc<dyn IndexProvider>> {
        self.providers
            .iter()
            .find_map(|p| (p.provider_tag() == tag).then(|| Arc::clone(p)))
    }
    /// Borrow observers for native procedure contexts.
    #[must_use]
    pub fn index_providers(&self) -> &[Arc<dyn IndexProvider>] {
        &self.providers
    }

    /// Acquire the single graph write lock. Commit seals here and publishes on the
    /// sole committer after releasing this lock. Concurrent writers queue normally.
    ///
    /// # Panics
    /// Same-thread observer re-entry panics before acquiring a lock; its enclosing
    /// observer boundary catches the panic. Cross-thread callback waits on graph
    /// work are unsupported and can deadlock.
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
        Arc::as_ptr(&*self.shared.read())
    }
    #[cfg(test)]
    pub(crate) fn locked_generation_for_test(&self) -> u64 {
        self.shared.read().meta.generation
    }
    #[cfg(test)]
    pub(crate) fn submit_sealed_for_test(
        &self,
        sealed: crate::write_txn::SealedCommit,
    ) -> GraphResult<crate::CommitOutcome> {
        self.committer.handle().submit_commit(sealed)
    }
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
        "selene-graph: {operation} called from within a provider callback on the committer thread; re-entrant graph operations from a provider callback are not supported. The enclosing callback boundary will catch this panic; the nested operation does not run."
    );
}

#[cfg(test)]
mod compaction_tests;
#[cfg(test)]
#[path = "shared_property_tests.rs"]
mod property_tests;
#[cfg(test)]
mod tests;
