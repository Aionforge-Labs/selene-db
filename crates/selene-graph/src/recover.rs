//! SharedGraph recovery helpers backed by selene-persist.

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;

use selene_core::{Change, GraphId};
use selene_persist::{
    AuditLog, DEFAULT_AUDIT_FILE_NAME, DEFAULT_WAL_FILE_NAME, PersistError, PersistenceReadGuard,
    ProviderRegistry, RecoveryError, RecoveryProvider, RecoveryResult, SyncPolicy, WalConfig,
    WalWriter, recover_guarded,
};

use crate::core_provider::CoreProvider;
use crate::graph_types::GraphTypeDef;
use crate::index_provider::{IndexProvider, ProviderError, SubTag};
use crate::shared::validate_unique_provider_tags;
use crate::{GraphError, GraphResult, SharedGraph};

impl SharedGraph {
    /// Recover an open (GG01) shared graph from a persistence directory.
    ///
    /// `graph_id` is the caller-asserted identity, and recovery fails if
    /// anything the directory declares disagrees with it: a snapshot's
    /// `CORE/META`, or the graph id stamped on any schema record replayed from
    /// the WAL. A WAL carrying no schema record declares no identity, so there
    /// the assertion is taken on trust. If a snapshot is present and declares a
    /// `bound_type`, recovery fails — a closed graph must be recovered via
    /// [`SharedGraph::recover_closed`].
    ///
    /// # Errors
    ///
    /// Returns persistence or epoch-lock errors, including
    /// [`selene_persist::PersistError::WriterLockHeld`] when another live graph
    /// owns the WAL; [`crate::GraphError::Provider`] when a snapshot or a
    /// replayed schema record disagrees with `graph_id`, or when a snapshot
    /// declares a closed binding; or graph errors when the recovered state
    /// cannot be materialized.
    pub fn recover(dir: &Path, graph_id: GraphId) -> GraphResult<Self> {
        Self::recover_in(&selene_persist::StoreDirectory::open(dir)?, graph_id)
    }

    /// Recover an open graph through a retained directory capability.
    ///
    /// # Errors
    /// Returns the same locking, replay, and graph-validation errors as [`Self::recover`].
    pub fn recover_in(
        dir: &selene_persist::StoreDirectory,
        graph_id: GraphId,
    ) -> GraphResult<Self> {
        Self::recover_inner(dir, graph_id, None, Vec::new())
    }

    /// Recover an open (GG01) shared graph with extension index providers.
    ///
    /// Each supplied provider participates in snapshot-section recovery and WAL
    /// replay, then remains attached to the returned graph so future commits
    /// route to the same provider set.
    ///
    /// # Errors
    ///
    /// Returns persistence errors, [`crate::GraphError::Provider`] when provider
    /// tags are duplicated or recovered provider state is inconsistent, or graph
    /// errors when the recovered state cannot be materialized.
    pub fn recover_with_providers(
        dir: &Path,
        graph_id: GraphId,
        providers: Vec<Arc<dyn IndexProvider>>,
    ) -> GraphResult<Self> {
        Self::recover_inner(
            &selene_persist::StoreDirectory::open(dir)?,
            graph_id,
            None,
            providers,
        )
    }

    /// Recover a closed (GG02) shared graph bound to `bound_type`.
    ///
    /// `graph_id` and `bound_type` are caller-asserted; recovery validates
    /// against the snapshot:
    ///
    /// - If the snapshot's `CORE/META` references a `bound_type` via
    ///   `CORE/GTYP`, it must equal `bound_type` or recovery fails (drift).
    /// - If the snapshot declares no binding but `bound_type` is provided,
    ///   recovery fails (snapshot says open, caller says closed).
    /// - If no snapshot is present (WAL-only or empty-dir), the caller's
    ///   `bound_type` is used and validation runs against replayed state.
    ///
    /// `graph_id` is validated independently of the binding, against
    /// `CORE/META` when a snapshot is present and against the graph id stamped
    /// on any schema record replayed from the WAL. See
    /// [`SharedGraph::recover`] for the limits of the WAL-side check.
    ///
    /// # Errors
    ///
    /// Returns persistence errors, [`crate::GraphError::Provider`] on type
    /// drift / inconsistency, [`crate::GraphError::TypeViolation`] when
    /// recovered entities don't conform to `bound_type`, or graph errors
    /// when the recovered state cannot be materialized.
    pub fn recover_closed(
        dir: &Path,
        graph_id: GraphId,
        bound_type: GraphTypeDef,
    ) -> GraphResult<Self> {
        Self::recover_inner(
            &selene_persist::StoreDirectory::open(dir)?,
            graph_id,
            Some(Arc::new(bound_type)),
            Vec::new(),
        )
    }

