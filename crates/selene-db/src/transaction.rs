//! Facade-owned in-memory mutation reservation and publication authority.
//!
//! The writer capability is universally quantified so its lifetime cannot be
//! selected by the caller or appear in the closure's result:
//!
//! ```compile_fail,E0308
//! use std::marker::PhantomData;
//!
//! struct MutationReservation<'writer>(PhantomData<&'writer mut ()>);
//!
//! fn reserve<T>(
//!     execute: impl for<'writer> FnOnce(MutationReservation<'writer>) -> T,
//! ) -> T {
//!     execute(MutationReservation(PhantomData))
//! }
//!
//! let _escaped = reserve(|reservation| reservation);
//! ```

use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
    rc::Rc,
    sync::Arc,
};

use parking_lot::{Mutex, MutexGuard};
use selene_catalog::{CatalogGeneration, CatalogObjectId, CatalogSnapshot, GraphId, GraphTypeId};
use selene_core::GraphId as CoreGraphId;
use selene_graph::{
    GraphAllocationAuthority, GraphTypeDef, SeleneGraph, SharedGraph,
    write_txn::PreparedGraphCommit,
};

use crate::{
    Error, Result,
    catalog_snapshot::graph_summary,
    database::{DatabaseInner, DatabaseState, GraphInstance, HighWaterMarks},
};

mod codec;
mod durable;
mod named;
mod state;
#[cfg(test)]
mod test_schema;

pub(crate) use state::{DetachedTransaction, MutationMode, TransitionEvent, transition};
pub use state::{Transaction, TransactionAccessMode, TransactionId, TransactionState};

/// Closure-local proof that the facade's writer mutex remains held.
///
/// The invariant lifetime comes from a mutable borrow of the stack-local mutex
/// guard. `Rc` also keeps the capability on the reserving thread. Database
/// drafts remain lifetime-free; only this publication authority is borrowed.
pub(crate) struct MutationReservation<'writer> {
    _writer: PhantomData<&'writer mut ()>,
    _not_send: PhantomData<Rc<()>>,
}

impl<'writer> MutationReservation<'writer> {
    fn new(_writer: &'writer mut MutexGuard<'_, ()>) -> Self {
        Self {
            _writer: PhantomData,
            _not_send: PhantomData,
        }
    }
}

/// Durability-independent result of the in-memory authority cut-line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "non-committed authority outcomes are injected only by test failpoints until M09"
)]
pub(crate) enum AuthorityOutcome {
    /// Publication was canceled before the outer state store.
    Canceled,
    /// The complete state was stored and acknowledged.
    Committed,
    /// The complete state was stored, but acknowledgement was uncertain.
    Indeterminate,
}

#[derive(Clone, Copy)]
struct PinnedGraph {
    id: GraphId,
    instance_identity: usize,
    generation: u64,
}

enum DetachedGraphReplacement {
    Snapshot(Box<SeleneGraph>),
    Prepared(PreparedGraphCommit),
}

impl DetachedGraphReplacement {
    fn bind_catalog(&mut self, catalog: &CatalogSnapshot) -> Result<()> {
        match self {
            Self::Snapshot(snapshot) => snapshot.bind_catalog(catalog),
            Self::Prepared(prepared) => prepared.bind_catalog(catalog),
        }
        .map_err(Error::from_catalog_invariant)
    }
    fn snapshot(&self) -> &SeleneGraph {
        match self {
            Self::Snapshot(snapshot) => snapshot,
            Self::Prepared(prepared) => prepared.snapshot(),
        }
    }

    fn into_snapshot(self) -> SeleneGraph {
        match self {
            Self::Snapshot(snapshot) => *snapshot,
            Self::Prepared(prepared) => prepared.into_snapshot(),
        }
    }
}

/// Lifetime-free detached catalog/graph draft pinned to outer-state metadata.
///
/// This type deliberately contains no outer state allocation, graph instance,
/// shared graph, transaction, lock guard, committer, or provider state. Its
/// allocation-only capability burns identities independently of draft lifetime.
pub(crate) struct DatabaseDraft {
    base_state_identity: usize,
    base_publication: u64,
    base_catalog_generation: CatalogGeneration,
    pub(crate) catalog: CatalogSnapshot,
    pub(crate) graph_types: BTreeMap<GraphTypeId, Arc<GraphTypeDef>>,
    pub(crate) high_water: HighWaterMarks,
    pinned_graph: Option<PinnedGraph>,
    graph_removals: BTreeSet<GraphId>,
    graph_replacements: BTreeMap<GraphId, DetachedGraphReplacement>,
    logical_changes: BTreeMap<GraphId, Vec<selene_core::Change>>,
    selected_graph: Option<Box<SeleneGraph>>,
    allocation: Option<GraphAllocationAuthority>,
    forget_graphs: BTreeSet<CoreGraphId>,
    modified: bool,
}

