//! Write transaction RAII handle per spec 03 sections 4 and 6.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use parking_lot::{MutexGuard, RwLockWriteGuard};
use selene_core::Change;

use crate::committer::Committer;
use crate::error::{GraphError, GraphResult};
use crate::graph::SeleneGraph;
use crate::id_allocator::IdAllocator;
use crate::index_provider::IndexProvider;
use crate::mutator::Mutator;
use crate::type_validator::TypeWarning;

mod pipeline;

pub(crate) use pipeline::{AppendedCommit, append_sealed, flush_durables, publish_appended};

#[cfg(test)]
pub(crate) use pipeline::publish_panic_inject;

/// Non-fatal graph commit warning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitWarning {
    /// Closed-graph validation warning.
    pub warning: TypeWarning,
}

/// Result metadata returned after a successful commit.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitOutcome {
    /// Published graph generation.
    pub generation: u64,
    /// Changes produced by the mutation funnel.
    pub changes: Vec<Change>,
    /// Opaque caller-supplied principal bytes for future WAL headers.
    pub principal: Option<Arc<[u8]>>,
    /// Highest durable sequence assigned by commit-critical providers.
    pub durable_at: Option<u64>,
    /// Next node ID after commit.
    pub next_node_id: u64,
    /// Next edge ID after commit.
    pub next_edge_id: u64,
    /// Non-fatal warnings produced during commit validation.
    pub warnings: Vec<CommitWarning>,
}

/// Owned graph snapshot and commit metadata prepared without publication.
///
/// This bundle is the graph-owned staging boundary used by the database facade:
/// it contains the immutable next [`SeleneGraph`] plus the metadata required to
/// shape the existing write outcome, but no lock guard, borrow, [`WriteTxn`],
/// [`crate::SharedGraph`], committer, WAL handle, or provider state. Preparing
/// it does not allocate a graph-local publish sequence, append or flush durable
/// state, store a graph snapshot, bump a live schema epoch, or notify providers.
/// The transaction's rollback remains armed, so its scratch graph is restored
/// when preparation returns while this owned snapshot remains valid.
#[doc(hidden)]
pub struct PreparedGraphCommit {
    next_snapshot: Arc<SeleneGraph>,
    changes: Vec<Change>,
    principal: Option<Arc<[u8]>>,
    schema_changed: bool,
    generation: u64,
    next_node_id: u64,
    next_edge_id: u64,
    warnings: Vec<CommitWarning>,
}

impl PreparedGraphCommit {
    /// Install validated catalog bindings before the facade's sole publication.
    #[doc(hidden)]
    pub fn bind_catalog(
        &mut self,
        catalog: &selene_catalog::CatalogSnapshot,
    ) -> selene_catalog::CatalogResult<()> {
        Arc::make_mut(&mut self.next_snapshot).bind_catalog(catalog)
    }

    /// Borrow the trusted logical changes validated by the graph mutation funnel.
    #[must_use]
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// Borrow the immutable graph snapshot that would become visible.
    #[must_use]
    pub fn snapshot(&self) -> &SeleneGraph {
        &self.next_snapshot
    }

    /// Return the existing in-memory write metadata without publishing.
    #[must_use]
    pub fn outcome(&self) -> CommitOutcome {
        CommitOutcome {
            generation: self.generation,
            changes: self.changes.clone(),
            principal: self.principal.clone(),
            durable_at: None,
            next_node_id: self.next_node_id,
            next_edge_id: self.next_edge_id,
            warnings: self.warnings.clone(),
        }
    }

    /// Return whether the prepared changes alter graph schema.
    #[must_use]
    pub const fn schema_changed(&self) -> bool {
        self.schema_changed
    }

    /// Consume the bundle and return its next graph snapshot.
    #[must_use]
    pub fn into_snapshot(self) -> SeleneGraph {
        Arc::try_unwrap(self.next_snapshot).unwrap_or_else(|snapshot| snapshot.as_ref().clone())
    }
}

/// Compile-time proof that facade-owned prepared graph state is lifetime-free.
const _: fn() = || {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<PreparedGraphCommit>();
};