    /// Recover a closed (GG02) shared graph with extension index providers.
    ///
    /// This is the provider-aware counterpart to [`SharedGraph::recover_closed`].
    /// Recovery validates the caller-supplied `bound_type`, rebuilds the supplied
    /// index providers from snapshot sections and WAL replay, and returns a live
    /// WAL-backed graph with those providers still attached.
    ///
    /// # Errors
    ///
    /// Returns persistence errors, [`crate::GraphError::Provider`] on provider
    /// tag duplication or type drift,
    /// [`crate::GraphError::TypeViolation`] when recovered entities don't
    /// conform to `bound_type`, or graph errors when the recovered state cannot
    /// be materialized.
    pub fn recover_closed_with_providers(
        dir: &Path,
        graph_id: GraphId,
        bound_type: GraphTypeDef,
        providers: Vec<Arc<dyn IndexProvider>>,
    ) -> GraphResult<Self> {
        Self::recover_inner(
            &selene_persist::StoreDirectory::open(dir)?,
            graph_id,
            Some(Arc::new(bound_type)),
            providers,
        )
    }

    fn recover_inner(
        dir: &selene_persist::StoreDirectory,
        graph_id: GraphId,
        expected_bound_type: Option<Arc<GraphTypeDef>>,
        providers: Vec<Arc<dyn IndexProvider>>,
    ) -> GraphResult<Self> {
        // Bind recovery, live WAL reopen, and audit reopen to one physical
        // directory so a caller alias cannot retarget between those phases.
        let core = CoreProvider::new_for_recovery();
        validate_recovery_provider_tags(&core, &providers)?;
        let mut attachments = RecoveryAttachmentGuard::reserve(&providers)?;
        let mut registry = ProviderRegistry::new();
        let provider: Arc<dyn RecoveryProvider> = core.clone();
        registry.register(provider)?;
        register_index_recovery_providers(&mut registry, &providers)?;

        // A graph recovery is a writer takeover, not an online inspection. An
        // existing WAL is locked before the shared MANIFEST epoch. When no WAL
        // exists, verify recovery under the shared guard before publishing a
        // seeded WAL; WalWriter acquisition is non-blocking, so a racing writer
        // fails this takeover without creating a lock-order cycle.
        let wal_existed = dir.contains(DEFAULT_WAL_FILE_NAME)?;
        let (writer, read_guard, outcome) = if wal_existed {
            let writer = open_recovery_writer(dir, 0)?;
            let read_guard = PersistenceReadGuard::acquire_in(dir)?;
            let outcome = recover_guarded(&read_guard, &registry)?;
            (writer, read_guard, outcome)
        } else {
            let read_guard = PersistenceReadGuard::acquire_in(dir)?;
            let outcome = recover_guarded(&read_guard, &registry)?;
            #[cfg(test)]
            run_before_missing_wal_open_hook();
            let writer = open_recovery_writer(dir, outcome.applied_snapshot_seq)?;
            (writer, read_guard, outcome)
        };

        if !wal_existed && writer.snapshot_seq() != outcome.applied_snapshot_seq {
            return Err(PersistError::WalSnapshotMismatch {
                wal_snapshot_seq: writer.snapshot_seq(),
                snapshot_seq: outcome.applied_snapshot_seq,
            }
            .into());
        }

        // The retained writer and guarded replay scanned the same immutable
        // WAL prefix. Exact agreement prevents later commits from reusing a
        // sequence at or below the recovered replay floor, while accepting the
        // legitimate Phase-3 state whose lagging-header WAL still reaches the
        // MANIFEST snapshot sequence through its entries.
        if writer.last_sequence() != outcome.last_wal_seq {
            return Err(PersistError::WalRecoverySequenceMismatch {
                writer_sequence: writer.last_sequence(),
                recovered_sequence: outcome.last_wal_seq,
            }
            .into());
        }
        let mut graph = core.finish_recovery(graph_id, expected_bound_type)?;
        // Physical WAL sequences include checkpoint watermarks and can diverge
        // from logical graph generations. Advance the snapshot META generation
        // once per replayed logical commit (including ordinary empty commits),
        // never once per physical watermark or Change value.
        graph.meta.generation = graph
            .meta
            .generation
            .checked_add(outcome.wal_commit_entries_applied)
            .ok_or_else(|| GraphError::Inconsistent {
                reason: "graph generation overflow while applying recovered WAL commits".to_owned(),
            })?;
        mark_recovered_provider_generation(&providers, &graph, &outcome)?;
        #[cfg(test)]
        run_after_persist_recovery_hook();
        drop(read_guard);

        // The retained writer makes post-recovery commits durable. The single
        // committer is the sole fsync caller, so OnFlushOnly still yields one
        // flush per commit under CommitBatching::Off below.
        // Reattach the audit log iff one exists on disk (its `SLAU` presence
        // means the embedder enabled audit) so post-recovery lifecycle commits
        // keep being mirrored. The historical events already persist in the file
        // — recovery never re-derives them, and the WAL replay above does not
        // re-mirror (write_commit is live-only). Absent file → no audit.
        let audit_log = if dir.contains(DEFAULT_AUDIT_FILE_NAME)? {
            Some(AuditLog::open_with_authority(
                writer.authority(),
                Path::new(DEFAULT_AUDIT_FILE_NAME),
            )?)
        } else {
            None
        };
        // Read before the writer moves into the committer's durable state; this
        // is the only point where recovery still owns it. Reporting the repair
        // is why it is read at all — `Ok` here otherwise looks identical to a
        // reopen that lost an unacknowledged commit (#1109).
        let recovery_tail_repair = writer.tail_repair();
        #[cfg(test)]
        run_before_shared_construction_hook()?;
        let mut shared = Self::from_graph_with_core_and_durables(
            graph,
            providers,
            Vec::new(),
            Some(writer),
            audit_log,
            // Recovery uses the BRIEF-1-equivalent policy: one fsync per commit.
            crate::committer_batch::CommitBatching::Off,
        )?;
        shared.recovery_tail_repair = recovery_tail_repair;
        let final_snapshot = shared.read();
        attachments.prepare_all(&final_snapshot)?;
        attachments.commit_all()?;
        attachments.finalize_all();
        attachments.disarm();
        Ok(shared)
    }
}