impl DatabaseDraft {
    pub(crate) fn new(base: &Arc<DatabaseState>, _reservation: &MutationReservation<'_>) -> Self {
        Self {
            base_state_identity: Arc::as_ptr(base) as usize,
            base_publication: base.publication,
            base_catalog_generation: base.catalog.generation(),
            catalog: base.catalog.clone(),
            graph_types: base.graph_types.clone(),
            high_water: base.high_water,
            pinned_graph: None,
            graph_removals: BTreeSet::new(),
            graph_replacements: BTreeMap::new(),
            logical_changes: BTreeMap::new(),
            selected_graph: None,
            allocation: None,
            forget_graphs: BTreeSet::new(),
            modified: false,
        }
    }

    pub(crate) fn forget_graph(&mut self, id: CoreGraphId) {
        self.forget_graphs.insert(id);
    }

    pub(crate) fn pin_graph(
        &mut self,
        base: &Arc<DatabaseState>,
        id: GraphId,
    ) -> Result<Arc<GraphInstance>> {
        if Arc::as_ptr(base) as usize != self.base_state_identity
            || base.publication != self.base_publication
        {
            return Err(Error::catalog_invariant(
                "graph pin does not belong to the database draft base",
            ));
        }
        let instance = base
            .graphs
            .get(&id)
            .cloned()
            .ok_or_else(Error::stale_session_reference)?;
        let snapshot = instance.graph.read();
        if snapshot.graph_id().get() != id.get() {
            return Err(Error::catalog_invariant(
                "registered graph identity disagrees with its catalog identity",
            ));
        }
        self.pinned_graph = Some(PinnedGraph {
            id,
            instance_identity: Arc::as_ptr(&instance) as usize,
            generation: snapshot.meta.generation,
        });
        self.selected_graph = Some(Box::new(snapshot.as_ref().clone()));
        self.allocation = Some(instance.graph.allocation_authority());
        drop(snapshot);
        Ok(instance)
    }

    pub(crate) const fn base_publication(&self) -> u64 {
        self.base_publication
    }

    pub(crate) const fn base_catalog_generation(&self) -> CatalogGeneration {
        self.base_catalog_generation
    }

    pub(crate) fn matches_base(&self, base: &Arc<DatabaseState>) -> bool {
        let outer_matches = Arc::as_ptr(base) as usize == self.base_state_identity
            && base.publication == self.base_publication
            && base.catalog.generation() == self.base_catalog_generation;
        outer_matches
            && self.pinned_graph.is_none_or(|pinned| {
                base.graphs.get(&pinned.id).is_some_and(|instance| {
                    let snapshot = instance.graph.read();
                    Arc::as_ptr(instance) as usize == pinned.instance_identity
                        && snapshot.graph_id().get() == pinned.id.get()
                        && snapshot.meta.generation == pinned.generation
                })
            })
    }

    pub(crate) fn selected_graph(&self) -> Result<&SeleneGraph> {
        let pinned = self
            .pinned_graph
            .ok_or_else(|| Error::catalog_invariant("database draft has no selected graph"))?;
        self.graph_replacements
            .get(&pinned.id)
            .map(DetachedGraphReplacement::snapshot)
            .or(self.selected_graph.as_deref())
            .ok_or_else(|| Error::catalog_invariant("database draft lost its selected graph"))
    }

    pub(crate) fn mutation_scratch(&self) -> Result<SharedGraph> {
        let authority = self.allocation.as_ref().ok_or_else(|| {
            Error::catalog_invariant("database draft has no allocation authority")
        })?;
        SharedGraph::try_from_graph_with_allocation(self.selected_graph()?.clone(), authority)
            .map_err(Error::invalid_graph_type_source)
    }

    pub(crate) fn selected_graph_id(&self) -> Result<GraphId> {
        self.pinned_graph
            .map(|pinned| pinned.id)
            .ok_or_else(|| Error::catalog_invariant("database draft has no selected graph"))
    }

    pub(crate) fn pinned_graph_generation(&self) -> Result<u64> {
        self.pinned_graph
            .map(|pinned| pinned.generation)
            .ok_or_else(|| Error::catalog_invariant("database draft has no selected graph"))
    }

    pub(crate) fn state_view(&self) -> DatabaseState {
        DatabaseState {
            publication: self.base_publication,
            catalog: self.catalog.clone(),
            graphs: BTreeMap::new(),
            graph_types: self.graph_types.clone(),
            high_water: self.high_water,
        }
    }

