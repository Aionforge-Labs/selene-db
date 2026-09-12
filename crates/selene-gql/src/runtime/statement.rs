//! Top-level statement executor.

use std::{sync::Arc, time::Instant};

use selene_core::{Change, metrics};
use selene_graph::CommitOutcome;
use selene_profile::current_profile_identity;

use super::call_plan_cache::CallPlanSourceLookup;
use super::plan_cache::{SharedPlanCacheInsert, SharedPlanCacheLookup};
use super::request_runtime::RequestRuntime;
use super::statement_exec::{
    execute_maintenance, execute_read_only, execute_session_control, execute_transaction_control,
    execute_write,
};
use crate::{
    CatalogOp, DatabaseCatalogCommand, ExecutionPlan, LiveIndexCatalog, OptimizeContext,
    ParameterUse, PipelineOp, ProcedureRegistry, SourceSpan, StatementCategory, TxOp,
    analyze::analyze,
    ast::Statement,
    optimize,
    parser::parse,
    plan::plan_with_caps as build_plan,
    runtime::{
        BindingTable, CallPlanKey, DiagnosticBundle, ExecutionFrame,
        ExecutionOutcome as RuntimeExecutionOutcome, ExecutorError, GqlStatusObject,
        RequestExecutionInput, Session, request,
    },
};

/// Result of a selected catalog session's single parse/analyze/plan pass.
///
/// A database-catalog statement is returned as its command before any
/// execution context or write transaction is created, so the facade can
/// dispatch it to the catalog service outside the graph request lease it
/// borrowed the lower session under. Every other statement is executed
/// normally.
#[derive(Clone, Debug)]
#[doc(hidden)]
#[non_exhaustive]
pub enum CatalogSessionOutput {
    /// The statement was executed by the lower engine.
    Statement(StatementOutput),
    /// A facade request completed with rows, declared types, and statuses.
    RequestOutcome(RuntimeExecutionOutcome),
    /// The statement is a database-catalog command for the facade to execute.
    DatabaseCatalog(DatabaseCatalogCommand),
    /// A selected-facade request was compiled but deliberately not executed.
    #[doc(hidden)]
    Prepared {
        /// Optimized request-independent execution plan.
        plan: Arc<ExecutionPlan>,
        /// Analyzer-derived parameter-use contract for future request binds.
        parameter_uses: Arc<[ParameterUse]>,
    },
}

impl PartialEq for CatalogSessionOutput {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Statement(left), Self::Statement(right)) => left == right,
            (Self::RequestOutcome(left), Self::RequestOutcome(right)) => left == right,
            (Self::DatabaseCatalog(left), Self::DatabaseCatalog(right)) => left == right,
            (Self::Prepared { plan: left, .. }, Self::Prepared { plan: right, .. }) => {
                Arc::ptr_eq(left, right)
            }
            _ => false,
        }
    }
}

/// Result returned by statement-level execution.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum StatementOutput {
    /// Statement completed without a row-bearing result.
    Empty,
    /// Statement published a graph generation.
    Written(WriteOutcome),
    /// Statement produced a binding table.
    Rows(BindingTable),
}

/// Metadata returned for a statement that committed graph changes.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct WriteOutcome {
    /// Optional row result for write statements with `RETURN`.
    pub rows: Option<BindingTable>,
    /// Changes produced by the committed write transaction.
    pub changes: Vec<Change>,
    /// Published graph generation.
    pub generation: u64,
    /// Next node ID after the commit.
    pub next_node_id: u64,
    /// Next edge ID after the commit.
    pub next_edge_id: u64,
    /// Durable-sequence slot; always `None` from the in-graph publisher.
    /// Sequence assignment below the graph layer is the owning database
    /// handle's authority.
    pub durable_at: Option<u64>,
}

impl WriteOutcome {
    pub(crate) fn from_commit(outcome: CommitOutcome, rows: Option<BindingTable>) -> Self {
        Self {
            rows,
            changes: outcome.changes,
            generation: outcome.generation,
            next_node_id: outcome.next_node_id,
            next_edge_id: outcome.next_edge_id,
            durable_at: outcome.durable_at,
        }
    }
}

