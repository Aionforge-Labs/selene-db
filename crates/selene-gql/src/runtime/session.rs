//! Statement-session state for explicit transaction control.

use std::{
    borrow::Cow, cell::RefCell, collections::BTreeMap, num::NonZeroUsize, sync::Arc, time::Instant,
};

use selene_core::{CancellationToken, Change, DbString, Value};
use selene_graph::{CommitOutcome, SharedGraph, WriteTxn, write_txn::PreparedGraphCommit};

use crate::{
    GqlStatus, SourceSpan,
    plan::ImplDefinedCaps,
    runtime::{
        BindingTable, BindingTableAllocationError, BindingTableRegistry, CallPlanCache,
        ExecutorError, ExecutorWarning, PlanCache, PlanCacheStats, RequestExecutionInput,
        SharedPlanCache, WarningSink, WriteOutcome, request_runtime::RequestRuntime,
    },
};

/// Session-local query parameter value.
///
/// Scalar parameters are visible to statements as regular [`Value`]s. Table
/// parameters are registered in that statement's binding-table registry and
/// materialized as request-scoped `TableRef` values.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionParameterValue {
    /// Scalar query parameter value.
    Scalar(Value),
    /// Binding-table query parameter value.
    Table(Arc<BindingTable>),
}

/// Caller-owned executor session bound to one shared graph.
pub struct Session<'g> {
    graph: &'g SharedGraph,
    principal: Option<Arc<[u8]>>,
    pub(crate) parameters: BTreeMap<DbString, SessionParameterValue>,
    pub(crate) scalar_parameters: BTreeMap<DbString, Value>,
    pub(crate) plan_cache: Option<PlanCache>,
    pub(crate) shared_plan_cache: Option<Arc<SharedPlanCache>>,
    pub(crate) call_plan_cache: Option<Arc<CallPlanCache>>,
    pub(crate) active_txn: Option<WriteTxn<'g>>,
    pub(crate) aborted: bool,
    pub(crate) tx_started_at: Option<Instant>,
    pub(crate) tx_statement_count: u32,
    pub(crate) cancellation: Option<CancellationToken>,
    pub(crate) deadline: Option<Instant>,
    pub(crate) max_nodes_scanned: Option<usize>,
    pub(crate) row_cap: Option<usize>,
    pub(crate) warning_sink: Option<RefCell<Box<dyn WarningSink>>>,
    /// When set, `execute_source` runs the optimizer with a snapshot-pinned
    /// [`LiveIndexCatalog`] so label / typed / composite index access paths are
    /// selected. Default `true` (greenfield default-on). Toggle off via
    /// [`Session::without_index_selection`] to lower the byte-identical Linear
    /// plan (perf-baseline pinning / debugging).
    pub(crate) index_selection: bool,
    /// Session-local time-zone displacement (ISO/IEC 39075:2024 section 4.5.2.1).
    ///
    /// `None` is the ID048 default (UTC); `SESSION SET TIME ZONE` sets it and
    /// `SESSION RESET TIME ZONE` clears it back to `None`. The threaded value
    /// is consumed by the section 20.27 current-datetime functions.
    pub(crate) time_zone: Option<jiff::tz::TimeZone>,
    /// Explicit facade request metadata; direct lower sessions leave this absent.
    pub(crate) request: Option<RequestExecutionInput>,
    /// Selected-facade execution prepares instead of graph-locally publishing.
    pub(crate) prepare_unpublished: bool,
    /// Owned result of one selected-facade unpublished write.
    pub(crate) prepared_graph: Option<PreparedGraphCommit>,
    #[cfg(test)]
    pub(crate) before_statement_execution: Option<Box<dyn FnOnce() + 'g>>,
    /// Session termination flag (ISO/IEC 39075:2024 section 7.3).
    ///
    /// Set by `SESSION CLOSE`; once set, every subsequent `execute_source`
    /// request returns [`ExecutorError::SessionClosed`].
    pub(crate) closed: bool,
    /// Embedder-configured implementation-defined planning/runtime caps
    /// (ISO IL013/IL015/IL018 limit surfaces). Defaults to
    /// [`ImplDefinedCaps::DEFAULT`]; overridden via
    /// [`Session::with_impl_defined_caps`]. Passed into `build_plan`, so it is
    /// baked into every lowered plan and consulted by the plan-time quantifier
    /// gate as well as the runtime/optimizer cap checks.
    pub(crate) caps: ImplDefinedCaps,
}