struct RecoveryAttachmentGuard {
    reserved: Vec<Arc<dyn IndexProvider>>,
    armed: bool,
}

impl RecoveryAttachmentGuard {
    fn reserve(providers: &[Arc<dyn IndexProvider>]) -> GraphResult<Self> {
        let mut guard = Self {
            reserved: Vec::with_capacity(providers.len()),
            armed: true,
        };
        for provider in providers {
            provider.reserve_recovery_attachment()?;
            guard.reserved.push(Arc::clone(provider));
        }
        Ok(guard)
    }

    fn prepare_all(&self, graph: &crate::SeleneGraph) -> GraphResult<()> {
        for provider in &self.reserved {
            provider.prepare_recovery_attachment(graph)?;
        }
        Ok(())
    }

    fn commit_all(&self) -> GraphResult<()> {
        for (ordinal, provider) in self.reserved.iter().enumerate() {
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                provider.commit_recovery_attachment()
            }));
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(GraphError::Provider(error)),
                Err(payload) => {
                    return Err(GraphError::Provider(ProviderError::Inconsistent {
                        reason: format!(
                            "recovery attachment commit panicked at provider ordinal {}: {}",
                            ordinal + 1,
                            bounded_provider_panic(&payload)
                        ),
                    }));
                }
            }
        }
        Ok(())
    }

    fn finalize_all(&self) {
        for (ordinal, provider) in self.reserved.iter().enumerate() {
            if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                provider.finalize_recovery_attachment();
            })) {
                tracing::error!(
                    provider_ordinal = ordinal + 1,
                    panic = %bounded_provider_panic(&payload),
                    "recovery attachment finalize callback panicked"
                );
            }
        }
    }

    fn abort_all(&self) {
        for (ordinal, provider) in self.reserved.iter().enumerate().rev() {
            if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                provider.abort_recovery_attachment();
            })) {
                tracing::error!(
                    provider_ordinal = ordinal + 1,
                    panic = %bounded_provider_panic(&payload),
                    "recovery attachment abort callback panicked"
                );
            }
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RecoveryAttachmentGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.abort_all();
    }
}

const MAX_PROVIDER_PANIC_BYTES: usize = 256;

