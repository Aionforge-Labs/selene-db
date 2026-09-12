//! Executor transaction context.

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fmt,
    sync::Arc,
    time::Instant,
};

use rustc_hash::{FxHashMap, FxHashSet};
use selene_core::{
    BindingTableId, CancellationCause, CancellationChecker, CancellationToken, DbString,
    NodeScanBudget, Value, metrics,
};
use selene_graph::{IndexProvider, Mutator, SeleneGraph, SharedGraph, WriteTxn};

use super::batch::budget::BatchCancel;
use crate::{
    GqlStatus, ProcedureRegistry, SourceSpan,
    analyze::{ExprId, ExprIdLookup},
    plan::SubqueryRegistry,
    plan::{ImplDefinedCaps, PipelineOpId},
    runtime::{
        BindingTable, BindingTableLookupError, BindingTableRegistry, BindingTableSchema,
        ExecutorError, ExecutorWarning, GqlStatusObject, WarningSink,
        request_runtime::RequestRuntime,
    },
};

mod construction;

/// Adaptive re-optimization hook reserved for future executor phases.
pub trait AdaptiveOptimizer: Send + Sync {
    /// Observe output cardinality for one pipeline operation.
    fn observe_cardinality(&self, _op: PipelineOpId, _rows: u64) {}
}

/// Row cadence for cooperative cancellation checkpoints.
pub(crate) const CANCEL_CHECK_STRIDE: usize = 1024;

/// Executor context for one statement.
///
/// Read-only contexts own an immutable graph snapshot so every scan in a
/// statement observes the same generation even if concurrent writers publish a
/// newer snapshot through [`selene_graph::SharedGraph`]. Write contexts also
/// carry a transaction-local working graph; reads then observe that working
/// graph so intra-statement writes are visible to later operators.
pub struct TxContext<'a, 'g> {
    snapshot: Arc<SeleneGraph>,
    impl_defined_caps: &'a ImplDefinedCaps,
    registry: &'a dyn ProcedureRegistry,
    providers: &'a [Arc<dyn IndexProvider>],
    parameters: Cow<'a, BTreeMap<DbString, Value>>,
    request_runtime: Arc<RequestRuntime>,
    reopt_hook: Option<&'a dyn AdaptiveOptimizer>,
    plan_expr_ids: Option<&'a ExprIdLookup>,
    plan_subqueries: Option<&'a SubqueryRegistry>,
    cancellation: Option<&'a CancellationToken>,
    deadline: Option<Instant>,
    node_scan_budget: Option<&'a NodeScanBudget>,
    row_cap: Option<usize>,
    warning_sink: Option<&'a RefCell<Box<dyn WarningSink>>>,
    emitted_warnings: RefCell<FxHashSet<(GqlStatus, SourceSpan)>>,
    result_rows_emitted: Cell<usize>,
    write_txn: Option<&'a mut WriteTxn<'g>>,
    maintenance_graph: Option<&'g SharedGraph>,
    session_time_zone: jiff::tz::TimeZone,
    request_timestamp: jiff::Timestamp,
    /// GQLRT-05 per-statement memo of correlated-subquery target schemas, keyed
    /// by the subquery expression id. Within one statement an EXISTS/COUNT
    /// expression is evaluated by exactly one filter/project op whose input
    /// schema is loop-invariant, so the target schema is identical for every
    /// outer row — caching it elides the per-row `schema_for_pattern` join-tree
    /// walk + `BTreeMap` of hidden slots. The id is 1:1 with the source schema
    /// within a statement, so the expression id alone is a sufficient key.
    subquery_target_schema: RefCell<FxHashMap<ExprId, BindingTableSchema>>,
}

/// Expression-evaluation context for one planned execution point.
///
/// Expression subqueries are planned into side tables on the execution plan.
/// The evaluator borrows those side tables through this wrapper while all
/// graph, parameter, and procedure access continues to flow through
/// [`TxContext`]. Reference-only, so batch operator trees share one value per
/// execution.
#[derive(Clone, Copy)]
pub struct EvalCtx<'a, 'ctx, 'g, 'plan> {
    /// Transaction context for graph and parameter access.
    pub tx: &'a TxContext<'ctx, 'g>,
    /// Plan-owned expression IDs cloned from analyzer output.
    pub expr_ids: &'plan ExprIdLookup,
    /// Plan-owned expression-subquery registry.
    pub subqueries: &'plan SubqueryRegistry,
}