pub(crate) fn materialize_parameter_values<'a>(
    parameters: &'a BTreeMap<DbString, SessionParameterValue>,
    scalar_parameters: &'a BTreeMap<DbString, Value>,
    registry: &BindingTableRegistry,
) -> Result<Cow<'a, BTreeMap<DbString, Value>>, BindingTableAllocationError> {
    if parameters
        .values()
        .all(|value| matches!(value, SessionParameterValue::Scalar(_)))
    {
        return Ok(Cow::Borrowed(scalar_parameters));
    }

    let mut materialized = scalar_parameters.clone();
    for (name, value) in parameters {
        if let SessionParameterValue::Table(table) = value {
            materialized.insert(
                name.clone(),
                Value::TableRef(registry.register(Arc::clone(table))?),
            );
        }
    }
    Ok(Cow::Owned(materialized))
}

/// Metadata returned after committing an explicit transaction through a [`Session`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct TransactionOutcome {
    /// Total changes aggregated across all statements in the transaction.
    pub changes: Vec<Change>,
    /// Graph generation published by the commit.
    pub generation: u64,
    /// Next node ID after the commit.
    pub next_node_id: u64,
    /// Next edge ID after the commit.
    pub next_edge_id: u64,
    /// Durable-sequence slot; always `None` from the in-graph publisher.
    /// Sequence assignment below the graph layer is the owning database
    /// handle's authority.
    pub durable_at: Option<u64>,
    /// Wall-clock duration from `start_transaction` to commit completion.
    pub duration_micros: u64,
    /// Number of accepted non-control statements in the transaction window.
    pub statement_count: u32,
}

impl TransactionOutcome {
    pub(crate) fn into_write_outcome(self) -> WriteOutcome {
        WriteOutcome {
            rows: None,
            changes: self.changes,
            generation: self.generation,
            next_node_id: self.next_node_id,
            next_edge_id: self.next_edge_id,
            durable_at: self.durable_at,
        }
    }
}

/// Metadata returned after rolling back an explicit transaction through a [`Session`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RollbackOutcome {
    /// Count of changes discarded by the rollback.
    pub discarded_changes: usize,
    /// Number of accepted non-control statements in the transaction window.
    pub statement_count: u32,
    /// Wall-clock duration from `start_transaction` to rollback completion.
    pub duration_micros: u64,
}

impl<'g> Session<'g> {
    /// Create a session without commit-principal bytes.
    #[must_use]
    pub const fn new(graph: &'g SharedGraph) -> Self {
        Self {
            graph,
            principal: None,
            parameters: BTreeMap::new(),
            scalar_parameters: BTreeMap::new(),
            plan_cache: None,
            shared_plan_cache: None,
            call_plan_cache: None,
            active_txn: None,
            aborted: false,
            tx_started_at: None,
            tx_statement_count: 0,
            cancellation: None,
            deadline: None,
            max_nodes_scanned: None,
            row_cap: None,
            warning_sink: None,
            index_selection: true,
            time_zone: None,
            request: None,
            prepare_unpublished: false,
            prepared_graph: None,
            #[cfg(test)]
            before_statement_execution: None,
            closed: false,
            caps: ImplDefinedCaps::DEFAULT,
        }
    }

    /// Create a session that forwards opaque principal bytes to commits.
    #[must_use]
    pub fn with_principal(graph: &'g SharedGraph, principal: Arc<[u8]>) -> Self {
        let mut session = Self::new(graph);
        session.principal = Some(principal);
        session
    }

