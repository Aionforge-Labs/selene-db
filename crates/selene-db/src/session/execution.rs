//! Selected request preparation and detached transaction execution.

use std::sync::Arc;

use selene_catalog::GraphId as LowerGraphId;
use selene_gql::{
    CatalogSessionOutput, PreparedCatalogPlan, PreparedCatalogRequest, PreparedCatalogRequestKind,
    PreparedSessionControl, PreparedTransactionControl,
};
use selene_graph::SharedGraph;

use crate::{
    CatalogReadSnapshot, Error, ExecutionOutcome, ObjectPath, Request, RequestContext, Result,
    TransactionAccessMode, TransactionState, ddl,
    session_context::TransactionCheckout,
    transaction::{DetachedTransaction, MutationMode},
};

use super::Session;
use crate::database::DatabaseInner;

enum CheckedRequest {
    Prepared(PreparedCatalogRequest),
    TransactionControl(PreparedTransactionControl),
    GraphIndependentSessionControl(PreparedSessionControl),
}

impl Session {
    pub(super) fn execute_active_request(
        &self,
        request: &Request,
        context: &RequestContext,
    ) -> Result<ExecutionOutcome> {
        let mut slot = self.context.checkout_transaction();
        let prepared = self
            .prepare_checked_request(request, context, &slot)
            .map_err(|error| {
                if let Some(transaction) = slot.as_mut()
                    && transaction.descriptor().state() == TransactionState::Active
                {
                    Self::fail_statement(transaction, error)
                } else {
                    error
                }
            })?;
        match prepared {
            CheckedRequest::Prepared(prepared) => self.dispatch_prepared(prepared, &mut slot),
            CheckedRequest::TransactionControl(control) => {
                self.execute_transaction_control(control, &mut slot)
            }
            CheckedRequest::GraphIndependentSessionControl(control) => {
                self.execute_graph_independent_session_control(control, &mut slot)
            }
        }
    }

    fn prepare_checked_request(
        &self,
        request: &Request,
        context: &RequestContext,
        slot: &TransactionCheckout<'_>,
    ) -> Result<CheckedRequest> {
        let graph = self.context.current_graph();
        let graph_id = LowerGraphId::new(graph.id.get()).map_err(|source| {
            Error::invalid_session_reference(Error::from_catalog_invariant(source))
        })?;
        let input = context.lower_input(self.context.time_zone_value());
        let audit = self
            .context
            .principal()
            .and_then(crate::Principal::audit_bytes_arc);
        match slot.as_ref() {
            Some(transaction) if transaction.descriptor().state() == TransactionState::Active => {
                let snapshot = transaction.draft()?.selected_graph()?.clone();
                self.inner
                    .prepare_catalog_request_from_snapshot(snapshot, audit, request.source(), input)
                    .map(CheckedRequest::Prepared)
            }
            Some(transaction) => match self.validate_context_references() {
                Ok(()) => self
                    .inner
                    .prepare_catalog_request(graph_id, &graph.path, audit, request.source(), input)
                    .map(CheckedRequest::Prepared),
                Err(reference_error) => {
                    if let Some(control) = selene_gql::parse_transaction_control(request.source())
                        .map_err(Error::from_engine)?
                    {
                        Ok(CheckedRequest::TransactionControl(control))
                    } else if let Some(control) =
                        selene_gql::parse_graph_independent_session_control(request.source())
                            .map_err(Error::from_engine)?
                    {
                        Ok(CheckedRequest::GraphIndependentSessionControl(control))
                    } else if transaction.descriptor().state() == TransactionState::Failed {
                        Err(Error::in_failed_transaction())
                    } else {
                        Err(reference_error)
                    }
                }
            },
            None => {
                let stamp = match self.capture_dependency_stamp() {
                    Ok(stamp) => stamp,
                    Err(reference_error) => {
                        if let Some(control) =
                            selene_gql::parse_graph_independent_session_control(request.source())
                                .map_err(Error::from_engine)?
                        {
                            return Ok(CheckedRequest::GraphIndependentSessionControl(control));
                        }
                        return Err(reference_error);
                    }
                };
                let key = context.plan_key(request.source());
                if let Some(plan) = self.context.cached_plan(&key, &stamp) {
                    let prepared =
                        self.inner
                            .bind_cached_plan(graph_id, &graph.path, plan, input)?;
                    return Ok(CheckedRequest::Prepared(prepared));
                }
                let prepared = self.inner.prepare_catalog_request(
                    graph_id,
                    &graph.path,
                    audit,
                    request.source(),
                    input,
                )?;
                if matches!(
                    prepared.kind(),
                    PreparedCatalogRequestKind::ReadOnly
                        | PreparedCatalogRequestKind::DataModifying
                        | PreparedCatalogRequestKind::CatalogModifying
                ) {
                    self.context.cache_plan(key, stamp, prepared.cached_plan());
                }
                Ok(CheckedRequest::Prepared(prepared))
            }
        }
    }