/// A frozen, owned, `Send + 'static` commit bundle handed from a session thread
/// to the single committer thread (v1.2 multi-writer, BRIEF 1).
///
/// Produced by [`WriteTxn::seal`] **after** the generation/meta bump + GG02
/// validation have run under the write lock on the session thread (so error
/// timing is unchanged), and after the lock + allocator guards have been
/// released. It contains the fully-built next snapshot plus everything the
/// committer needs to run the durable+publish tail — **no guards, no graph
/// reference, no borrow**. The committer never re-validates, re-allocates ids,
/// or re-applies a change list; it just stamps the HLC, appends to the WAL,
/// publishes the frozen snapshot, and bumps the schema epoch.
///
/// The HLC timestamp is deliberately **not** stamped here: the committer stamps
/// it per bundle in **seal-sequence** drain order so HLC is monotonic in commit
/// order (== publish order == seal order). Stamping it on the session thread
/// would break that monotonicity once seal-order and stamp-order diverge.
///
/// # Why a `seal_seq` (publish-order correctness, P0 fix)
///
/// `seal()` consumes the [`WriteTxn`], so the write lock + allocator guards drop
/// as it returns — **before** the caller enqueues the bundle. Two sessions can
/// therefore seal in lock order (A then B) yet `send()` in the opposite order
/// (B before A) if A is preempted between lock-release and send. A naive FIFO
/// committer would then publish B's gen-`N+1` snapshot before A's gen-`N`
/// snapshot, regressing the published snapshot and losing A's older view under
/// B — a D10 serializability violation. To prevent it, `seal()` stamps a
/// strictly-monotonic `seal_seq` **while still holding the write lock** (so
/// seal-seq order == lock-acquisition order == the intended total order), and
/// the committer publishes strictly in `seal_seq` order via a reorder buffer,
/// regardless of channel arrival order. Compaction takes a `seal_seq` from the
/// same counter under the same lock, so a compact can never be reordered ahead
/// of an earlier-sealed commit.
pub(crate) struct SealedCommit {
    /// Strictly-monotonic publish-order key, allocated under the write lock in
    /// [`WriteTxn::seal`]. The committer publishes in ascending `seal_seq`.
    pub(crate) seal_seq: u64,
    /// Fully-built next snapshot, frozen under the session's write lock.
    pub(crate) next_snapshot: Arc<SeleneGraph>,
    /// Persisted change list (the WAL/changeset payload).
    pub(crate) changes: Vec<Change>,
    /// Truncate-expanded fan-out view, built on the session thread, or `None`
    /// when no truncate/reset expansion is staged (the common path).
    pub(crate) fanout_changes: Option<Vec<Change>>,
    /// Opaque caller-supplied principal bytes for the WAL entry header (D12).
    pub(crate) principal: Option<Arc<[u8]>>,
    /// Whether the change list bumps the schema epoch.
    pub(crate) schema_changed: bool,
    /// Already-bumped graph generation.
    pub(crate) generation: u64,
    /// Next node id after this commit (peeked under the lock).
    pub(crate) next_node_id: u64,
    /// Next edge id after this commit (peeked under the lock).
    pub(crate) next_edge_id: u64,
    /// Non-fatal validation warnings collected during seal.
    pub(crate) warnings: Vec<CommitWarning>,
}

/// Compile-time proof that [`SealedCommit`] is `Send + 'static`, so it can be
/// moved to the committer thread. A guard or borrow leaking into the struct
/// would fail this assertion at compile time.
const _: fn() = || {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<SealedCommit>();
};

struct PreparedCommitParts {
    prepared: PreparedGraphCommit,
    fanout_changes: Option<Vec<Change>>,
}

/// RAII owner of the single graph write lock.
///
/// Since v1.2 (BRIEF 1) the transaction no longer holds the snapshot cell,
/// schema-version, or provider handles — those moved to the single committer
/// thread, which the transaction reaches via the cheap `Committer` submit
/// handle. The transaction still owns the write lock + allocator guards for the
/// duration of execution and releases them when `seal` consumes it.
pub struct WriteTxn<'g> {
    pub(crate) guard: RwLockWriteGuard<'g, Arc<SeleneGraph>>,
    pub(crate) committer: Committer,
    pub(crate) pre_txn: Option<Arc<SeleneGraph>>,
    pub(crate) allocator: MutexGuard<'g, IdAllocator>,
    /// Index-provider registry, retained so `Mutator::index_provider_by_tag`
    /// can resolve a provider during execution. Shares the one frozen
    /// registry allocation with `SharedGraph` and the committer — handing it
    /// to a transaction is a refcount bump, not a `Vec` clone.
    pub(crate) providers: Arc<[Arc<dyn IndexProvider>]>,
    pub(crate) changes: Vec<Change>,
    /// Per-truncate per-row tombstone expansions, keyed by the index of the
    /// declarative truncate change in [`Self::changes`] that produced them.
    ///
    /// BRIEF-150 / deletion-reclamation audit Item 11. The WAL/changeset carries
    /// only the O(1) declarative `NodesOfTypeTruncated`/`EdgesOfTypeTruncated`
    /// change, but live index-provider fan-out must observe the same per-row
    /// `NodeDeleted`/`EdgeDeleted` multiset a `MATCH (n:L) DETACH DELETE n` would
    /// emit (so derived state is reclaimed without leaks). The mutator
    /// snapshots the matched ids while it still holds the store and stages their
    /// tombstones here; commit substitutes each truncate change with its staged
    /// expansion before fan-out.
    pub(crate) truncate_expansions: Vec<(usize, Vec<Change>)>,
    pub(crate) warnings: Vec<CommitWarning>,
}