    /// Attach a cooperative cancellation token to subsequent statements.
    ///
    /// Cancellation is cooperative: statements observe the token at executor,
    /// built-in-procedure, and algorithm checkpoints. If a statement inside an
    /// explicit transaction returns `Cancelled`, the transaction enters the
    /// failed state until `ROLLBACK`.
    #[must_use]
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation = Some(token);
        self
    }

    /// Attach an absolute per-statement deadline to subsequent statements.
    ///
    /// The deadline is compared with `Instant::now()` at the same cooperative
    /// checkpoints as cancellation. Expiry returns `Timeout`; inside an
    /// explicit transaction that also marks the transaction failed until
    /// `ROLLBACK`.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Attach a deterministic per-statement node-scan budget.
    ///
    /// Scan-heavy graph and procedure loops debit this budget at batch
    /// boundaries. Exceeding it returns GQLSTATUS `5GQL1`
    /// (program-limit-exceeded); inside an explicit transaction that also
    /// marks the transaction failed until `ROLLBACK`.
    #[must_use]
    pub fn with_max_nodes_scanned(mut self, max_nodes: usize) -> Self {
        self.max_nodes_scanned = Some(max_nodes);
        self
    }

    /// Set the implementation-defined planning/runtime caps for subsequent
    /// statements (ISO IL013/IL015/IL018 limit surfaces — e.g.
    /// [`max_quantifier`](ImplDefinedCaps::max_quantifier), set-op / `GROUP BY`
    /// key caps, optimizer-iteration, string/byte-string concat length,
    /// path-length, and list-cardinality bounds).
    ///
    /// The caps are baked into every plan lowered for this session, so they are
    /// honored by both the plan-time variable-length quantifier gate and the
    /// runtime/optimizer cap checks. Defaults to [`ImplDefinedCaps::DEFAULT`].
    #[must_use]
    pub fn with_impl_defined_caps(mut self, caps: ImplDefinedCaps) -> Self {
        self.caps = caps;
        self
    }

    /// Attach an outermost result-row cap to subsequent statements.
    ///
    /// The cap is enforced only at the statement output boundary. Intermediate
    /// rows produced by scans, joins, `FOR`, or other pipeline operators do
    /// not count against it. Exceeding the cap returns `RowCapExceeded`; inside
    /// an explicit transaction that marks the transaction failed until
    /// `ROLLBACK`.
    #[must_use]
    pub fn with_row_cap(mut self, max_rows: usize) -> Self {
        self.row_cap = Some(max_rows);
        self
    }

    /// Attach an opt-in runtime warning sink to subsequent statements.
    ///
    /// Sessions without a sink silently discard warnings. The sink currently
    /// receives ISO warning records such as `01G11` for aggregate NULL
    /// elimination and `01N01` for relaxed validation-mode writes; see
    /// `docs/embedding-guide.md` for an embedder-side collection example.
    #[must_use]
    pub fn with_warning_sink(mut self, sink: impl WarningSink + 'static) -> Self {
        self.warning_sink = Some(RefCell::new(Box::new(sink)));
        self
    }

    /// Bind or replace a session-local query parameter.
    ///
    /// Parameters are named without the leading `$` and are resolved by
    /// `$name` references during statement execution. Binding is an upsert:
    /// rebinding a name replaces the prior value and affects subsequent
    /// statements only. Parameters are session-level metadata, so transaction
    /// boundaries and [`Self::abort`] preserve the map. Parameters not
    /// referenced by a statement are ignored. Session plan-cache keys remain
    /// source-only; parameter values and runtime types are checked during each
    /// execution.
    ///
    /// Runtime positions that require a specific type validate strictly; for
    /// example, `LIMIT $n` accepts only non-negative integer values and returns
    /// [`ExecutorError::InvalidParameterType`] for mismatches.
    ///
    /// If `name` previously held a table binding, the table is replaced and
    /// `None` is returned. Use [`Self::bind_table_parameter`] when callers need
    /// table-aware replacement information.
    pub fn bind_parameter(&mut self, name: DbString, value: Value) -> Option<Value> {
        self.scalar_parameters.insert(name.clone(), value.clone());
        match self
            .parameters
            .insert(name, SessionParameterValue::Scalar(value))
        {
            Some(SessionParameterValue::Scalar(prior)) => Some(prior),
            Some(SessionParameterValue::Table(_)) | None => None,
        }
    }

    /// Bind or replace a session-local query parameter with a binding table.
    ///
    /// The table is stored at session scope and materialized into a fresh
    /// request-scoped table reference for each statement execution.
    pub fn bind_table_parameter(
        &mut self,
        name: DbString,
        table: BindingTable,
    ) -> Option<SessionParameterValue> {
        self.scalar_parameters.remove(&name);
        self.parameters
            .insert(name, SessionParameterValue::Table(Arc::new(table)))
    }

    /// Remove one session-local query parameter and return its prior scalar value.
    ///
    /// If `name` held a table binding, the table is removed and `None` is
    /// returned.
    pub fn clear_parameter(&mut self, name: &DbString) -> Option<Value> {
        self.scalar_parameters.remove(name);
        match self.parameters.remove(name) {
            Some(SessionParameterValue::Scalar(prior)) => Some(prior),
            Some(SessionParameterValue::Table(_)) | None => None,
        }
    }

    /// Remove all session-local query parameters.
    pub fn clear_parameters(&mut self) {
        self.parameters.clear();
        self.scalar_parameters.clear();
    }

    /// True when a session-local parameter named `name` is currently bound.
    ///
    /// Used to honor `SESSION SET VALUE IF NOT EXISTS` (ISO section 7.4): an
    /// existing binding is left untouched.
    #[must_use]
    pub(crate) fn has_parameter(&self, name: &DbString) -> bool {
        self.parameters.contains_key(name)
    }

    /// Set the session-local time-zone displacement (ISO feature GS15).
    ///
    /// Consumed by the section 20.27 current-datetime functions; persists across
    /// transaction boundaries like the other session characteristics.
    pub(crate) fn set_time_zone(&mut self, zone: jiff::tz::TimeZone) {
        self.time_zone = Some(zone);
    }

    /// Reset the session time zone to the ID048 default, UTC (ISO feature GS07).
    pub(crate) fn reset_time_zone(&mut self) {
        self.time_zone = None;
    }

    /// Return the time zone temporal evaluation should use for this session.
    ///
    /// `None` maps to the ID048 default of UTC.
    #[must_use]
    pub(crate) fn effective_time_zone(&self) -> jiff::tz::TimeZone {
        self.time_zone.clone().unwrap_or(jiff::tz::TimeZone::UTC)
    }

    /// Return one timestamp for the statement, preserving an explicit facade request instant.
    #[must_use]
    pub(crate) fn effective_request_timestamp(&self) -> jiff::Timestamp {
        self.request
            .as_ref()
            .map_or_else(jiff::Timestamp::now, |request| request.timestamp)
    }

    /// Clone this explicit request's runtime, or create one for a direct statement.
    pub(crate) fn execution_runtime(&self) -> Arc<RequestRuntime> {
        self.request.as_ref().map_or_else(
            || Arc::new(RequestRuntime::new()),
            RequestExecutionInput::runtime,
        )
    }

    /// Reset every session characteristic (ISO feature GS04).
    ///
    /// Clears all session parameters and resets the time zone to its default.
    pub(crate) fn reset_characteristics(&mut self) {
        self.clear_parameters();
        self.reset_time_zone();
    }

    /// Reset all session parameters, leaving other characteristics (ISO feature GS08).
    pub(crate) fn reset_parameters(&mut self) {
        self.clear_parameters();
    }

    /// Reset one named session parameter (ISO feature GS16).
    pub(crate) fn reset_parameter(&mut self, name: &DbString) {
        self.clear_parameter(name);
    }

    /// Mark this session closed (ISO/IEC 39075:2024 section 7.3).
    ///
    /// Any active explicit transaction is rolled back first so a closed session
    /// leaves no dangling write lock.
    pub(crate) fn close(&mut self) {
        self.abort();
        self.closed = true;
    }

    /// True when `SESSION CLOSE` has terminated this session.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Borrow the session-local query-parameter map used for statement execution.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn parameters(&self) -> &BTreeMap<DbString, SessionParameterValue> {
        &self.parameters
    }

    #[cfg(test)]
    pub(crate) fn materialize_parameters<'a>(
        &'a self,
        registry: &BindingTableRegistry,
    ) -> Result<Cow<'a, BTreeMap<DbString, Value>>, BindingTableAllocationError> {
        materialize_parameter_values(&self.parameters, &self.scalar_parameters, registry)
    }

    /// Disable optimizer index selection; all scans fall back to
    /// [`ScanAccess::Linear`](crate::ScanAccess::Linear).
    ///
    /// With index selection off, `execute_source` skips the optimizer entirely
    /// and lowers the byte-identical Linear plan (and EXPLAIN output) of
    /// pre-optimizer-wiring HEAD. This is the escape hatch for committed
    /// perf-baseline reproduction and access-path debugging.
    #[must_use]
    pub const fn without_index_selection(mut self) -> Self {
        self.index_selection = false;
        self
    }

    /// (Re-)enable optimizer index selection (the default).
    ///
    /// When enabled, `execute_source` builds a snapshot-pinned
    /// [`LiveIndexCatalog`](crate::LiveIndexCatalog) per cache-miss statement
    /// and runs the optimizer so label / typed / composite index access paths
    /// are selected. Linear remains the always-correct fallback inside every
    /// rule, so results are byte-identical to the disabled path.
    #[must_use]
    pub const fn with_index_selection(mut self) -> Self {
        self.index_selection = true;
        self
    }

    /// Enable this session's source-string plan cache with the given capacity.
    ///
    /// The cache is Session-local and invalidates entries when the backing
    /// graph's schema-version epoch changes.
    #[must_use]
    pub fn with_plan_cache(mut self, capacity: NonZeroUsize) -> Self {
        self.plan_cache = Some(PlanCache::new(capacity));
        self
    }

    /// Enable this session's shared non-CALL source-string plan cache.
    ///
    /// Embedders should pass one shared cache per graph so short-lived
    /// sessions can reuse read and write plans across requests. The cache key
    /// includes graph ID, schema-version epoch, procedure-registry version,
    /// source text, implementation-defined caps, and optimizer
    /// index-selection mode.
    #[must_use]
    pub fn with_shared_plan_cache(mut self, cache: Arc<SharedPlanCache>) -> Self {
        self.shared_plan_cache = Some(cache);
        self
    }

    /// Enable this session's shared procedure-CALL plan cache.
    ///
    /// Embedders should pass one shared cache per graph so short-lived
    /// sessions can reuse procedure-call plans across requests. The cache key
    /// includes the graph ID, schema-version epoch, and procedure-registry
    /// version.
    #[must_use]
    pub fn with_call_plan_cache(mut self, cache: Arc<CallPlanCache>) -> Self {
        self.call_plan_cache = Some(cache);
        self
    }

    /// Return this session's plan-cache counters, if caching is enabled.
    #[must_use]
    pub fn plan_cache_stats(&self) -> Option<PlanCacheStats> {
        self.plan_cache.as_ref().map(PlanCache::stats)
    }

    /// Clear this session's cached plans without resetting counters.
    pub fn clear_plan_cache(&mut self) {
        if let Some(cache) = self.plan_cache.as_mut() {
            cache.clear();
        }
    }

    /// Borrow the graph this session executes against.
    #[must_use]
    pub(crate) const fn graph(&self) -> &'g SharedGraph {
        self.graph
    }

    #[cfg(test)]
    pub(crate) fn with_before_statement_execution(mut self, hook: impl FnOnce() + 'g) -> Self {
        self.before_statement_execution = Some(Box::new(hook));
        self
    }

    #[cfg(test)]
    pub(crate) fn run_before_statement_execution(&mut self) {
        if let Some(hook) = self.before_statement_execution.take() {
            hook();
        }
    }

    /// Clone the principal bytes for a commit boundary.
    #[must_use]
    pub(crate) fn principal(&self) -> Option<Arc<[u8]>> {
        self.principal.clone()
    }

    /// Return true when the session owns an explicit write transaction.
    #[must_use]
    pub const fn has_active_txn(&self) -> bool {
        self.active_txn.is_some()
    }

    /// Return true when the active explicit transaction is aborted.
    #[must_use]
    pub const fn is_aborted(&self) -> bool {
        self.aborted
    }

    /// Open an explicit write transaction.
    ///
    /// Subsequent non-control statements executed through this session run
    /// inside the transaction until [`Self::commit_transaction`] or
    /// [`Self::rollback_transaction`] closes it.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError::TransactionAlreadyActive`] when this session
    /// already owns an explicit transaction.
    pub fn start_transaction(&mut self) -> Result<(), ExecutorError> {
        if self.active_txn.is_some() {
            return Err(ExecutorError::TransactionAlreadyActive {
                span: SourceSpan::default(),
            });
        }
        self.active_txn = Some(self.graph.begin_write());
        self.tx_started_at = Some(Instant::now());
        self.tx_statement_count = 0;
        self.aborted = false;
        Ok(())
    }

    /// Commit the open explicit transaction.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError::NoActiveTransaction`] when no explicit
    /// transaction is open, [`ExecutorError::InFailedTransaction`] when the
    /// transaction has been aborted by a failed statement, or
    /// [`ExecutorError::GraphMutation`] when the graph commit is rejected.
    pub fn commit_transaction(&mut self) -> Result<TransactionOutcome, ExecutorError> {
        if self.aborted {
            if let Some(txn) = self.active_txn.take() {
                txn.rollback();
            }
            self.clear_tx_state();
            return Err(ExecutorError::InFailedTransaction {
                span: SourceSpan::default(),
            });
        }
        let txn = self
            .active_txn
            .take()
            .ok_or(ExecutorError::NoActiveTransaction {
                span: SourceSpan::default(),
            })?;
        let statement_count = self.tx_statement_count;
        let outcome = txn.commit_with_principal(self.principal.clone());
        let duration_micros = self.tx_duration_micros();
        self.clear_tx_state();
        let outcome = outcome.map_err(|source| ExecutorError::GraphMutation {
            source,
            span: SourceSpan::default(),
        })?;
        emit_commit_warnings(&outcome, self.warning_sink.as_ref());
        Ok(TransactionOutcome {
            changes: outcome.changes,
            generation: outcome.generation,
            next_node_id: outcome.next_node_id,
            next_edge_id: outcome.next_edge_id,
            durable_at: outcome.durable_at,
            duration_micros,
            statement_count,
        })
    }

    /// Roll back the open explicit transaction.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError::NoActiveTransaction`] when no explicit
    /// transaction is open.
    pub fn rollback_transaction(&mut self) -> Result<RollbackOutcome, ExecutorError> {
        let txn = self
            .active_txn
            .take()
            .ok_or(ExecutorError::NoActiveTransaction {
                span: SourceSpan::default(),
            })?;
        let discarded_changes = txn.change_count();
        let statement_count = self.tx_statement_count;
        let duration_micros = self.tx_duration_micros();
        txn.rollback();
        self.clear_tx_state();
        Ok(RollbackOutcome {
            discarded_changes,
            statement_count,
            duration_micros,
        })
    }

    /// Roll back and clear the explicit transaction, when one is active.
    pub fn abort(&mut self) {
        if let Some(txn) = self.active_txn.take() {
            txn.rollback();
        }
        self.clear_tx_state();
    }

    fn tx_duration_micros(&self) -> u64 {
        self.tx_started_at
            .map_or(0, |started| started.elapsed().as_micros() as u64)
    }

    fn clear_tx_state(&mut self) {
        self.aborted = false;
        self.tx_started_at = None;
        self.tx_statement_count = 0;
    }
}

fn emit_commit_warnings(
    outcome: &CommitOutcome,
    warning_sink: Option<&RefCell<Box<dyn WarningSink>>>,
) {
    let Some(sink) = warning_sink else {
        return;
    };
    for warning in &outcome.warnings {
        sink.borrow_mut().emit(ExecutorWarning {
            code: GqlStatus::VALIDATION_MODE_RELAXED_WRITE,
            message: warning.warning.violation.to_string(),
            span: SourceSpan::default(),
        });
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;