    fn dispatch_prepared(
        &self,
        prepared: PreparedCatalogRequest,
        slot: &mut TransactionCheckout<'_>,
    ) -> Result<ExecutionOutcome> {
        match prepared.kind() {
            PreparedCatalogRequestKind::TransactionControl(control) => {
                self.execute_transaction_control(control, slot)
            }
            PreparedCatalogRequestKind::Maintenance => {
                let error = Error::selected_maintenance_unsupported();
                if let Some(transaction) = slot.as_mut()
                    && transaction.descriptor().state() == TransactionState::Active
                {
                    return Err(Self::fail_statement(transaction, error));
                }
                Err(error)
            }
            PreparedCatalogRequestKind::SessionControl => {
                self.execute_session_control(prepared, slot)
            }
            PreparedCatalogRequestKind::ReadOnly => self.execute_read(prepared, slot),
            PreparedCatalogRequestKind::DataModifying => {
                self.execute_modification(prepared, slot, MutationMode::Data)
            }
            PreparedCatalogRequestKind::CatalogModifying => {
                self.execute_modification(prepared, slot, MutationMode::Catalog)
            }
        }
    }

    fn execute_transaction_control(
        &self,
        control: PreparedTransactionControl,
        slot: &mut TransactionCheckout<'_>,
    ) -> Result<ExecutionOutcome> {
        match control {
            PreparedTransactionControl::Start => {
                self.start_transaction_checked(slot, TransactionAccessMode::ReadWrite, true)?;
            }
            PreparedTransactionControl::Commit => {
                self.commit_transaction_checked(slot)?;
            }
            PreparedTransactionControl::Rollback => {
                self.rollback_transaction_checked(slot)?;
            }
        }
        Ok(ExecutionOutcome::SUCCESSFUL_OMITTED)
    }

    fn execute_read(
        &self,
        prepared: PreparedCatalogRequest,
        slot: &mut TransactionCheckout<'_>,
    ) -> Result<ExecutionOutcome> {
        match slot.as_mut() {
            Some(transaction) if transaction.descriptor().state() == TransactionState::Failed => {
                Err(Error::in_failed_transaction())
            }
            Some(transaction) if transaction.descriptor().state() == TransactionState::Active => {
                let result = self.inner.execute_prepared_detached_read(
                    transaction,
                    self.audit_bytes(),
                    prepared,
                );
                match result {
                    Ok(outcome) => {
                        transaction.record_statement(0);
                        Ok(outcome)
                    }
                    Err(error) => Err(Self::fail_statement(transaction, error)),
                }
            }
            _ => {
                let graph = self.context.current_graph();
                let id = LowerGraphId::new(graph.id.get()).map_err(|source| {
                    Error::invalid_session_reference(Error::from_catalog_invariant(source))
                })?;
                self.inner
                    .execute_prepared_live_read(id, &graph.path, self.audit_bytes(), prepared)
            }
        }
    }