fn bounded_provider_panic(payload: &Box<dyn std::any::Any + Send>) -> String {
    let mut detail = crate::panic_payload::describe(payload);
    if detail.len() <= MAX_PROVIDER_PANIC_BYTES {
        return detail;
    }
    let mut end = MAX_PROVIDER_PANIC_BYTES - 3;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail.truncate(end);
    detail.push_str("...");
    detail
}

fn open_recovery_writer(
    dir: &selene_persist::StoreDirectory,
    snapshot_seq: u64,
) -> GraphResult<WalWriter> {
    Ok(WalWriter::open_in(
        dir,
        Path::new(DEFAULT_WAL_FILE_NAME),
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq,
        },
    )?)
}

#[cfg(test)]
type SharedConstructionHook = Box<dyn FnOnce() -> GraphResult<()>>;

#[cfg(test)]
thread_local! {
    static AFTER_PERSIST_RECOVERY_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static BEFORE_MISSING_WAL_OPEN_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static BEFORE_SHARED_CONSTRUCTION_HOOK: std::cell::RefCell<Option<SharedConstructionHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_after_persist_recovery_hook(hook: impl FnOnce() + 'static) {
    AFTER_PERSIST_RECOVERY_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn set_before_missing_wal_open_hook(hook: impl FnOnce() + 'static) {
    BEFORE_MISSING_WAL_OPEN_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn set_before_shared_construction_hook(hook: impl FnOnce() -> GraphResult<()> + 'static) {
    BEFORE_SHARED_CONSTRUCTION_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn run_after_persist_recovery_hook() {
    AFTER_PERSIST_RECOVERY_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn run_before_missing_wal_open_hook() {
    BEFORE_MISSING_WAL_OPEN_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn run_before_shared_construction_hook() -> GraphResult<()> {
    BEFORE_SHARED_CONSTRUCTION_HOOK.with(|slot| match slot.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

/// Recovery wrapper that drives a runtime [`IndexProvider`] through
/// [`selene-persist`]'s `RecoveryProvider` interface during snapshot-section
/// reads and WAL replay.
struct IndexRecoveryProvider {
    provider: Arc<dyn IndexProvider>,
}

impl RecoveryProvider for IndexRecoveryProvider {
    fn provider_tag(&self) -> [u8; 4] {
        self.provider.provider_tag().0
    }

    fn read_section(&self, sub: [u8; 4], bytes: &[u8]) -> RecoveryResult<()> {
        self.provider
            .read_section(SubTag(sub), bytes)
            .map_err(recovery_error)
    }

    fn on_change(&self, change: &Change) -> RecoveryResult<()> {
        self.provider.on_change(change).map_err(recovery_error)
    }

    fn on_changes(&self, changes: &[Change]) -> RecoveryResult<()> {
        self.provider.on_changes(changes).map_err(recovery_error)
    }
}

fn validate_recovery_provider_tags(
    core: &Arc<CoreProvider>,
    providers: &[Arc<dyn IndexProvider>],
) -> GraphResult<()> {
    let mut all_providers = Vec::with_capacity(providers.len() + 1);
    let core_provider: Arc<dyn IndexProvider> = core.clone();
    all_providers.push(core_provider);
    all_providers.extend(providers.iter().cloned());
    validate_unique_provider_tags(&all_providers)
}

fn register_index_recovery_providers(
    registry: &mut ProviderRegistry,
    providers: &[Arc<dyn IndexProvider>],
) -> GraphResult<()> {
    for provider in providers {
        let recovery_provider: Arc<dyn RecoveryProvider> = Arc::new(IndexRecoveryProvider {
            provider: Arc::clone(provider),
        });
        registry.register(recovery_provider)?;
    }
    Ok(())
}

fn mark_recovered_provider_generation(
    providers: &[Arc<dyn IndexProvider>],
    graph: &crate::SeleneGraph,
    outcome: &selene_persist::RecoveryOutcome,
) -> GraphResult<()> {
    for provider in providers {
        let provider_tag = provider.provider_tag().0;
        let missing_snapshot_state = outcome.applied_snapshot_seq > 0
            && !provider.declared_sub_tags().is_empty()
            && outcome
                .snapshot_providers_invoked
                .binary_search(&provider_tag)
                .is_err();
        if missing_snapshot_state {
            provider.rebuild_from_graph(graph)?;
        }
        provider.on_commit_applied(graph.meta.generation)?;
    }
    Ok(())
}

fn recovery_error(error: ProviderError) -> RecoveryError {
    Box::new(error)
}

#[cfg(test)]
#[path = "recover_tests.rs"]
mod tests;