    pub(crate) const fn is_modified(&self) -> bool {
        self.modified
    }

    pub(crate) fn mark_modified(&mut self) {
        self.modified = true;
    }

    pub(crate) fn remove_graph(&mut self, id: GraphId) {
        self.modified = true;
        self.graph_removals.insert(id);
        self.graph_replacements.remove(&id);
        self.logical_changes.remove(&id);
    }

    pub(crate) fn replace_graph(&mut self, id: GraphId, snapshot: SeleneGraph) -> Result<()> {
        if snapshot.graph_id().get() != id.get() || self.graph_replacements.contains_key(&id) {
            return Err(Error::catalog_invariant(
                "database draft has an invalid or duplicate graph replacement",
            ));
        }
        self.graph_removals.remove(&id);
        self.modified = true;
        self.graph_replacements
            .insert(id, DetachedGraphReplacement::Snapshot(Box::new(snapshot)));
        Ok(())
    }

    pub(crate) fn replacement_snapshot(&self, id: GraphId) -> Option<&SeleneGraph> {
        self.graph_replacements
            .get(&id)
            .map(DetachedGraphReplacement::snapshot)
    }

    pub(crate) fn attach_prepared_graph(
        &mut self,
        id: GraphId,
        mut prepared: PreparedGraphCommit,
    ) -> Result<()> {
        let Some(pinned) = self.pinned_graph else {
            return Err(Error::catalog_invariant(
                "prepared graph commit has no pinned graph base",
            ));
        };
        if pinned.id != id || prepared.snapshot().graph_id().get() != id.get() {
            return Err(Error::catalog_invariant(
                "prepared graph commit changed the stable graph identity",
            ));
        }
        let current_generation = self.selected_graph()?.meta.generation;
        if prepared.snapshot().meta.generation != current_generation.saturating_add(1) {
            return Err(Error::catalog_invariant(
                "prepared graph generation is not the detached generation successor",
            ));
        }
        if prepared.schema_changed() {
            self.stage_registrations(id, &prepared)?;
            prepared
                .bind_catalog(&self.catalog)
                .map_err(Error::from_catalog_invariant)?;
        }
        self.validate_named_graph(prepared.snapshot(), prepared.changes())?;
        self.graph_removals.remove(&id);
        self.modified = true;
        self.logical_changes
            .entry(id)
            .or_default()
            .extend_from_slice(prepared.changes());
        self.graph_replacements
            .insert(id, DetachedGraphReplacement::Prepared(prepared));
        Ok(())
    }
}

/// One facade-owned serial mutation coordinator.
pub(crate) struct MutationCoordinator {
    writer: Mutex<()>,
    durable: Mutex<Option<durable::DurableAuthority>>,
}

impl MutationCoordinator {
    pub(crate) const fn new() -> Self {
        Self {
            writer: Mutex::new(()),
            durable: Mutex::new(None),
        }
    }
}