    fn execute_modification(
        &self,
        prepared: PreparedCatalogRequest,
        slot: &mut TransactionCheckout<'_>,
        mode: MutationMode,
    ) -> Result<ExecutionOutcome> {
        if slot
            .as_ref()
            .is_some_and(|transaction| transaction.descriptor().state() == TransactionState::Failed)
        {
            return Err(Error::in_failed_transaction());
        }
        if slot
            .as_ref()
            .is_none_or(|transaction| transaction.descriptor().state().is_terminal())
        {
            self.start_transaction_checked(slot, TransactionAccessMode::ReadWrite, false)?;
        }
        let transaction = slot
            .as_mut()
            .ok_or_else(Error::invalid_transaction_transition)?;
        Self::authorize_mutation(transaction, mode)?;
        let result = if let Some(command) = prepared.database_catalog_command().cloned() {
            let schema = &self.context.current_schema().path;
            self.inner.with_mutation_reservation(|_reservation| {
                ddl::stage(&self.inner, transaction.draft_mut()?, schema, command)
            })
        } else {
            self.inner
                .execute_prepared_detached_mutation(transaction, self.audit_bytes(), prepared)
        };
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => return Err(Self::fail_statement(transaction, error)),
        };
        let changes = outcome
            .write_summary()
            .map_or(0, crate::WriteSummary::change_count);
        transaction.record_statement(changes);
        if transaction.is_explicit() {
            return Ok(outcome);
        }
        #[cfg(test)]
        {
            let pause = self.inner.before_implicit_commit.lock().take();
            if let Some(pause) = pause {
                pause.staged.send(transaction.descriptor().clone()).unwrap();
                pause
                    .resume
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .expect("test must release the staged implicit transaction");
            }
        }
        match self.commit_transaction_checked(slot) {
            Ok(_) => Ok(outcome),
            Err(error) => Err(error),
        }
    }

    pub(super) fn validate_context_references(&self) -> Result<()> {
        let snapshot = CatalogReadSnapshot {
            state: self.inner.state.load_full(),
        };
        self.validate_context_snapshot(&snapshot)
    }

    pub(super) fn validate_context_snapshot(&self, snapshot: &CatalogReadSnapshot) -> Result<()> {
        let current_schema = self.context.current_schema();
        let current_graph = self.context.current_graph();
        let references_are_current = snapshot.matches_schema_reference(&current_schema)
            && snapshot.matches_graph_reference(&current_graph)
            && self
                .context
                .home_schema()
                .is_none_or(|schema| snapshot.matches_schema_reference(schema))
            && self
                .context
                .home_graph()
                .is_none_or(|graph| snapshot.matches_graph_reference(graph));
        if references_are_current {
            Ok(())
        } else {
            Err(Error::stale_session_reference())
        }
    }

    pub(super) fn audit_bytes(&self) -> Option<Arc<[u8]>> {
        self.context
            .principal()
            .and_then(crate::Principal::audit_bytes_arc)
    }
}

impl DatabaseInner {
    fn bind_cached_plan(
        &self,
        id: LowerGraphId,
        path: &ObjectPath,
        plan: PreparedCatalogPlan,
        input: selene_gql::RequestExecutionInput,
    ) -> Result<PreparedCatalogRequest> {
        self.with_graph_request(id, path, |graph| {
            let snapshot = graph.read();
            plan.bind(input, &snapshot).map_err(Error::from_engine)
        })
    }

    fn prepare_catalog_request(
        &self,
        id: LowerGraphId,
        path: &ObjectPath,
        audit: Option<Arc<[u8]>>,
        source: &str,
        request: selene_gql::RequestExecutionInput,
    ) -> Result<PreparedCatalogRequest> {
        self.with_graph_request(id, path, |graph| {
            prepare_on_graph(graph, audit, source, request, &self.procedures)
        })
    }

    fn prepare_catalog_request_from_snapshot(
        &self,
        snapshot: selene_graph::SeleneGraph,
        audit: Option<Arc<[u8]>>,
        source: &str,
        request: selene_gql::RequestExecutionInput,
    ) -> Result<PreparedCatalogRequest> {
        let scratch =
            SharedGraph::try_from_graph(snapshot).map_err(Error::invalid_graph_type_source)?;
        prepare_on_graph(&scratch, audit, source, request, &self.procedures)
    }

    fn execute_prepared_live_read(
        &self,
        id: LowerGraphId,
        path: &ObjectPath,
        audit: Option<Arc<[u8]>>,
        prepared: PreparedCatalogRequest,
    ) -> Result<ExecutionOutcome> {
        self.with_graph_request(id, path, |graph| {
            execute_read_on_graph(graph, audit, prepared, &self.procedures)
        })
    }

    fn execute_prepared_detached_read(
        &self,
        transaction: &DetachedTransaction,
        audit: Option<Arc<[u8]>>,
        prepared: PreparedCatalogRequest,
    ) -> Result<ExecutionOutcome> {
        let scratch = SharedGraph::try_from_graph(transaction.draft()?.selected_graph()?.clone())
            .map_err(Error::invalid_graph_type_source)?;
        execute_read_on_graph(&scratch, audit, prepared, &self.procedures)
    }