/// Execute one planned statement against a caller-owned session.
///
/// The procedure registry argument is optional because statement kinds without
/// CALL should not force embedders to construct a registry.
#[tracing::instrument(
    name = "selene.gql.execute_statement",
    skip(plan, session, registry),
    fields(category = ?plan.category)
)]
pub fn execute_statement(
    plan: &ExecutionPlan,
    session: &mut Session<'_>,
    registry: &dyn ProcedureRegistry,
) -> Result<StatementOutput, ExecutorError> {
    let request_runtime = session.execution_runtime();
    let mut execution_stack = request_runtime.stack();
    let mut execution_frame = execution_stack.push_child();
    // ISO/IEC 39075:2024 section 7.3: once `SESSION CLOSE` sets the termination
    // flag, every subsequent GQL-request is rejected regardless of category.
    // `execute_source` guards the source-string entry; this guard covers the
    // sibling public path where an embedder caches an `ExecutionPlan` and
    // re-executes it directly (the pipeline documented in the embedding guide),
    // so the termination flag is enforced at the single statement funnel
    // (hard rule 11) rather than only one layer up. The flag is set *during*
    // execution of `SESSION CLOSE` itself, so this check never blocks the
    // closing statement — only the requests that follow it.
    let started = Instant::now();
    let counts_toward_tx =
        plan.category != StatementCategory::TransactionControl && session.active_txn.is_some();
    let result = (|| {
        if session.is_closed() {
            return Err(ExecutorError::SessionClosed {
                span: SourceSpan::default(),
            });
        }
        if session.aborted && plan.category != StatementCategory::TransactionControl {
            return Err(ExecutorError::InFailedTransaction {
                span: SourceSpan::default(),
            });
        }
        match plan.category {
            StatementCategory::ReadOnly => {
                execute_read_only(plan, session, registry, &request_runtime)
            }
            StatementCategory::Maintenance => {
                execute_maintenance(plan, session, registry, &request_runtime)
            }
            StatementCategory::DataModifying | StatementCategory::CatalogModifying => {
                execute_write(plan, session, registry, &request_runtime)
            }
            StatementCategory::TransactionControl => execute_transaction_control(plan, session),
            StatementCategory::SessionControl => execute_session_control(plan, session, registry),
        }
    })();
    if counts_toward_tx {
        if result.is_ok() {
            session.tx_statement_count = session.tx_statement_count.saturating_add(1);
        } else {
            session.aborted = true;
        }
    }
    record_statement_metrics(plan, started);
    finish_execution_frame(&mut execution_frame, &request_runtime, plan, result)
}

fn finish_execution_frame(
    frame: &mut ExecutionFrame<'_>,
    runtime: &RequestRuntime,
    plan: &ExecutionPlan,
    result: Result<StatementOutput, ExecutorError>,
) -> Result<StatementOutput, ExecutorError> {
    match result {
        Ok(mut output) => {
            match &mut output {
                StatementOutput::Rows(table) => table.declare_result_order(plan),
                StatementOutput::Written(write) => {
                    if let Some(table) = &mut write.rows {
                        table.declare_result_order(plan);
                    }
                }
                StatementOutput::Empty => {}
            }
            let table = match &output {
                StatementOutput::Rows(table) => Some(table),
                StatementOutput::Written(write) => write.rows.as_ref(),
                StatementOutput::Empty => None,
            };
            if let Some(table) = table {
                frame
                    .context_mut()
                    .replace_table(Arc::new(table.clone()))
                    .map_err(|_| ExecutorError::ImplementationDefined {
                        detail: "statement output overlaps the execution working record",
                    })?;
            }
            frame
                .context_mut()
                .replace_outcome(RuntimeExecutionOutcome::from_statement(
                    output.clone(),
                    runtime.statuses(),
                ));
            Ok(output)
        }
        Err(error) => {
            let primary = GqlStatusObject::new(error.gqlstatus(), error.to_string());
            frame
                .context_mut()
                .replace_outcome(RuntimeExecutionOutcome::failed(DiagnosticBundle::new(
                    primary,
                    runtime.statuses(),
                )));
            Err(error)
        }
    }
}