impl<'a, 'ctx, 'g, 'plan> EvalCtx<'a, 'ctx, 'g, 'plan> {
    /// Borrow the same transaction context with a different plan registry.
    #[must_use]
    pub const fn with_plan<'next>(
        &self,
        expr_ids: &'next ExprIdLookup,
        subqueries: &'next SubqueryRegistry,
    ) -> EvalCtx<'a, 'ctx, 'g, 'next> {
        EvalCtx {
            tx: self.tx,
            expr_ids,
            subqueries,
        }
    }

    /// Borrow the planner/executor implementation-defined caps.
    #[must_use]
    pub const fn impl_defined_caps(&self) -> &'ctx ImplDefinedCaps {
        self.tx.impl_defined_caps()
    }
}

impl<'a, 'g> TxContext<'a, 'g> {
    /// Attach per-statement cooperative cancellation, scan-budget, and output row-cap limits.
    #[must_use]
    pub fn with_resource_limits(
        mut self,
        cancellation: Option<&'a CancellationToken>,
        deadline: Option<Instant>,
        row_cap: Option<usize>,
        node_scan_budget: Option<&'a NodeScanBudget>,
    ) -> Self {
        self.cancellation = cancellation;
        self.deadline = deadline;
        self.node_scan_budget = node_scan_budget;
        self.row_cap = row_cap;
        self
    }

    /// Attach the session warning sink visible to runtime operators.
    #[must_use]
    pub const fn with_warning_sink(
        mut self,
        warning_sink: Option<&'a RefCell<Box<dyn WarningSink>>>,
    ) -> Self {
        self.warning_sink = warning_sink;
        self
    }

    /// Attach the session time-zone displacement visible to temporal functions.
    ///
    /// The section 20.27 current-datetime functions read this through
    /// [`Self::session_time_zone`]. Defaults to UTC (ID048) when not set.
    #[must_use]
    pub fn with_session_time_zone(mut self, zone: jiff::tz::TimeZone) -> Self {
        self.session_time_zone = zone;
        self
    }

    /// Borrow the session time-zone displacement for temporal evaluation.
    ///
    /// Per ISO/IEC 39075:2024 section 4.5.2.1 the current-datetime functions
    /// evaluate against the session time zone; the default is UTC (ID048).
    #[must_use]
    pub fn session_time_zone(&self) -> &jiff::tz::TimeZone {
        &self.session_time_zone
    }

    /// Render this statement's request timestamp in the session time zone.
    ///
    /// ISO/IEC 39075:2024 section 20.27 sets the current request timestamp once
    /// before evaluating a datetime value function, so all current-datetime
    /// reads within this statement share this instant.
    #[must_use]
    pub fn request_timestamp_zoned(&self) -> jiff::Zoned {
        self.request_timestamp
            .to_zoned(self.session_time_zone.clone())
    }

    /// Return the cached target schema for the correlated subquery `expr_id`,
    /// computing and memoizing it on first use within this statement (GQLRT-05).
    ///
    /// The target schema is loop-invariant across the outer rows that drive a
    /// correlated `EXISTS`/`COUNT` subquery (its source schema is the enclosing
    /// op's input schema), so this avoids rebuilding it — and the relatively
    /// expensive `schema_for_pattern` join-tree walk it performs — per row.
    /// `compute` is invoked at most once per `expr_id` per statement and must
    /// not re-enter this method (it does not: schema construction is pure).
    pub(crate) fn cached_subquery_schema(
        &self,
        expr_id: ExprId,
        compute: impl FnOnce() -> Result<BindingTableSchema, ExecutorError>,
    ) -> Result<BindingTableSchema, ExecutorError> {
        if let Some(schema) = self.subquery_target_schema.borrow().get(&expr_id) {
            return Ok(schema.clone());
        }
        let schema = compute()?;
        self.subquery_target_schema
            .borrow_mut()
            .insert(expr_id, schema.clone());
        Ok(schema)
    }

    /// Emit one runtime warning if the session opted into warning collection.
    pub(crate) fn emit_warning(&self, warning: ExecutorWarning) {
        self.request_runtime
            .record_status(GqlStatusObject::new(warning.code, warning.message.clone()));
        if let Some(sink) = self.warning_sink {
            sink.borrow_mut().emit(warning);
        }
    }