impl DatabaseInner {
    /// Run one mutation while holding the facade's only writer reservation.
    pub(crate) fn with_mutation_reservation<T>(
        &self,
        execute: impl for<'writer> FnOnce(MutationReservation<'writer>) -> T,
    ) -> T {
        #[cfg(test)]
        assert_eq!(
            crate::database::GraphRequestDepth::current(),
            0,
            "catalog lifecycle entered under a same-thread graph request lease"
        );
        let mut writer = self.transactions.writer.lock();
        execute(MutationReservation::new(&mut writer))
    }

    /// Validate, publish once, clean graph-scoped runtime state, and acknowledge.
    pub(crate) fn publish_database_draft(
        &self,
        reservation: MutationReservation<'_>,
        draft: DatabaseDraft,
    ) -> Result<AuthorityOutcome> {
        let mut durable = self.transactions.durable.lock();
        let result = self.publish_draft_checked(reservation, draft, durable.as_mut());
        result.map_err(|error| match durable.as_ref() {
            Some(wal) if error.durable_commit_outcome().is_none() => durable::canceled(error, wal),
            _ => error,
        })
    }

    fn publish_draft_checked(
        &self,
        _reservation: MutationReservation<'_>,
        mut draft: DatabaseDraft,
        mut wal: Option<&mut durable::DurableAuthority>,
    ) -> Result<AuthorityOutcome> {
        if wal.as_ref().is_some_and(|wal| wal.is_fenced()) {
            return Err(Error::catalog_invariant(
                "durable writer fenced; reconciliation required",
            ));
        }
        if !draft.is_modified() && draft.pinned_graph.is_none() {
            return Ok(AuthorityOutcome::Committed);
        }
        #[cfg(test)]
        if self.take_failure(crate::catalog::FailurePoint::BeforeAuthorityPrepare) {
            if wal.is_some() {
                return Err(Error::injected_failure("before durable preparation"));
            }
            return Ok(AuthorityOutcome::Canceled);
        }
        #[cfg(test)]
        if self.take_failure(crate::catalog::FailurePoint::BeforeAuthorityFlush) {
            if wal.is_some() {
                return Err(Error::injected_failure("before authoritative append"));
            }
            return Ok(AuthorityOutcome::Canceled);
        }

        let current = self.state.load_full();
        if draft.catalog.generation() != draft.base_catalog_generation {
            self.procedures
                .validate_catalog(&draft.catalog)
                .map_err(Error::from_catalog_invariant)?;
            for descriptor in draft.catalog.descriptors() {
                if let Some(previous) = current.catalog.descriptor(descriptor.id())
                    && descriptor != previous
                    && (descriptor.generation() <= previous.generation()
                        || descriptor.creation() != previous.creation()
                        || descriptor.parent() != previous.parent())
                {
                    return Err(Error::from_catalog_invariant(
                        selene_catalog::CatalogError::InvalidDeclaration {
                            reason: "invalid_descriptor_revision",
                        },
                    ));
                }
            }
            for descriptor in current
                .catalog
                .descriptors()
                .chain(draft.catalog.descriptors())
            {
                if descriptor.payload().declaration_metadata().is_none()
                    || current.catalog.descriptor(descriptor.id())
                        == draft.catalog.descriptor(descriptor.id())
                {
                    continue;
                }
                if let selene_catalog::CatalogParent::Graph(id) = descriptor.parent()
                    && !draft.graph_removals.contains(&id)
                    && !draft.graph_replacements.contains_key(&id)
                {
                    return Err(Error::catalog_invariant(
                        "declaration change lacks an atomic runtime replacement",
                    ));
                }
                if matches!(
                    descriptor.parent(),
                    selene_catalog::CatalogParent::GraphType(_)
                ) && descriptor
                    .payload()
                    .declaration_metadata()
                    .is_some_and(|metadata| {
                        metadata.state == selene_catalog::DeclarationState::Ready
                    })
                {
                    return Err(Error::from_catalog_invariant(
                        selene_catalog::CatalogError::InvalidDeclaration {
                            reason: "unsupported_type_declaration_activation",
                        },
                    ));
                }
            }
        }
        for replacement in draft.graph_replacements.values_mut() {
            if draft.catalog.generation() != draft.base_catalog_generation {
                replacement.bind_catalog(&draft.catalog)?;
            }
        }
        if Arc::as_ptr(&current) as usize != draft.base_state_identity
            || current.publication != draft.base_publication
            || current.catalog.generation() != draft.base_catalog_generation
        {
            return Err(Error::catalog_invariant(
                "database mutation base changed while its reservation was active",
            ));
        }
        let lifecycle_ids = draft
            .graph_removals
            .iter()
            .chain(draft.graph_replacements.keys())
            .filter(|id| current.graphs.contains_key(id))
            .copied()
            .collect::<BTreeSet<_>>();
        let lifecycle_instances = lifecycle_ids
            .iter()
            .map(|id| {
                current
                    .graphs
                    .get(id)
                    .cloned()
                    .map(|instance| (*id, instance))
                    .ok_or_else(|| {
                        Error::catalog_invariant(
                            "published graph descriptor has no runtime instance",
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let _lifecycle_guards = lifecycle_instances
            .iter()
            .map(|(_, instance)| instance.lifecycle.write())
            .collect::<Vec<_>>();

        let locked_current = self.state.load_full();
        if !draft.matches_base(&locked_current)
            || lifecycle_instances.iter().any(|(id, instance)| {
                locked_current
                    .graphs
                    .get(id)
                    .is_none_or(|registered| !Arc::ptr_eq(registered, instance))
            })
        {
            return Err(Error::transaction_rollback());
        }
        if let Some(pinned) = draft.pinned_graph {
            let instance = locked_current
                .graphs
                .get(&pinned.id)
                .ok_or_else(Error::stale_session_reference)?;
            let snapshot = instance.graph.read();
            if Arc::as_ptr(instance) as usize != pinned.instance_identity
                || snapshot.graph_id().get() != pinned.id.get()
                || snapshot.meta.generation != pinned.generation
            {
                return Err(Error::stale_session_reference());
            }
            if let Some(replacement) = draft.graph_replacements.get(&pinned.id)
                && (replacement.snapshot().graph_id().get() != pinned.id.get()
                    || replacement.snapshot().meta.generation <= pinned.generation)
            {
                return Err(Error::catalog_invariant(
                    "prepared database state has a stale graph identity or generation",
                ));
            }
        }
        for id in &draft.graph_removals {
            let Some(instance) = locked_current.graphs.get(id) else {
                continue;
            };
            let snapshot = instance.graph.read();
            if snapshot.node_count() != 0 || snapshot.edge_count() != 0 {
                let descriptor = locked_current
                    .catalog
                    .descriptor(CatalogObjectId::Graph(*id))
                    .ok_or_else(|| {
                        Error::catalog_invariant("removed graph descriptor is missing")
                    })?;
                let path = graph_summary(&locked_current, descriptor)?.path;
                return Err(Error::nonempty_graph(
                    &path,
                    snapshot.node_count(),
                    snapshot.edge_count(),
                ));
            }
        }

        #[cfg(test)]
        if wal.is_none() && self.take_failure(crate::catalog::FailurePoint::BeforePublication) {
            return Ok(AuthorityOutcome::Canceled);
        }

        draft.validate_named_replacements(&current)?;
        let encoded = wal
            .as_mut()
            .map(|wal| durable::prepare(wal, &draft, &current))
            .transpose()?;
        let DatabaseDraft {
            catalog,
            graph_types,
            high_water,
            graph_removals,
            graph_replacements,
            forget_graphs,
            ..
        } = draft;
        let mut graphs = current.graphs.clone();
        for id in graph_removals {
            graphs.remove(&id);
        }
        for (id, replacement) in graph_replacements {
            let snapshot = replacement.into_snapshot();
            let graph = match current.graphs.get(&id) {
                Some(instance) => SharedGraph::try_from_graph_with_allocation(
                    snapshot,
                    &instance.graph.allocation_authority(),
                ),
                None => SharedGraph::try_from_graph(snapshot),
            }
            .map_err(Error::invalid_graph_type_source)?;
            #[cfg(test)]
            self.replacement_graph_constructions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            graphs.insert(id, Arc::new(GraphInstance::new(graph)));
        }
        let next = Arc::new(DatabaseState {
            publication: current.publication.saturating_add(1),
            catalog,
            graphs,
            graph_types,
            high_water,
        });

        // The sole facade mutation cut-line. No facade code outside this
        // authority may store the outer database state.
        let publish = |notification: Option<
            &mut selene_persist::logical_stream::Publication<'_>,
        >| {
            #[cfg(test)]
            if notification.is_some()
                && self.take_failure(crate::catalog::FailurePoint::BeforePublication)
            {
                return Err(selene_persist::logical_stream::StreamError::Protocol(
                    "interrupted before outer publication",
                ));
            }
            self.state.store(next);
            if let Some(notification) = notification {
                notification.mark_published();
            }
            #[cfg(test)]
            if self.take_failure(crate::catalog::FailurePoint::AfterPublicationObserverPanic) {
                panic!("injected post-store observer unwind");
            }
            for id in forget_graphs {
                self.procedures.forget_graph(id);
            }
            #[cfg(test)]
            if self.take_failure(crate::catalog::FailurePoint::AfterPublicationAcknowledgement) {
                return Err(selene_persist::logical_stream::StreamError::Protocol(
                    "interrupted acknowledgment",
                ));
            }
            Ok(())
        };
        if let (Some(wal), Some(encoded)) = (wal, encoded) {
            wal.wal
                .commit(
                    encoded.group,
                    || false,
                    |notification| publish(Some(notification)),
                )
                .map_err(Error::durable_failure)?;
            wal.replay = encoded.replay;
            Ok(AuthorityOutcome::Committed)
        } else {
            Ok(if publish(None).is_ok() {
                AuthorityOutcome::Committed
            } else {
                AuthorityOutcome::Indeterminate
            })
        }
    }

    #[cfg(test)]
    fn take_failure(&self, point: crate::catalog::FailurePoint) -> bool {
        let mut failure = self.failure.lock();
        if *failure == Some(point) {
            failure.take();
            true
        } else {
            false
        }
    }
}

pub(crate) fn require_committed(outcome: AuthorityOutcome) -> Result<()> {
    match outcome {
        AuthorityOutcome::Committed => Ok(()),
        AuthorityOutcome::Canceled => Err(Error::mutation_canceled()),
        AuthorityOutcome::Indeterminate => Err(Error::mutation_indeterminate()),
    }
}

const _: fn() = || {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<DatabaseDraft>();
    assert_send_static::<AuthorityOutcome>();
};

#[cfg(test)]
#[path = "transaction/tests.rs"]
mod tests;