impl Session<'_> {
    /// Parse, plan, and execute one source-string statement through this session.
    ///
    /// When the session has a plan cache, source strings whose cached plan was
    /// prepared against the current graph schema version skip parse, analyze,
    /// and plan. Short-lived sessions can additionally use the opt-in shared
    /// non-CALL source-plan cache, keyed by graph ID, schema version, registry
    /// version, profile identity, source text, caps, and optimizer mode.
    /// Procedure-call-rooted
    /// statements use the separate opt-in shared CALL plan cache, keyed by
    /// graph ID, schema version, registry version, profile identity, caps,
    /// optimizer mode, and formatter-canonical source. Embedded in-pipeline
    /// `CALL` remains uncached.
    /// If an active explicit transaction has uncommitted schema changes, both
    /// caches bypass lookup and insert so analysis sees transaction-local
    /// schema.
    pub fn execute_source(
        &mut self,
        source: &str,
        registry: &dyn ProcedureRegistry,
    ) -> Result<StatementOutput, ExecutorError> {
        self.execute_source_with_policy(source, registry, SourceExecutionPolicy::Stateful)
            .and_then(expect_executed)
    }

    /// Execute one source statement for a selected database-catalog session.
    ///
    /// Transaction and `SESSION` controls are rejected after the single
    /// parse/analyze/plan pass. A plan consisting of one database-catalog
    /// operation is returned as [`CatalogSessionOutput::DatabaseCatalog`]
    /// before any execution context or write transaction exists. Every other
    /// statement executes without a second parse.
    #[doc(hidden)]
    pub fn execute_source_catalog_session(
        &mut self,
        source: &str,
        registry: &dyn ProcedureRegistry,
    ) -> Result<CatalogSessionOutput, ExecutorError> {
        self.execute_source_with_policy(source, registry, SourceExecutionPolicy::CatalogSession)
    }

    /// Execute one facade request with an explicit parameter and temporal snapshot.
    #[doc(hidden)]
    pub fn execute_source_catalog_request(
        &mut self,
        source: &str,
        registry: &dyn ProcedureRegistry,
        request: RequestExecutionInput,
    ) -> Result<CatalogSessionOutput, ExecutorError> {
        let (result, _, request_runtime, _) = self.with_facade_request(request, false, |session| {
            session.execute_source_with_policy(
                source,
                registry,
                SourceExecutionPolicy::CatalogSession,
            )
        });
        result.map(|output| match output {
            CatalogSessionOutput::Statement(output) => CatalogSessionOutput::RequestOutcome(
                RuntimeExecutionOutcome::from_statement(output, request_runtime.statuses()),
            ),
            output => output,
        })
    }

    pub(super) fn execute_source_with_policy(
        &mut self,
        source: &str,
        registry: &dyn ProcedureRegistry,
        policy: SourceExecutionPolicy,
    ) -> Result<CatalogSessionOutput, ExecutorError> {
        let profile_identity = current_profile_identity();
        let explicit_request = self.request.is_some();
        // ISO/IEC 39075:2024 section 6 GR3 + section 7.3: once a session is
        // closed by `SESSION CLOSE`, every subsequent GQL-request is rejected.
        // This is the spec request boundary; embedders that drop the `Session`
        // struct still work, but the termination flag is the conformant model.
        if self.is_closed() {
            return Err(ExecutorError::SessionClosed {
                span: SourceSpan::default(),
            });
        }
        let schema_version = self.graph().schema_version();
        let registry_version = registry.registry_version();
        let top_level_call_candidate = is_top_level_call_candidate(source);
        let active_txn_has_schema_changes = self
            .active_txn
            .as_ref()
            .is_some_and(|txn| txn.has_schema_changes());
        if !explicit_request
            && !active_txn_has_schema_changes
            && let Some(cached) = self
                .plan_cache
                .as_mut()
                .and_then(|cache| cache.get(source, schema_version, profile_identity))
        {
            return execute_source_plan(&cached, self, registry, policy);
        }

        let shared_plan_cache = (!explicit_request && !active_txn_has_schema_changes)
            .then(|| self.shared_plan_cache.as_ref().map(Arc::clone))
            .flatten();
        let call_plan_cache = (!explicit_request && !active_txn_has_schema_changes)
            .then(|| self.call_plan_cache.as_ref().map(Arc::clone))
            .flatten();
        let cache_graph_id = if shared_plan_cache.is_some() || call_plan_cache.is_some() {
            Some(self.graph().read().graph_id())
        } else {
            None
        };
        if !top_level_call_candidate
            && let (Some(cache), Some(graph_id)) = (shared_plan_cache.as_ref(), cache_graph_id)
            && let Some(cached) = cache.get(SharedPlanCacheLookup {
                graph_id,
                schema_version,
                registry_version,
                profile_identity,
                source,
                caps: self.caps,
                index_selection: self.index_selection,
            })
        {
            if let Some(cache) = self.plan_cache.as_mut() {
                cache.insert(
                    Arc::from(source),
                    Arc::clone(&cached),
                    schema_version,
                    profile_identity,
                );
            }
            return execute_source_plan(&cached, self, registry, policy);
        }
        if top_level_call_candidate
            && let (Some(cache), Some(graph_id)) = (call_plan_cache.as_ref(), cache_graph_id)
            && let Some(cached) = cache.get_source(CallPlanSourceLookup {
                graph_id,
                schema_version,
                registry_version,
                profile_identity,
                caps: self.caps,
                index_selection: self.index_selection,
                source,
            })
        {
            metrics::counter_inc(metrics::CALL_PLAN_CACHE_HITS_TOTAL);
            return execute_source_plan(&cached, self, registry, policy);
        }

        // Codex PR #127 auto-review P2 #2 + P2 #3: compile failures inside an
        // explicit transaction must abort the transaction (PostgreSQL-style
        // contract: any error → rollback). Without this, parse/analyze/plan
        // errors would leave the explicit-tx in a usable state, diverging
        // from `execute_inside_explicit_tx` which marks `aborted = true` on
        // execution-time errors.
        let statement = parse(source).map_err(|source| {
            if self.active_txn.is_some() {
                self.aborted = true;
            }
            ExecutorError::Parse { source }
        })?;

        // Codex PR #127 auto-review P2 #1: short-circuit aborted-session
        // non-control statements before analyze/plan/cache-insert work.
        // TX-control statements (START/COMMIT/ROLLBACK) must remain reachable
        // on an aborted session so the caller can recover via ROLLBACK.
        if self.aborted && !is_tx_control_statement(&statement) {
            return Err(ExecutorError::InFailedTransaction {
                span: SourceSpan::default(),
            });
        }

        let call_plan_key = cache_graph_id.and_then(|graph_id| {
            CallPlanKey::for_statement(
                graph_id,
                schema_version,
                registry_version,
                profile_identity,
                self.caps,
                self.index_selection,
                &statement,
            )
        });
        if let (Some(cache), Some(key)) = (call_plan_cache.as_ref(), call_plan_key.as_ref())
            && let Some(cached) = cache.get(key)
        {
            metrics::counter_inc(metrics::CALL_PLAN_CACHE_HITS_TOTAL);
            return execute_source_plan(&cached, self, registry, policy);
        }

        let graph_type = self
            .active_txn
            .as_ref()
            .and_then(|txn| txn.read().meta.bound_type.as_ref().map(Arc::clone))
            .or_else(|| self.graph().graph_type());
        let analyzed = analyze(statement, registry, graph_type.as_deref()).map_err(|source| {
            if self.active_txn.is_some() {
                self.aborted = true;
            }
            ExecutorError::Analysis { source }
        })?;
        if let Some(request) = &self.request {
            let validation = if let Some(txn) = self.active_txn.as_ref() {
                request::validate(request, &analyzed.parameters, txn.read())
            } else {
                let snapshot = self.graph().read();
                request::validate(request, &analyzed.parameters, &snapshot)
            };
            if let Err(error) = validation {
                if self.active_txn.is_some() {
                    self.aborted = true;
                }
                return Err(error);
            }
        }
        let lowered = build_plan(&analyzed, registry, &self.caps).map_err(|source| {
            if self.active_txn.is_some() {
                self.aborted = true;
            }
            ExecutorError::Plan { source }
        })?;
        // Optimize on the cache-MISS path only: cached plans are already
        // optimized, so the two cache-hit early-returns above serve optimized
        // plans at hit-cost. EXPLAIN renders the optimized inner plan for free
        // because the optimizer recurses into PipelineOp::ExplainPlan { inner }.
        let plan = Arc::new(self.optimize_plan(lowered));
        ensure_source_policy(&plan, policy)?;
        let source_arc = Arc::<str>::from(source);
        if !explicit_request
            && !active_txn_has_schema_changes
            && let Some(cache) = self.plan_cache.as_mut()
        {
            cache.insert(
                Arc::clone(&source_arc),
                Arc::clone(&plan),
                schema_version,
                profile_identity,
            );
        }
        if let (Some(cache), Some(graph_id)) = (shared_plan_cache, cache_graph_id) {
            cache.insert(
                SharedPlanCacheInsert {
                    graph_id,
                    schema_version,
                    registry_version,
                    profile_identity,
                    source: Arc::clone(&source_arc),
                    caps: self.caps,
                    index_selection: self.index_selection,
                },
                Arc::clone(&plan),
            );
        }
        if let (Some(cache), Some(key)) = (call_plan_cache, call_plan_key) {
            cache.insert_with_source(key, source_arc, Arc::clone(&plan));
        }
        if policy == SourceExecutionPolicy::PrepareCatalogSession {
            return Ok(CatalogSessionOutput::Prepared {
                plan,
                parameter_uses: analyzed.parameters.clone().into(),
            });
        }
        execute_source_plan(&plan, self, registry, policy)
    }

    /// Run the default optimizer over a freshly-lowered plan.
    ///
    /// When [`index_selection`](Session::without_index_selection) is enabled
    /// (the default), the optimizer probes a snapshot-pinned
    /// [`LiveIndexCatalog`] so label / typed / composite index access paths are
    /// selected; Linear remains the always-correct fallback inside every rule.
    /// When disabled, the lowered (Linear) plan is returned unchanged — the
    /// optimizer fixed-point is skipped entirely, giving byte-identical Linear
    /// lowering and EXPLAIN to pre-optimizer-wiring HEAD.
    ///
    /// The catalog is built from a single pinned `Arc<SeleneGraph>` snapshot —
    /// the active transaction's working snapshot when inside an explicit
    /// transaction (so txn-local index DDL is visible), else the published
    /// snapshot. The plan-cache key is the `schema_version` epoch, which bumps
    /// only on schema-changing commits and is published after the new snapshot
    /// (see `selene_graph::WriteTxn::commit`); index *selection* depends only
    /// on which indexes exist, so a structural access path stays correct for
    /// any data mutation within an epoch.
    pub(super) fn optimize_plan(&self, lowered: ExecutionPlan) -> ExecutionPlan {
        if !self.index_selection {
            return lowered;
        }
        let snapshot = match self.active_txn.as_ref() {
            Some(txn) => Arc::new(txn.read().clone()),
            None => self.graph().read(),
        };
        let catalog = LiveIndexCatalog::new(snapshot);
        let caps = lowered.impl_defined_caps;
        let ctx = OptimizeContext::new(&caps).with_index_catalog(&catalog);
        optimize(lowered, &ctx)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum SourceExecutionPolicy {
    Stateful,
    /// Selected database-catalog session interception; see
    /// [`Session::execute_source_catalog_session`].
    CatalogSession,
    /// Compile one selected request without executing any plan operation.
    PrepareCatalogSession,
}

/// Unwrap the executed output for the policies that never intercept.
fn expect_executed(output: CatalogSessionOutput) -> Result<StatementOutput, ExecutorError> {
    match output {
        CatalogSessionOutput::Statement(output) => Ok(output),
        CatalogSessionOutput::RequestOutcome(_)
        | CatalogSessionOutput::DatabaseCatalog(_)
        | CatalogSessionOutput::Prepared { .. } => Err(ExecutorError::ImplementationDefined {
            detail: "database-catalog interception is only enabled for the catalog-session policy",
        }),
    }
}

pub(super) fn execute_source_plan(
    plan: &ExecutionPlan,
    session: &mut Session<'_>,
    registry: &dyn ProcedureRegistry,
    policy: SourceExecutionPolicy,
) -> Result<CatalogSessionOutput, ExecutorError> {
    ensure_source_policy(plan, policy)?;
    if policy == SourceExecutionPolicy::PrepareCatalogSession {
        return Err(ExecutorError::ImplementationDefined {
            detail: "prepared request plan is missing its analyzed parameter-use contract",
        });
    }
    if policy == SourceExecutionPolicy::CatalogSession
        && let Some(command) = database_catalog_command(plan)
    {
        return Ok(CatalogSessionOutput::DatabaseCatalog(command.clone()));
    }
    #[cfg(test)]
    session.run_before_statement_execution();
    execute_statement(plan, session, registry).map(CatalogSessionOutput::Statement)
}

/// Return the database-catalog command when the plan is exactly one such
/// operation. `EXPLAIN` wraps the operation and is executed normally.
pub(super) fn database_catalog_command(plan: &ExecutionPlan) -> Option<&DatabaseCatalogCommand> {
    match plan.pipeline.as_slice() {
        [PipelineOp::Catalog(CatalogOp::DatabaseCatalog(command))] => Some(command),
        _ => None,
    }
}

pub(super) fn ensure_source_policy(
    plan: &ExecutionPlan,
    policy: SourceExecutionPolicy,
) -> Result<(), ExecutorError> {
    if matches!(
        policy,
        SourceExecutionPolicy::CatalogSession | SourceExecutionPolicy::PrepareCatalogSession
    ) {
        let feature = match plan.category {
            StatementCategory::SessionControl
                if policy == SourceExecutionPolicy::CatalogSession =>
            {
                Some("session control in a selected facade session")
            }
            StatementCategory::TransactionControl
                if policy == SourceExecutionPolicy::CatalogSession =>
            {
                Some("transaction control delegated below the selected facade state machine")
            }
            StatementCategory::Maintenance if policy == SourceExecutionPolicy::CatalogSession => {
                Some("maintenance outside the selected facade detached-maintenance boundary")
            }
            StatementCategory::SessionControl
            | StatementCategory::TransactionControl
            | StatementCategory::Maintenance => None,
            StatementCategory::ReadOnly
            | StatementCategory::DataModifying
            | StatementCategory::CatalogModifying => None,
        };
        if let Some(feature) = feature {
            return Err(ExecutorError::FeatureNotSupportedYet {
                feature,
                span: SourceSpan::default(),
            });
        }
    }
    Ok(())
}

fn is_tx_control_statement(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::StartTransaction { .. } | Statement::Commit { .. } | Statement::Rollback { .. }
    )
}