    fn execute_prepared_detached_mutation(
        &self,
        transaction: &mut DetachedTransaction,
        audit: Option<Arc<[u8]>>,
        prepared: PreparedCatalogRequest,
    ) -> Result<ExecutionOutcome> {
        let scratch = SharedGraph::try_from_graph(transaction.draft()?.selected_graph()?.clone())
            .map_err(Error::invalid_graph_type_source)?;
        let mut session = lower_session(&scratch, audit);
        let prepared = reprepare_if_stale(&scratch, &mut session, prepared, &self.procedures)?;
        if prepared.is_read_only() || prepared.is_database_catalog() {
            return Err(Error::catalog_invariant(
                "selected mutation changed category while replanning",
            ));
        }
        let staged = session
            .execute_prepared_catalog_request_unpublished(prepared, &self.procedures)
            .map_err(Error::from_engine)?;
        let (output, prepared_graph) = staged.into_parts();
        drop(session);
        drop(scratch);
        let selected_graph = transaction.draft()?.selected_graph_id()?;
        transaction
            .draft_mut()?
            .attach_prepared_graph(selected_graph, prepared_graph)?;
        ExecutionOutcome::from_engine(output)
    }

    pub(crate) fn with_graph_request<T>(
        &self,
        id: LowerGraphId,
        _path: &impl std::fmt::Display,
        execute: impl FnOnce(&SharedGraph) -> Result<T>,
    ) -> Result<T> {
        let observed = self.state.load_full();
        let instance = observed
            .graphs
            .get(&id)
            .cloned()
            .ok_or_else(Error::stale_session_reference)?;
        #[cfg(test)]
        let _depth = crate::database::GraphRequestDepth::enter();
        let _lease = instance.lifecycle.read();
        let current = self.state.load_full();
        if current
            .graphs
            .get(&id)
            .is_none_or(|registered| !Arc::ptr_eq(registered, &instance))
        {
            return Err(Error::stale_session_reference());
        }
        execute(&instance.graph)
    }
}

fn prepare_on_graph(
    graph: &SharedGraph,
    audit: Option<Arc<[u8]>>,
    source: &str,
    request: selene_gql::RequestExecutionInput,
    procedures: &selene_gql::BuiltinProcedureRegistry,
) -> Result<PreparedCatalogRequest> {
    lower_session(graph, audit)
        .prepare_source_catalog_request(source, procedures, request)
        .map_err(Error::from_engine)
}

fn execute_read_on_graph(
    graph: &SharedGraph,
    audit: Option<Arc<[u8]>>,
    prepared: PreparedCatalogRequest,
    procedures: &selene_gql::BuiltinProcedureRegistry,
) -> Result<ExecutionOutcome> {
    let mut session = lower_session(graph, audit);
    let prepared = reprepare_if_stale(graph, &mut session, prepared, procedures)?;
    match session
        .execute_prepared_catalog_request(prepared, procedures)
        .map_err(Error::from_engine)?
    {
        CatalogSessionOutput::RequestOutcome(output) => ExecutionOutcome::from_engine(output),
        _ => Err(Error::unsupported_engine_outcome()),
    }
}

fn reprepare_if_stale<'g>(
    graph: &SharedGraph,
    session: &mut selene_gql::Session<'g>,
    prepared: PreparedCatalogRequest,
    procedures: &selene_gql::BuiltinProcedureRegistry,
) -> Result<PreparedCatalogRequest> {
    let snapshot = graph.read();
    let stale = prepared.graph_id() != snapshot.graph_id()
        || prepared.graph_generation() != snapshot.meta.generation
        || prepared.schema_version() != graph.schema_version();
    drop(snapshot);
    if stale {
        session
            .reprepare_source_catalog_request(prepared, procedures)
            .map_err(Error::from_engine)
    } else {
        Ok(prepared)
    }
}

pub(super) fn lower_session<'g>(
    graph: &'g SharedGraph,
    audit: Option<Arc<[u8]>>,
) -> selene_gql::Session<'g> {
    match audit {
        Some(audit) => selene_gql::Session::with_principal(graph, audit),
        None => selene_gql::Session::new(graph),
    }
}