    /// Emit one warning once for a planned expression span within this statement.
    pub(crate) fn emit_warning_once(&self, warning: ExecutorWarning) {
        let key = (warning.code, warning.span);
        if self.emitted_warnings.borrow_mut().insert(key) {
            self.emit_warning(warning);
        }
    }

    /// Check the token and deadline at a cooperative cancellation point.
    pub(crate) fn check_cancellation(&self) -> Result<(), ExecutorError> {
        self.cancellation_checker()
            .check()
            .map_err(|cause| self.cancellation_error(cause, SourceSpan::default()))
    }

    /// Accumulate processed rows and check cancellation when the stride is reached.
    pub(crate) fn check_cancellation_stride(
        &self,
        rows_since_check: &mut usize,
        rows: usize,
    ) -> Result<(), ExecutorError> {
        *rows_since_check = rows_since_check.saturating_add(rows);
        if *rows_since_check >= CANCEL_CHECK_STRIDE {
            self.check_cancellation()?;
            *rows_since_check = 0;
        }
        Ok(())
    }

    /// Count outermost result rows and enforce the optional row cap.
    pub(crate) fn note_result_rows(&self, n: usize) -> Result<(), ExecutorError> {
        let Some(cap) = self.row_cap else {
            return Ok(());
        };
        let Some(next) = self.result_rows_emitted.get().checked_add(n) else {
            return Err(ExecutorError::RowCapExceeded {
                cap,
                span: SourceSpan::default(),
            });
        };
        self.result_rows_emitted.set(next);
        if next > cap {
            return Err(ExecutorError::RowCapExceeded {
                cap,
                span: SourceSpan::default(),
            });
        }
        Ok(())
    }