impl<'g> WriteTxn<'g> {
    pub(crate) fn new(
        guard: RwLockWriteGuard<'g, Arc<SeleneGraph>>,
        committer: Committer,
        allocator: MutexGuard<'g, IdAllocator>,
        providers: Arc<[Arc<dyn IndexProvider>]>,
    ) -> Self {
        let pre_txn = Some(Arc::clone(&*guard));
        Self {
            guard,
            committer,
            pre_txn,
            allocator,
            providers,
            changes: Vec::new(),
            truncate_expansions: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Borrow a mutator tied to this transaction.
    #[must_use]
    pub fn mutator(&mut self) -> Mutator<'_, 'g> {
        Mutator::new(self)
    }

    /// Borrow the transaction-local working graph.
    #[must_use]
    pub fn read(&self) -> &SeleneGraph {
        self.guard.as_ref()
    }

    pub(crate) fn guard_mut(&mut self) -> &mut SeleneGraph {
        Arc::make_mut(&mut *self.guard)
    }

    /// Commit without caller principal bytes.
    pub fn commit(self) -> GraphResult<CommitOutcome> {
        self.commit_with_principal(None)
    }

    /// Commit with optional caller-owned principal bytes for D12 audit replay.
    ///
    /// Since v1.2 (BRIEF 1) commit is **seal-and-handover**: this method runs
    /// `seal` on the calling thread (generation/meta bump + GG02
    /// validation under the write lock, then **lock release**), then submits the
    /// resulting `SealedCommit` to the per-graph single committer thread and
    /// blocks until it is durable + visible. The public contract is unchanged —
    /// "`commit()` returns ⇒ durable + visible" — only the internal threading
    /// model differs.
    ///
    /// GG02 closed-graph violations still abort here, on the calling thread,
    /// before any handoff, so error timing is identical to v1.0/v1.1.
    ///
    /// Registered index providers are notified by the committer after the new
    /// snapshot is published; the same-thread re-entrancy guard now protects the
    /// committer thread (one committer ⇒ still sound). Same-thread re-entrant
    /// provider calls into `SharedGraph::begin_write()` are detected via the
    /// thread-local fanout counter and panic with a clear message; the
    /// committer's fan-out boundary catches those panics (along with
    /// callback-internal panics and returned errors) so a single misbehaving
    /// provider can never crash the committer thread.
    ///
    /// # Errors
    ///
    /// Returns the GG02 / validation error from `seal` — a definite rejection,
    /// nothing was written — or, once the commit has reached the durable path,
    /// [`GraphError::IndeterminateOutcome`].
    ///
    /// **An `Err` from this method does not mean "the transition did not
    /// happen."** Past `seal` the engine cannot promise ISO §8.4 GR 1)b)'s "any
    /// changes ... are canceled": the WAL bytes may already be written, or
    /// written and fsynced, and a reopen replays them. Treat
    /// [`GraphError::IndeterminateOutcome`] as an outcome of unknown durability
    /// — quiesce, reopen through [`crate::SharedGraph::recover`], read back to
    /// see whether it landed, and only then decide whether to retry. Retrying
    /// blind double-applies. See that variant for the full contract.
    #[tracing::instrument(
        name = "selene.graph.commit",
        skip(self, principal),
        fields(change_count = self.change_count())
    )]
    pub fn commit_with_principal(self, principal: Option<Arc<[u8]>>) -> GraphResult<CommitOutcome> {
        // Clone the submit handle BEFORE sealing — `seal()` consumes `self`
        // (dropping the write lock + allocator guards as it returns), so the
        // session releases the lock strictly before it enqueues and blocks on
        // `recv()`. The committer never takes the write lock (compaction builds
        // on the caller thread too, since v1.2 BRIEF 1 P0 fix), so a session is
        // never simultaneously lock-holding and recv-blocked.
        let committer = self.committer.clone();
        let sealed = self.seal(principal, None)?;
        committer.submit_commit(sealed)
    }