fn is_top_level_call_candidate(source: &str) -> bool {
    let source = source.trim_start();
    let Some(prefix) = source.get(..4) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case("CALL") {
        return false;
    }
    source[4..].chars().next().is_none_or(char::is_whitespace)
}

fn record_statement_metrics(plan: &ExecutionPlan, started: Instant) {
    let label = metrics::Label::new(metrics::STATEMENT_KIND_LABEL, statement_kind(plan));
    metrics::counter_inc_with_label(metrics::QUERIES_TOTAL, label);
    metrics::histogram_record_with_label(
        metrics::QUERY_DURATION_SECONDS,
        started.elapsed().as_secs_f64(),
        label,
    );
}

fn statement_kind(plan: &ExecutionPlan) -> &'static str {
    if let Some(kind) = plan.pipeline.iter().find_map(pipeline_statement_kind) {
        return kind;
    }
    match plan.category {
        StatementCategory::ReadOnly => "query",
        StatementCategory::DataModifying => "mutation",
        StatementCategory::CatalogModifying => "catalog",
        StatementCategory::Maintenance => "maintenance",
        StatementCategory::TransactionControl => "transaction",
        StatementCategory::SessionControl => "session",
    }
}

fn pipeline_statement_kind(op: &PipelineOp) -> Option<&'static str> {
    match op {
        PipelineOp::Union { .. } | PipelineOp::Chain(_) | PipelineOp::CorrelatedChain(_) => {
            Some("composite")
        }
        PipelineOp::Match(_) | PipelineOp::OptionalMatch(_) => Some("query"),
        PipelineOp::Call(_) => Some("call"),
        PipelineOp::CallSubquery(_) => Some("call_subquery"),
        PipelineOp::Mutation(_) => Some("mutation"),
        PipelineOp::Catalog(_) => Some("catalog"),
        PipelineOp::ExplainPlan { .. } => Some("explain"),
        PipelineOp::Tx(TxOp::Start { .. }) => Some("start_transaction"),
        PipelineOp::Tx(TxOp::Commit { .. }) => Some("commit"),
        PipelineOp::Tx(TxOp::Rollback { .. }) => Some("rollback"),
        _ => None,
    }
}