    /// Build a checker that can cross into the native algorithms crate and
    /// built-in procedures.
    #[must_use]
    pub(crate) const fn cancellation_checker(&self) -> CancellationChecker<'a> {
        CancellationChecker::new_with_node_scan_budget(
            self.cancellation,
            self.deadline,
            self.node_scan_budget,
        )
    }

    /// Build the cooperative batch-execution checkpoint for this statement.
    ///
    /// Batch operators observe the same cancellation token, deadline, and
    /// deterministic node-scan budget as the row executor, with the identical
    /// GQLSTATUS mapping (`5GQL2` cancel, `5GQL3` timeout, `5GQL1`
    /// scan-budget exhaustion).
    #[must_use]
    pub(crate) fn batch_cancel(&self) -> BatchCancel<'_> {
        BatchCancel::new(self.cancellation, self.deadline, self.node_scan_budget)
    }

    /// Return the configured absolute deadline for this statement, if any.
    #[must_use]
    pub(crate) const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn cancellation_error(
        &self,
        cause: CancellationCause,
        span: SourceSpan,
    ) -> ExecutorError {
        match cause {
            CancellationCause::Cancelled => {
                metrics::counter_inc(metrics::CANCELLATIONS_TOTAL);
                ExecutorError::Cancelled { span }
            }
            CancellationCause::Timeout { elapsed } => {
                metrics::counter_inc(metrics::CANCELLATIONS_TOTAL);
                ExecutorError::Timeout {
                    deadline: self.deadline.unwrap_or_else(Instant::now),
                    elapsed,
                    span,
                }
            }
            CancellationCause::NodeScanBudgetExceeded { .. } => {
                ExecutorError::ProgramLimitExceeded {
                    detail: "node scan budget exceeded",
                    span,
                }
            }
        }
    }

    /// Attach plan-owned expression metadata for direct pipeline execution.
    ///
    /// Top-level statement execution passes this metadata directly from the
    /// owning [`crate::ExecutionPlan`]. Embedding and test-harness code that
    /// runs `execute_pipeline(&plan.pipeline, ...)` directly should attach the
    /// same side tables so planned expression subqueries can be evaluated.
    #[must_use]
    pub fn with_plan_metadata(
        mut self,
        expr_ids: &'a ExprIdLookup,
        subqueries: &'a SubqueryRegistry,
    ) -> Self {
        self.plan_expr_ids = Some(expr_ids);
        self.plan_subqueries = Some(subqueries);
        self
    }

    /// Borrow the graph snapshot used by this context.
    ///
    /// Write contexts return the transaction-local working graph so later
    /// reads in the same statement see earlier writes.
    #[must_use]
    pub fn snapshot(&self) -> &SeleneGraph {
        if let Some(txn) = self.write_txn.as_deref() {
            return txn.read();
        }
        self.snapshot.as_ref()
    }

    /// Borrow the statement mutator.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError::InvalidTransactionState`] when a mutation
    /// operator executes in a read-only context.
    pub fn mutator(&mut self) -> Result<Mutator<'_, 'g>, ExecutorError> {
        self.mutator_with_span(
            "mutation invoked without write transaction",
            SourceSpan::default(),
        )
    }

    /// Borrow the statement mutator with a caller-specific diagnostic.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError::InvalidTransactionState`] when the current
    /// context is read-only.
    pub fn mutator_with_span(
        &mut self,
        detail: &'static str,
        span: SourceSpan,
    ) -> Result<Mutator<'_, 'g>, ExecutorError> {
        self.write_txn
            .as_deref_mut()
            .map(WriteTxn::mutator)
            .ok_or(ExecutorError::InvalidTransactionState { detail, span })
    }

    /// Confirm a write transaction is attached without borrowing its mutator.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError::InvalidTransactionState`] when the current
    /// context is read-only.
    pub fn ensure_write_txn(
        &self,
        detail: &'static str,
        span: SourceSpan,
    ) -> Result<(), ExecutorError> {
        if self.write_txn.is_some() {
            Ok(())
        } else {
            Err(ExecutorError::InvalidTransactionState { detail, span })
        }
    }

    /// Borrow the shared graph attached to a maintenance statement context.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutorError::InvalidTransactionState`] when the current
    /// context is not a top-level maintenance statement context.
    pub fn maintenance_graph_with_span(
        &self,
        detail: &'static str,
        span: SourceSpan,
    ) -> Result<&'g SharedGraph, ExecutorError> {
        self.maintenance_graph
            .ok_or(ExecutorError::InvalidTransactionState { detail, span })
    }

    /// Borrow the planner/executor implementation-defined caps.
    #[must_use]
    pub const fn impl_defined_caps(&self) -> &'a ImplDefinedCaps {
        self.impl_defined_caps
    }

    /// Borrow the procedure registry for this statement.
    #[must_use]
    pub const fn registry(&self) -> &'a dyn ProcedureRegistry {
        self.registry
    }

    /// Borrow the fixed index-provider registry visible to this statement.
    #[must_use]
    pub const fn providers(&self) -> &'a [Arc<dyn IndexProvider>] {
        self.providers
    }

    /// Borrow the session-local query parameters visible to this statement.
    #[must_use]
    pub fn parameters(&self) -> &BTreeMap<DbString, Value> {
        self.parameters.as_ref()
    }

    /// Clone the request-owned binding-table registry handle.
    #[must_use]
    pub(crate) fn binding_table_registry(&self) -> Arc<BindingTableRegistry> {
        self.request_runtime.binding_tables()
    }

    /// Register a binding table in this statement's request-scoped registry.
    pub fn register_binding_table(
        &self,
        table: Arc<BindingTable>,
    ) -> Result<BindingTableId, ExecutorError> {
        self.request_runtime
            .binding_tables()
            .register(table)
            .map_err(ExecutorError::from)
    }

    /// Look up a binding table from this statement's request-scoped registry.
    pub fn binding_table_for(
        &self,
        id: BindingTableId,
    ) -> Result<Arc<BindingTable>, BindingTableLookupError> {
        self.request_runtime.binding_tables().resolve(id)
    }

    /// Borrow the adaptive optimizer hook, when one was supplied.
    #[must_use]
    pub const fn reopt_hook(&self) -> Option<&dyn AdaptiveOptimizer> {
        self.reopt_hook
    }

    pub(crate) const fn plan_metadata(&self) -> Option<(&'a ExprIdLookup, &'a SubqueryRegistry)> {
        match (self.plan_expr_ids, self.plan_subqueries) {
            (Some(expr_ids), Some(subqueries)) => Some((expr_ids, subqueries)),
            _ => None,
        }
    }
}

