//! Facade session context and selected-graph request leases.

use std::{cell::Cell, marker::PhantomData, sync::Arc};

use crate::{
    ExecutionOutcome, GeneralParameter, Request, RequestContext, RequestOutcome, RequestParams,
    RequestTimestamp, Result, SessionContext, database::DatabaseInner,
    params::validated_parameter_name,
};

pub(crate) mod cache;
pub(crate) mod control;
mod execution;
mod transaction;

/// Movable session selected to one catalog graph identity.
///
/// The session owns its database and a controlled [`SessionContext`], but no
/// runtime graph handle or catalog snapshot. Each call validates every copied
/// home/current stable identity, then rechecks the selected graph under a
/// temporary lifecycle read lease. A same-path recreation never rebinds the
/// session.
///
/// A session is `Send` but intentionally not `Sync`. It accepts one request at a
/// time and retains the actual active [`RequestContext`] in its request slot.
/// Sharing it across threads requires caller-owned synchronization; the facade
/// does not claim concurrent request execution.
///
/// ```compile_fail
/// fn require_sync<T: Sync>() {}
/// require_sync::<selene_db::Session>();
/// ```
///
/// Transaction controls use facade-owned detached state and the single Part 1
/// publication authority. Selected `SESSION SET`/`RESET` controls persist in
/// facade state, while `SESSION CLOSE` releases transaction state and rejects
/// future requests. Relative graph and graph-type references resolve against
/// the current session schema.
/// Successful catalog statements return
/// [`ExecutionOutcome::OmittedResult`]; their failures carry the same
/// [`ErrorKind`](crate::ErrorKind) and GQLSTATUS as the equivalent
/// [`Catalog`](crate::Catalog) call.
pub struct Session {
    pub(crate) inner: Arc<DatabaseInner>,
    pub(crate) context: SessionContext,
    not_sync: PhantomData<Cell<()>>,
}

impl Session {
    pub(crate) fn new(inner: Arc<DatabaseInner>, context: SessionContext) -> Self {
        Self {
            inner,
            context,
            not_sync: PhantomData,
        }
    }

    /// Return typed inspection of this session's dependencies and controlled state.
    #[must_use]
    pub const fn context(&self) -> &SessionContext {
        &self.context
    }

    /// Bind or replace one typed session parameter for subsequent requests.
    ///
    /// Request-scoped parameters shadow this dictionary by exact name without
    /// mutating it. A request already in progress keeps its immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns an invalid-name diagnostic when `name` would not parse after `$`.
    pub fn set_parameter(
        &self,
        name: &str,
        parameter: GeneralParameter,
    ) -> Result<Option<GeneralParameter>> {
        if self.context.termination() == crate::SessionTerminationState::Closed {
            return Err(crate::Error::session_closed());
        }
        let name = validated_parameter_name(name)?;
        Ok(self.context.set_parameter(name, parameter))
    }

    /// Remove one exact-name session parameter.
    ///
    /// # Errors
    ///
    /// Returns an invalid-name diagnostic when `name` would not parse after `$`.
    pub fn remove_parameter(&self, name: &str) -> Result<Option<GeneralParameter>> {
        if self.context.termination() == crate::SessionTerminationState::Closed {
            return Err(crate::Error::session_closed());
        }
        let name = validated_parameter_name(name)?;
        Ok(self.context.remove_parameter(name.as_str()))
    }

    /// Parse, plan, and execute one GQL statement.
    ///
    /// This compatibility entry point executes a [`Request`] with no
    /// request-scoped bindings and converts its [`RequestOutcome`] back to the
    /// existing `Result` shape. Session bindings still seed the request.
    ///
    /// # Errors
    ///
    /// Returns a facade-owned diagnostic for invalid GQL, unsupported stateful
    /// controls, a stale context reference, analysis/planning failures, or
    /// execution failures.
    pub fn execute(&self, source: &str) -> Result<ExecutionOutcome> {
        self.execute_request(Request::new(source)).into_result()
    }

    /// Execute one source statement with an explicit immutable request context.
    ///
    /// Session parameters are snapshotted first, then request parameters shadow
    /// exact names. The associated context remains active through graph
    /// execution and post-lease catalog dispatch. Every returned failure uses
    /// [`RequestOutcome::Failed`]. Panics are not caught; the request guard clears
    /// the slot while unwinding.
    #[must_use]
    pub fn execute_request(&self, request: Request) -> RequestOutcome {
        self.execute_request_with(request, RequestTimestamp::capture(), |_| {})
    }

