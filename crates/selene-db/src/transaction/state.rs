//! Facade transaction descriptors and pure demarcation transitions.

use std::num::NonZeroU64;

use crate::{CatalogGeneration, Error, GraphId, Result};

use super::DatabaseDraft;

/// Monotonic database-local transaction identifier.
///
/// IDs are nonzero, never reused by a database instance, and remain useful for
/// diagnostics after a transaction reaches a terminal state.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TransactionId(NonZeroU64);

impl TransactionId {
    pub(crate) const fn from_nonzero(value: NonZeroU64) -> Self {
        Self(value)
    }

    /// Return the raw nonzero identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Access characteristic fixed when a transaction starts.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransactionAccessMode {
    /// The transaction may execute reads but cannot stage writes.
    ReadOnly,
    /// The transaction may execute reads and one supported modification class.
    ReadWrite,
}

/// Public transaction lifecycle state.
///
/// ```text
/// Active --statement failure--> Failed --ROLLBACK/COMMIT--> RolledBack
/// Active --ROLLBACK-----------> RolledBack
/// Active --COMMIT-------------> Committing
/// Committing --ack------------> Committed
/// Committing --pre-store stop-> RolledBack
/// Committing --post-store ?---> Indeterminate
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransactionState {
    /// Requests may execute and stage work.
    Active,
    /// A statement failed and detached work was discarded.
    Failed,
    /// Commit validation/publication is in progress.
    Committing,
    /// Detached work was discarded without publication.
    RolledBack,
    /// The complete successor state was published and acknowledged.
    Committed,
    /// Termination was not acknowledged. In-memory mutations have published;
    /// durable failures require inspecting `Error::durable_commit_outcome` for
    /// recovery evidence and live visibility, which may precede publication.
    Indeterminate,
}

impl TransactionState {
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::RolledBack | Self::Committed | Self::Indeterminate
        )
    }
}

/// Immutable, lower-type-free inspection of one facade transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transaction {
    id: TransactionId,
    access_mode: TransactionAccessMode,
    state: TransactionState,
    pinned_publication: u64,
    pinned_catalog_generation: CatalogGeneration,
    selected_graph: GraphId,
    pinned_graph_generation: u64,
    statement_count: u32,
    staged_change_count: usize,
}

impl Transaction {
    pub(crate) const fn new(
        id: TransactionId,
        access_mode: TransactionAccessMode,
        pinned_publication: u64,
        pinned_catalog_generation: CatalogGeneration,
        selected_graph: GraphId,
        pinned_graph_generation: u64,
    ) -> Self {
        Self {
            id,
            access_mode,
            state: TransactionState::Active,
            pinned_publication,
            pinned_catalog_generation,
            selected_graph,
            pinned_graph_generation,
            statement_count: 0,
            staged_change_count: 0,
        }
    }

    /// Return the database-local transaction ID.
    #[must_use]
    pub const fn id(&self) -> TransactionId {
        self.id
    }

    /// Return the fixed access mode.
    #[must_use]
    pub const fn access_mode(&self) -> TransactionAccessMode {
        self.access_mode
    }