impl fmt::Debug for TxContext<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TxContext")
            .field("snapshot", &self.snapshot)
            .field("impl_defined_caps", self.impl_defined_caps)
            .field("providers", &self.providers.len())
            .field("parameters", &self.parameters.len())
            .field("reopt_hook", &self.reopt_hook.is_some())
            .field("plan_expr_ids", &self.plan_expr_ids.is_some())
            .field("plan_subqueries", &self.plan_subqueries.is_some())
            .field("cancellation", &self.cancellation.is_some())
            .field("deadline", &self.deadline.is_some())
            .field("node_scan_budget", &self.node_scan_budget.is_some())
            .field("row_cap", &self.row_cap)
            .field("result_rows_emitted", &self.result_rows_emitted.get())
            .field("write_txn", &self.write_txn.is_some())
            .field("maintenance_graph", &self.maintenance_graph.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use selene_core::GraphId;

    use crate::{EmptyProcedureRegistry, ImplDefinedCaps};

    use super::*;

    fn read_only_ctx<'a>(
        graph: &'a Arc<SeleneGraph>,
        caps: &'a ImplDefinedCaps,
        registry: &'a EmptyProcedureRegistry,
    ) -> TxContext<'a, 'a> {
        TxContext::read_only(graph.clone(), caps, registry, &[])
    }

    #[test]
    fn request_timestamp_is_stable_within_context() {
        let graph = Arc::new(SeleneGraph::new(GraphId::new(8_808)));
        let caps = ImplDefinedCaps::default();
        let registry = EmptyProcedureRegistry;
        let ctx = read_only_ctx(&graph, &caps, &registry);

        let first = ctx.request_timestamp_zoned();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = ctx.request_timestamp_zoned();

        assert_eq!(first, second);
    }

    #[test]
    fn stride_accumulates_then_resets_and_only_checks_at_boundary() {
        // GQLRT-29: the in-loop stride accumulator must NOT call the (cancelled)
        // token until the accumulated row count reaches CANCEL_CHECK_STRIDE. A
        // cancelled token is attached up front, so any premature check would
        // error immediately.
        let graph = Arc::new(SeleneGraph::new(GraphId::new(8_801)));
        let caps = ImplDefinedCaps::default();
        let registry = EmptyProcedureRegistry;
        let token = CancellationToken::new();
        token.cancel();
        let ctx = read_only_ctx(&graph, &caps, &registry).with_resource_limits(
            Some(&token),
            None,
            None,
            None,
        );

        let mut rows_since_check = 0usize;
        // Accumulate one short of the stride: no boundary crossed, no check.
        for _ in 0..(CANCEL_CHECK_STRIDE - 1) {
            ctx.check_cancellation_stride(&mut rows_since_check, 1)
                .expect("below the stride boundary the cancelled token is never consulted");
        }
        assert_eq!(rows_since_check, CANCEL_CHECK_STRIDE - 1);

        // The row that crosses the boundary triggers the check, which observes
        // the cancelled token and surfaces 5GQL2.
        let err = ctx
            .check_cancellation_stride(&mut rows_since_check, 1)
            .expect_err("crossing the stride boundary consults the cancelled token");
        assert!(matches!(err, ExecutorError::Cancelled { .. }));
        assert_eq!(err.gqlstatus().as_str(), "5GQL2");
    }

    #[test]
    fn stride_resets_counter_after_an_uncancelled_boundary_check() {
        // With no cancellation attached, crossing the boundary still resets the
        // accumulator to 0 so the next stride window starts fresh (the
        // accumulate-then-reset contract).
        let graph = Arc::new(SeleneGraph::new(GraphId::new(8_802)));
        let caps = ImplDefinedCaps::default();
        let registry = EmptyProcedureRegistry;
        let ctx = read_only_ctx(&graph, &caps, &registry);

        let mut rows_since_check = 0usize;
        // A single batch large enough to cross the boundary resets to 0.
        ctx.check_cancellation_stride(&mut rows_since_check, CANCEL_CHECK_STRIDE + 5)
            .expect("no cancellation token attached");
        assert_eq!(
            rows_since_check, 0,
            "the accumulator must reset to 0 once a boundary check fires"
        );

        // A sub-stride batch after the reset does not trigger another check and
        // accumulates from 0.
        ctx.check_cancellation_stride(&mut rows_since_check, 3)
            .expect("below boundary");
        assert_eq!(rows_since_check, 3);
    }
}