    fn execute_request_with(
        &self,
        request: Request,
        timestamp: RequestTimestamp,
        active_hook: impl FnOnce(&Self),
    ) -> RequestOutcome {
        let merged =
            RequestParams::overlay(&self.context.parameter_snapshot(), request.parameters());
        let context = Arc::new(RequestContext::new(merged, timestamp));
        let guard = match self.context.activate_request(Arc::clone(&context)) {
            Ok(guard) => guard,
            Err(error) => return RequestOutcome::failed(context, error),
        };
        active_hook(self);
        let result = self.execute_active_request(&request, &context);
        drop(guard);
        match result {
            Ok(outcome) => RequestOutcome::Succeeded { context, outcome },
            Err(error) => RequestOutcome::failed(context, error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{panic::AssertUnwindSafe, sync::Arc};

    use crate::{
        CreatePolicy, Database, ErrorKind, ExecutionOutcome, GeneralParameter, GqlType, ObjectPath,
        Request, RequestOutcome, RequestParams, RequestSlotState, RequestTimestamp, SchemaPath,
        Value,
    };

    fn session() -> super::Session {
        let database = Database::builder().build();
        let catalog = database.catalog();
        let schema = SchemaPath::regular("selene", "request_guard").unwrap();
        let graph = ObjectPath::regular("selene", "request_guard", "main").unwrap();
        catalog
            .create_schema(&schema, CreatePolicy::Strict)
            .unwrap();
        catalog
            .create_graph(&graph, None, CreatePolicy::Strict)
            .unwrap();
        database.session(&graph).unwrap()
    }

    fn request_with_int(value: i64) -> Request {
        let mut params = RequestParams::new();
        params
            .insert(
                "p",
                GeneralParameter::new(GqlType::Integer, Value::Int(value)).unwrap(),
            )
            .unwrap();
        Request::with_params("RETURN $p", params)
    }

    fn request_with_any(source: &str, value: Value) -> Request {
        let mut params = RequestParams::new();
        params
            .insert("value", GeneralParameter::new(GqlType::Any, value).unwrap())
            .unwrap();
        Request::with_params(source, params)
    }

    fn outcome_int(outcome: &RequestOutcome) -> i64 {
        let ExecutionOutcome::Rows { result, .. } = outcome.execution().unwrap() else {
            panic!("expected rows");
        };
        let Value::Int(value) = result.rows()[0].values()[0] else {
            panic!("expected integer");
        };
        value
    }

    #[test]
    fn facade_cache_hits_rebind_values_and_survive_unrelated_catalog_changes() {
        let session = session();
        assert_eq!(session.context.plan_cache_stats(), (0, 0));
        session.execute("RETURN 1").unwrap();
        session.execute("RETURN 1").unwrap();
        assert_eq!(session.context.plan_cache_stats(), (1, 1));

        assert_eq!(
            outcome_int(&session.execute_request(request_with_int(7))),
            7
        );
        assert_eq!(
            outcome_int(&session.execute_request(request_with_int(9))),
            9
        );
        assert_eq!(session.context.plan_cache_stats(), (2, 2));

        crate::Catalog::new(Arc::clone(&session.inner))
            .create_schema(
                &SchemaPath::regular("selene", "cache_generation").unwrap(),
                CreatePolicy::Strict,
            )
            .unwrap();
        assert_eq!(
            outcome_int(&session.execute_request(request_with_int(11))),
            11
        );
        assert_eq!(session.context.plan_cache_stats(), (3, 2));
    }

    #[test]
    fn semantic_cache_separates_data_publication_from_schema_invalidation() {
        let session = session();
        let database = Database {
            inner: Arc::clone(&session.inner),
        };
        let writer = database
            .session(&session.context.current_graph().path)
            .unwrap();
        let query = "USE /request_guard/main MATCH (n:Item) RETURN count(n)";
        assert_eq!(
            outcome_int(&session.execute_request(Request::new(query))),
            0
        );
        assert_eq!(session.context.plan_cache_stats(), (0, 1));
        writer.execute("INSERT (:Item {body: 'text'})").unwrap();
        assert_eq!(
            outcome_int(&session.execute_request(Request::new(query))),
            1
        );
        assert_eq!(session.context.plan_cache_stats(), (1, 1));
        writer
            .execute("CALL selene.create_text_index('Item', 'body')")
            .unwrap();
        assert_eq!(
            outcome_int(&session.execute_request(Request::new(query))),
            1
        );
        assert_eq!(session.context.plan_cache_stats(), (1, 2));
    }

    #[test]
    fn facade_cache_hits_revalidate_inline_parameter_declarations() {
        let session = session();
        let source = "MATCH (n:NoRows) RETURN $value::STRING";
        let warm = session.execute_request(request_with_any(
            source,
            Value::String(selene_core::db_string("warm").unwrap()),
        ));
        assert_eq!(warm.execution().unwrap().row_count(), Some(0));

        let replay = session.execute_request(request_with_any(source, Value::Int(7)));
        assert_eq!(
            replay.error().unwrap().gqlstatus().unwrap().as_str(),
            "22G03"
        );
        assert_eq!(session.context.plan_cache_stats(), (1, 1));
    }

    #[test]
    fn active_slot_holds_actual_context_and_rejects_reentry() {
        let session = session();
        let timestamp = RequestTimestamp::from_parts(1_788_692_096, 123_456_789);
        let outcome =
            session.execute_request_with(Request::new("RETURN 1"), timestamp, |session| {
                assert_eq!(session.context().request_slot(), RequestSlotState::Active);
                let active = session
                    .context()
                    .current_request()
                    .expect("request is associated");
                assert_eq!(active.timestamp(), timestamp);

                let nested = session.execute_request(Request::new("RETURN 2"));
                assert_eq!(
                    nested.error().map(crate::Error::kind),
                    Some(ErrorKind::RequestAlreadyActive)
                );
                let still_active = session.context().current_request().unwrap();
                assert!(Arc::ptr_eq(&active, &still_active));
            });

        assert!(matches!(outcome, RequestOutcome::Succeeded { .. }));
        assert_eq!(session.context().request_slot(), RequestSlotState::Vacant);
        assert!(session.context().current_request().is_none());
    }

    #[test]
    fn panic_propagates_clears_slot_and_session_is_reusable() {
        let session = session();
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            session.execute_request_with(
                Request::new("RETURN 1"),
                RequestTimestamp::from_parts(1_788_692_096, 0),
                |_| panic!("injected request panic"),
            )
        }));

        assert!(panic.is_err());
        assert_eq!(session.context().request_slot(), RequestSlotState::Vacant);
        assert!(matches!(
            session.execute_request(Request::new("RETURN 1")),
            RequestOutcome::Succeeded { .. }
        ));
    }