    /// Return the current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> TransactionState {
        self.state
    }

    /// Return the outer publication pinned at start.
    #[must_use]
    pub const fn pinned_publication(&self) -> u64 {
        self.pinned_publication
    }

    /// Return the catalog generation pinned at start.
    #[must_use]
    pub const fn pinned_catalog_generation(&self) -> CatalogGeneration {
        self.pinned_catalog_generation
    }

    /// Return the stable selected graph identity.
    #[must_use]
    pub const fn selected_graph(&self) -> GraphId {
        self.selected_graph
    }

    /// Return the selected graph generation pinned at start.
    #[must_use]
    pub const fn pinned_graph_generation(&self) -> u64 {
        self.pinned_graph_generation
    }

    /// Return the number of successful non-control statements.
    #[must_use]
    pub const fn statement_count(&self) -> u32 {
        self.statement_count
    }

    /// Return the accumulated graph-change count of staged writes.
    #[must_use]
    pub const fn staged_change_count(&self) -> usize {
        self.staged_change_count
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MutationMode {
    Data,
    Catalog,
}

/// Lifetime-free state retained between facade requests.
pub(crate) struct DetachedTransaction {
    descriptor: Transaction,
    draft: Option<DatabaseDraft>,
    mutation_mode: Option<MutationMode>,
    explicit: bool,
}

impl DetachedTransaction {
    pub(crate) const fn new(descriptor: Transaction, draft: DatabaseDraft, explicit: bool) -> Self {
        Self {
            descriptor,
            draft: Some(draft),
            mutation_mode: None,
            explicit,
        }
    }

    pub(crate) const fn descriptor(&self) -> &Transaction {
        &self.descriptor
    }

    pub(crate) const fn is_explicit(&self) -> bool {
        self.explicit
    }

    pub(crate) const fn mutation_mode(&self) -> Option<MutationMode> {
        self.mutation_mode
    }

    pub(crate) fn set_mutation_mode(&mut self, mode: MutationMode) {
        self.mutation_mode = Some(mode);
    }

    pub(crate) fn draft(&self) -> Result<&DatabaseDraft> {
        self.draft.as_ref().ok_or_else(Error::in_failed_transaction)
    }

    pub(crate) fn draft_mut(&mut self) -> Result<&mut DatabaseDraft> {
        self.draft.as_mut().ok_or_else(Error::in_failed_transaction)
    }

    pub(crate) fn take_draft(&mut self) -> Result<DatabaseDraft> {
        self.draft.take().ok_or_else(Error::in_failed_transaction)
    }

    pub(crate) fn record_statement(&mut self, change_count: usize) {
        self.descriptor.statement_count = self.descriptor.statement_count.saturating_add(1);
        self.descriptor.staged_change_count = self
            .descriptor
            .staged_change_count
            .saturating_add(change_count);
    }

    pub(crate) fn transition(&mut self, event: TransitionEvent) -> Result<()> {
        self.descriptor.state = transition(Some(self.descriptor.state), event)?;
        if matches!(
            self.descriptor.state,
            TransactionState::Failed
                | TransactionState::RolledBack
                | TransactionState::Committed
                | TransactionState::Indeterminate
        ) {
            self.draft = None;
        }
        Ok(())
    }

    pub(crate) fn abandon_on_unwind(&mut self) {
        self.descriptor.state = if self.explicit {
            TransactionState::Failed
        } else {
            TransactionState::RolledBack
        };
        self.draft = None;
    }

    #[cfg(test)]
    pub(crate) const fn retains_detached_state(&self) -> bool {
        self.draft.is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransitionEvent {
    Start,
    StatementFailed,
    BeginCommit,
    Rollback,
    CommitSucceeded,
    CommitCanceled,
    CommitIndeterminate,
}

/// Pure transition authority shared by Rust and GQL demarcation paths.
pub(crate) fn transition(
    state: Option<TransactionState>,
    event: TransitionEvent,
) -> Result<TransactionState> {
    use TransactionState as State;
    use TransitionEvent as Event;
    match (state, event) {
        (None, Event::Start)
        | (Some(State::RolledBack | State::Committed | State::Indeterminate), Event::Start) => {
            Ok(State::Active)
        }
        (Some(State::Active | State::Failed | State::Committing), Event::Start) => {
            Err(Error::active_transaction())
        }
        (Some(State::Active), Event::StatementFailed) => Ok(State::Failed),
        (Some(State::Active), Event::BeginCommit) => Ok(State::Committing),
        (Some(State::Failed), Event::BeginCommit) => Ok(State::RolledBack),
        (Some(State::Active | State::Failed), Event::Rollback) => Ok(State::RolledBack),
        (Some(State::Committing), Event::CommitSucceeded) => Ok(State::Committed),
        (Some(State::Committing), Event::CommitCanceled) => Ok(State::RolledBack),
        (Some(State::Committing), Event::CommitIndeterminate) => Ok(State::Indeterminate),
        (
            None | Some(State::RolledBack | State::Committed | State::Indeterminate),
            Event::BeginCommit | Event::Rollback,
        ) => Err(Error::no_active_transaction()),
        (Some(State::Failed), Event::StatementFailed) => Err(Error::in_failed_transaction()),
        _ => Err(Error::invalid_transaction_transition()),
    }
}

const _: fn() = || {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<DetachedTransaction>();
    assert_send_static::<Transaction>();
};

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn transition_guards_and_terminal_restart_are_exact() {
        assert_eq!(
            transition(None, TransitionEvent::Start).unwrap(),
            TransactionState::Active
        );
        let error = transition(Some(TransactionState::Active), TransitionEvent::Start).unwrap_err();
        assert_eq!(error.gqlstatus().unwrap().as_str(), "25G01");
        for state in [
            TransactionState::RolledBack,
            TransactionState::Committed,
            TransactionState::Indeterminate,
        ] {
            assert_eq!(
                transition(Some(state), TransitionEvent::Start).unwrap(),
                TransactionState::Active
            );
            assert_eq!(
                transition(Some(state), TransitionEvent::Rollback)
                    .unwrap_err()
                    .gqlstatus()
                    .unwrap()
                    .as_str(),
                "2D000"
            );
        }
    }

    proptest! {
        #[test]
        fn hostile_command_sequences_never_escape_declared_states(events in prop::collection::vec(0_u8..7, 0..128)) {
            let mut state = None;
            for raw in events {
                let event = match raw {
                    0 => TransitionEvent::Start,
                    1 => TransitionEvent::StatementFailed,
                    2 => TransitionEvent::BeginCommit,
                    3 => TransitionEvent::Rollback,
                    4 => TransitionEvent::CommitSucceeded,
                    5 => TransitionEvent::CommitCanceled,
                    _ => TransitionEvent::CommitIndeterminate,
                };
                if let Ok(next) = transition(state, event) {
                    state = Some(next);
                }
                prop_assert!(state.is_none_or(|current| matches!(
                    current,
                    TransactionState::Active
                        | TransactionState::Failed
                        | TransactionState::Committing
                        | TransactionState::RolledBack
                        | TransactionState::Committed
                        | TransactionState::Indeterminate
                )));
            }
        }
    }
}
