//! Facade transaction demarcation shared by Rust and GQL controls.

use selene_catalog::GraphId as LowerGraphId;

use crate::{
    Error, Result, Transaction, TransactionAccessMode, TransactionState,
    session_context::TransactionCheckout,
    transaction::{
        AuthorityOutcome, DatabaseDraft, DetachedTransaction, MutationMode, TransitionEvent,
        transition,
    },
};

use super::Session;

impl Session {
    /// Start an explicit transaction with the requested access mode.
    ///
    /// The returned descriptor contains only facade-owned identities and pinned
    /// generation summaries. No writer reservation survives this call.
    pub fn start_transaction(&self, access_mode: TransactionAccessMode) -> Result<Transaction> {
        self.ensure_no_active_request()?;
        let mut slot = self.context.checkout_transaction();
        self.start_transaction_checked(&mut slot, access_mode, true)
    }

    /// Commit the active explicit transaction.
    ///
    /// Read-only transactions require no publication. Read-write transactions
    /// validate their exact pinned base under the Part 1 reservation and cross
    /// the sole outer publication barrier once.
    pub fn commit_transaction(&self) -> Result<Transaction> {
        self.ensure_no_active_request()?;
        let mut slot = self.context.checkout_transaction();
        self.commit_transaction_checked(&mut slot)
    }

    /// Roll back the active explicit transaction by discarding detached work.
    pub fn rollback_transaction(&self) -> Result<Transaction> {
        self.ensure_no_active_request()?;
        let mut slot = self.context.checkout_transaction();
        self.rollback_transaction_checked(&mut slot)
    }

    pub(super) fn start_transaction_checked(
        &self,
        slot: &mut TransactionCheckout<'_>,
        access_mode: TransactionAccessMode,
        explicit: bool,
    ) -> Result<Transaction> {
        let current = slot
            .as_ref()
            .map(|transaction| transaction.descriptor().state());
        transition(current, TransitionEvent::Start)?;
        self.validate_context_references()?;
        let graph = self.context.current_graph();
        let graph_id = LowerGraphId::new(graph.id.get()).map_err(|source| {
            Error::invalid_session_reference(Error::from_catalog_invariant(source))
        })?;
        let id = self.inner.allocate_transaction_id()?;
        let detached = self.inner.with_mutation_reservation(|reservation| {
            let base = self.inner.state.load_full();
            let mut draft = DatabaseDraft::new(&base, &reservation);
            let instance = draft.pin_graph(&base, graph_id)?;
            drop(instance);
            let descriptor = Transaction::new(
                id,
                access_mode,
                draft.base_publication(),
                crate::CatalogGeneration::from_lower(draft.base_catalog_generation()),
                graph.id,
                draft.pinned_graph_generation()?,
            );
            Ok(DetachedTransaction::new(descriptor, draft, explicit))
        })?;
        let descriptor = detached.descriptor().clone();
        slot.replace(detached);
        Ok(descriptor)
    }

    pub(super) fn commit_transaction_checked(
        &self,
        slot: &mut TransactionCheckout<'_>,
    ) -> Result<Transaction> {
        let Some(transaction) = slot.as_mut() else {
            return Err(Error::no_active_transaction());
        };
        match transaction.descriptor().state() {
            TransactionState::Failed => {
                transaction.transition(TransitionEvent::BeginCommit)?;
                return Err(Error::in_failed_transaction());
            }
            TransactionState::Active => transaction.transition(TransitionEvent::BeginCommit)?,
            TransactionState::RolledBack
            | TransactionState::Committed
            | TransactionState::Indeterminate => return Err(Error::no_active_transaction()),
            TransactionState::Committing => return Err(Error::invalid_transaction_transition()),
        }

        if transaction.descriptor().access_mode() == TransactionAccessMode::ReadOnly
            || !transaction.draft()?.is_modified()
        {
            drop(transaction.take_draft()?);
            transaction.transition(TransitionEvent::CommitSucceeded)?;
            return Ok(transaction.descriptor().clone());
        }

        let draft = transaction.take_draft()?;
        let publication = self.inner.with_mutation_reservation(|reservation| {
            let current = self.inner.state.load_full();
            if !draft.matches_base(&current) {
                return Ok(None);
            }
            self.inner
                .publish_database_draft(reservation, draft)
                .map(Some)
        });
        match publication {
            Ok(None) => {
                transaction.transition(TransitionEvent::CommitCanceled)?;
                Err(Error::transaction_rollback())
            }
            Ok(Some(AuthorityOutcome::Committed)) => {
                transaction.transition(TransitionEvent::CommitSucceeded)?;
                Ok(transaction.descriptor().clone())
            }
            Ok(Some(AuthorityOutcome::Canceled)) => {
                transaction.transition(TransitionEvent::CommitCanceled)?;
                Err(Error::mutation_canceled())
            }
            Ok(Some(AuthorityOutcome::Indeterminate)) => {
                transaction.transition(TransitionEvent::CommitIndeterminate)?;
                Err(Error::mutation_indeterminate())
            }
            Err(error) => {
                let uncertain = error
                    .durable_commit_outcome()
                    .is_some_and(|outcome| outcome.state != crate::DurableCommitState::Canceled);
                transaction.transition(if uncertain {
                    TransitionEvent::CommitIndeterminate
                } else {
                    TransitionEvent::CommitCanceled
                })?;
                if error.kind() == crate::ErrorKind::StaleSessionReference {
                    Err(Error::transaction_rollback())
                } else if error.durable_commit_outcome().is_none()
                    && error.gqlstatus() == Some(crate::GqlStatus::GRAPH_TYPE_VIOLATION)
                {
                    Err(Error::local_commit_rollback(error))
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(super) fn rollback_transaction_checked(
        &self,
        slot: &mut TransactionCheckout<'_>,
    ) -> Result<Transaction> {
        let Some(transaction) = slot.as_mut() else {
            return Err(Error::no_active_transaction());
        };
        transaction.transition(TransitionEvent::Rollback)?;
        Ok(transaction.descriptor().clone())
    }

    pub(super) fn authorize_mutation(
        transaction: &mut DetachedTransaction,
        requested: MutationMode,
    ) -> Result<()> {
        if transaction.descriptor().access_mode() == TransactionAccessMode::ReadOnly {
            transaction.transition(TransitionEvent::StatementFailed)?;
            return Err(Error::read_only_transaction());
        }
        if transaction
            .mutation_mode()
            .is_some_and(|established| established != requested)
        {
            transaction.transition(TransitionEvent::StatementFailed)?;
            return Err(Error::transaction_mixing());
        }
        if transaction.mutation_mode().is_none() {
            transaction.set_mutation_mode(requested);
        }
        Ok(())
    }

    pub(super) fn fail_statement(transaction: &mut DetachedTransaction, error: Error) -> Error {
        if transaction.descriptor().state() == TransactionState::Active {
            let _ = transaction.transition(TransitionEvent::StatementFailed);
        }
        if !transaction.is_explicit()
            && transaction.descriptor().state() == TransactionState::Failed
        {
            let _ = transaction.transition(TransitionEvent::Rollback);
        }
        error
    }

    fn ensure_no_active_request(&self) -> Result<()> {
        if self.context.termination() == crate::SessionTerminationState::Closed {
            Err(Error::session_closed())
        } else if self.context.current_request().is_some() {
            Err(Error::request_already_active())
        } else {
            Ok(())
        }
    }
}