    #[test]
    fn panic_marks_explicit_transaction_failed_and_discards_staged_work() {
        let session = session();
        session
            .start_transaction(crate::TransactionAccessMode::ReadWrite)
            .unwrap();
        session.execute("INSERT (:BeforePanic)").unwrap();
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            session.execute_request_with(
                Request::new("RETURN 1"),
                RequestTimestamp::from_parts(1_788_692_096, 0),
                |_| panic!("injected transaction request panic"),
            )
        }));

        assert!(panic.is_err());
        assert_eq!(
            session.context().transaction_slot(),
            crate::TransactionSlotState::Failed
        );
        session.rollback_transaction().unwrap();
        assert_eq!(
            session
                .execute("MATCH (n:BeforePanic) RETURN n")
                .unwrap()
                .row_count(),
            Some(0)
        );
    }

    #[test]
    fn transaction_id_exhaustion_is_fallible_and_never_wraps() {
        let database = Database::builder().build();
        let catalog = database.catalog();
        let schema = SchemaPath::regular("selene", "transaction_ids").unwrap();
        let graph = ObjectPath::regular("selene", "transaction_ids", "main").unwrap();
        catalog
            .create_schema(&schema, CreatePolicy::Strict)
            .unwrap();
        catalog
            .create_graph(&graph, None, CreatePolicy::Strict)
            .unwrap();
        catalog.inner.set_next_transaction_id(u64::MAX);
        let session = database.session(&graph).unwrap();

        let last = session
            .start_transaction(crate::TransactionAccessMode::ReadWrite)
            .unwrap();
        assert_eq!(last.id().get(), u64::MAX);
        session.rollback_transaction().unwrap();
        let error = session
            .start_transaction(crate::TransactionAccessMode::ReadWrite)
            .unwrap_err();
        assert_eq!(error.gqlstatus().unwrap().as_str(), "5GQL1");
        assert_eq!(
            session.context().transaction_slot(),
            crate::TransactionSlotState::RolledBack
        );
    }

    #[test]
    fn failed_and_terminal_transactions_release_all_detached_state() {
        let session = session();

        session
            .start_transaction(crate::TransactionAccessMode::ReadWrite)
            .unwrap();
        assert_eq!(
            session.context().transaction_retains_detached_state(),
            Some(true)
        );
        session.execute("INSERT (:Explicit)").unwrap();
        session.commit_transaction().unwrap();
        assert_eq!(
            session.context().transaction_retains_detached_state(),
            Some(false)
        );

        session
            .start_transaction(crate::TransactionAccessMode::ReadOnly)
            .unwrap();
        session.commit_transaction().unwrap();
        assert_eq!(
            session.context().transaction_retains_detached_state(),
            Some(false)
        );

        session
            .start_transaction(crate::TransactionAccessMode::ReadWrite)
            .unwrap();
        session.rollback_transaction().unwrap();
        assert_eq!(
            session.context().transaction_retains_detached_state(),
            Some(false)
        );

        session
            .start_transaction(crate::TransactionAccessMode::ReadWrite)
            .unwrap();
        session.execute("RETURN 1 / 0").unwrap_err();
        assert_eq!(
            session.context().transaction_retains_detached_state(),
            Some(false)
        );

        session.execute("ROLLBACK").unwrap();
        session.execute("INSERT (:Implicit)").unwrap();
        assert_eq!(
            session.context().transaction_retains_detached_state(),
            Some(false)
        );
    }
}