    /// Validate and freeze this transaction without graph-local publication.
    ///
    /// This is intended for a closure-local, CORE-only scratch
    /// [`crate::SharedGraph`] whose returned snapshot will be published by a
    /// higher-level authority. It performs the same generation/next-ID update,
    /// GG02 and unique validation, warning collection, and cancellation sample
    /// as [`WriteTxn::seal`], but deliberately leaves rollback armed and does not
    /// allocate a `seal_seq` or contact the committer or providers.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Cancelled`] when `cancel` is set at the preparation
    /// cut-line, or the same graph-type validation errors as a normal commit.
    #[doc(hidden)]
    pub fn prepare_unpublished(
        mut self,
        principal: Option<Arc<[u8]>>,
        cancel: Option<&AtomicBool>,
    ) -> GraphResult<PreparedGraphCommit> {
        let parts = self.prepare_commit_parts(principal, cancel)?;
        Ok(parts.prepared)
    }

    /// Run the under-lock half of commit and hand back an owned, `Send`
    /// [`SealedCommit`] for the committer thread (v1.2 multi-writer, BRIEF 1).
    ///
    /// Runs, on the calling thread under the held write lock: schema-change
    /// detection, the generation/meta bump (`generation += 1` + write the next
    /// ids into `GraphMeta`), and GG02 closed-graph validation — which **still
    /// aborts synchronously here** (the `?` propagates and `Drop` rolls back the
    /// generation bump, exactly as in v1.0/v1.1). It then samples the optional
    /// BRIEF-117 cancellation token **while the lock is still held and before
    /// `Drop` is disarmed** (see below), allocates the strictly-monotonic
    /// `seal_seq` under the lock, disarms `Drop` (`pre_txn = None`), clones the
    /// now-frozen next snapshot, `mem::take`s the change list / truncate
    /// expansions / warnings, builds the truncate-expanded fan-out view, and
    /// returns. The write lock + allocator guards drop as this method returns
    /// (it consumes `self`).
    ///
    /// # Cancellation cut-line (BRIEF-117, P0 fix)
    ///
    /// The cancellation token is sampled **here, under the write lock, before
    /// disarming `Drop`** — not on the committer before the WAL append. In the
    /// seal-and-handover model multiple commits can be sealed-but-unpublished at
    /// once, each forking `*shared` off the previous; a commit's mutation is
    /// therefore already woven into `*shared` (and possibly built upon by a
    /// later seal) by the time the committer would run a pre-WAL check, so a
    /// committer-side cancel could not surgically remove it without poisoning
    /// the engine. Sampling under the lock means a cancelled commit is rolled
    /// back by `Drop` exactly like a GG02 abort — `*shared` is restored, no
    /// `seal_seq` is consumed, nothing is enqueued — so the cut-line's
    /// guarantee ("no append, no publish, exactly as an aborted transaction
    /// would leave it") is *literally* true. A cancel observed after `seal`
    /// returns is too late: the commit is already in flight and irrevocable.
    ///
    /// The HLC timestamp is **not** stamped here (see [`SealedCommit`]); the
    /// committer stamps it per bundle in seal-sequence drain order.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::Cancelled`] when `cancel` is set at entry (rolled
    /// back via `Drop`), or the GG02 / closed-graph validation error
    /// ([`GraphError::TypeViolation`]) when a change violates the bound type.
    pub(crate) fn seal(
        mut self,
        principal: Option<Arc<[u8]>>,
        cancel: Option<&AtomicBool>,
    ) -> GraphResult<SealedCommit> {
        debug_assert!(
            self.pre_txn.is_some(),
            "pre_txn must be present at seal entry"
        );

        let PreparedCommitParts {
            prepared,
            fanout_changes,
        } = self.prepare_commit_parts(principal, cancel)?;

        // Allocate the publish-order key under the lock so seal-seq order equals
        // lock-acquisition order (the intended total order). Preparation has
        // completed every fallible step, keeping aborted sequences gap-free.
        let seal_seq = self.committer.next_seal_seq();

        // Disarm Drop-rollback: from here the commit is handed to the committer
        // and the in-place mutations become the published state.
        self.pre_txn = None;

        Ok(SealedCommit {
            seal_seq,
            next_snapshot: prepared.next_snapshot,
            changes: prepared.changes,
            fanout_changes,
            principal: prepared.principal,
            schema_changed: prepared.schema_changed,
            generation: prepared.generation,
            next_node_id: prepared.next_node_id,
            next_edge_id: prepared.next_edge_id,
            warnings: prepared.warnings,
        })
    }

    fn prepare_commit_parts(
        &mut self,
        principal: Option<Arc<[u8]>>,
        cancel: Option<&AtomicBool>,
    ) -> GraphResult<PreparedCommitParts> {
        for change in &self.changes {
            change.validate_stored_values()?;
        }
        let schema_changed = self
            .changes
            .iter()
            .any(|change| matches!(change, Change::SchemaChanged { .. }));
        let next_node_id = self.allocator.peek_next_node();
        let next_edge_id = self.allocator.peek_next_edge();
        {
            let graph = self.guard_mut();
            graph.meta.generation =
                graph
                    .meta
                    .generation
                    .checked_add(1)
                    .ok_or(GraphError::CounterExhausted {
                        kind: "graph generation",
                    })?;
            graph.meta.next_node_id = next_node_id;
            graph.meta.next_edge_id = next_edge_id;
        }

        let generation = self.read().meta.generation;

        let mut validation_warnings = Vec::new();
        if let Some(type_def) = self.read().meta.bound_type.as_deref() {
            for change in &self.changes {
                validation_warnings.extend(
                    // NB: `?` here returns Err with the generation bump still
                    // applied to the guard-Arc; `Drop` then restores `pre_txn`
                    // and undoes the bump — error timing + rollback unchanged.
                    crate::type_validator::validate_change(change, self.read(), type_def)?
                        .into_iter()
                        .map(|warning| CommitWarning { warning }),
                );
            }
            if schema_changed {
                validation_warnings.extend(
                    crate::type_validator::validate_entity_state(self.read(), type_def)?
                        .into_iter()
                        .map(|warning| CommitWarning { warning }),
                );
            } else {
                crate::type_validator::validate_unique_property_changes(
                    &self.changes,
                    self.read(),
                    type_def,
                )?;
            }
        }
        for warning in validation_warnings {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }

        // BRIEF-117 cut-line: sample the cancellation token while the lock is
        // still held and `Drop` is still armed. A cancel here returns Err with
        // the generation bump + staged mutations still on the guard-Arc; `Drop`
        // then restores `pre_txn`, rolling everything back exactly as a GG02
        // abort or an aborted transaction would. Nothing is enqueued or
        // published, and no `seal_seq` is consumed.
        if let Some(flag) = cancel
            && flag.load(Ordering::Acquire)
        {
            return Err(GraphError::Cancelled);
        }

        // Freeze the next snapshot under the lock. Unpublished preparation keeps
        // this Arc while Drop restores the scratch guard's prior Arc.
        let next_snapshot = Arc::clone(&*self.guard);

        let changes = std::mem::take(&mut self.changes);
        let truncate_expansions = std::mem::take(&mut self.truncate_expansions);
        let warnings = std::mem::take(&mut self.warnings);

        // BRIEF-150 / audit Item 11: build the fan-out-only truncate-expanded
        // view on the session thread so the committer holds a fully-owned
        // bundle. `None` on the common (non-truncate) path → zero allocation.
        let fanout_changes = pipeline::expand_truncates_for_fanout(&changes, &truncate_expansions);

        Ok(PreparedCommitParts {
            prepared: PreparedGraphCommit {
                next_snapshot,
                changes,
                principal,
                schema_changed,
                generation,
                next_node_id,
                next_edge_id,
                warnings,
            },
            fanout_changes,
        })
    }

    /// Roll back graph changes via `Drop` and release the write lock.
    pub fn rollback(self) {}

    /// Number of changes accumulated since this transaction opened.
    #[must_use]
    pub fn change_count(&self) -> usize {
        self.changes.len()
    }

    /// Whether this transaction has accumulated schema-changing work.
    #[must_use]
    pub fn has_schema_changes(&self) -> bool {
        self.changes
            .iter()
            .any(|change| matches!(change, Change::SchemaChanged { .. }))
    }
}

impl Drop for WriteTxn<'_> {
    fn drop(&mut self) {
        if let Some(prior) = self.pre_txn.take() {
            *self.guard = prior;
        }
    }
}

#[cfg(test)]
mod tests;
