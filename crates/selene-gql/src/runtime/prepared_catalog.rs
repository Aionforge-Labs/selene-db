//! Single-parse selected-facade request preparation and staged execution.

mod analysis;

use std::{mem, panic::AssertUnwindSafe, sync::Arc};

use selene_graph::write_txn::PreparedGraphCommit;

use crate::{
    CatalogObjectReference, DatabaseCatalogCommand, ExecutionPlan, GqlType, ParameterUse,
    PipelineOp, ProcedureRegistry, SessionOp, SessionResetTarget, SessionSetGraphTarget, Statement,
    StatementCategory, TxOp, Value,
};

use super::{
    CatalogSessionOutput, ExecutionOutcome as RuntimeExecutionOutcome, ExecutorError,
    RequestExecutionInput, Session, SessionParameterValue,
    request_runtime::RequestRuntime,
    statement::{SourceExecutionPolicy, database_catalog_command, execute_source_plan},
};

/// One owned selected-facade plan plus its exact request input.
///
/// Parsing, analysis, request validation, planning, and optimization have
/// already completed. The facade can execute this plan under the live graph
/// lease for reads, or under its global mutation reservation on a scratch graph
/// for writes, without a second parse.
#[doc(hidden)]
pub struct PreparedCatalogRequest {
    source: Arc<str>,
    plan: Arc<ExecutionPlan>,
    parameter_uses: Arc<[ParameterUse]>,
    request: RequestExecutionInput,
    graph_id: selene_core::GraphId,
    graph_generation: u64,
    schema_version: u64,
    catalog: Option<Arc<crate::analyze::catalog::CatalogResolution>>,
}

/// Request-independent selected-facade plan suitable for facade-owned reuse.
///
/// The facade must validate its complete dependency stamp before binding a new
/// [`RequestExecutionInput`]. This object intentionally retains no request
/// parameter values, timestamp, cancellation token, or warning sink.
#[doc(hidden)]
#[derive(Clone)]
pub struct PreparedCatalogPlan {
    source: Arc<str>,
    plan: Arc<ExecutionPlan>,
    parameter_uses: Arc<[ParameterUse]>,
    graph_id: selene_core::GraphId,
    schema_version: u64,
    catalog: Option<Arc<crate::analyze::catalog::CatalogResolution>>,
}

/// Hidden selected-facade statement classification retained after one compile pass.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedCatalogRequestKind {
    /// Query or read-only procedure.
    ReadOnly,
    /// Graph-data modification.
    DataModifying,
    /// Graph-schema or database-catalog modification.
    CatalogModifying,
    /// Derived-state maintenance, which the detached facade rejects.
    Maintenance,
    /// Bare transaction demarcation handled by the facade state machine.
    TransactionControl(PreparedTransactionControl),
    /// Session control validated and applied by the persistent facade session.
    SessionControl,
}

/// Fully evaluated session control returned to the persistent facade.
///
/// Catalog references remain unresolved until the facade captures one catalog
/// read snapshot. No variant mutates the transient lower-engine session.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub enum PreparedSessionControl {
    /// An `IF NOT EXISTS` assignment whose target already exists.
    NoOp,
    /// Bind one evaluated scalar session parameter.
    SetValue {
        /// Database-string parameter name without the leading `$`.
        param: selene_core::DbString,
        /// Declared type, or `ANY` when the source omitted one.
        declared_type: GqlType,
        /// Evaluated parameter value.
        value: Value,
    },
    /// Replace the session time zone.
    SetTimeZone {
        /// Parsed IANA, POSIX, or fixed-offset time zone.
        zone: jiff::tz::TimeZone,
        /// Offset at the immutable request timestamp for facade diagnostics.
        displacement_seconds: i32,
    },
    /// Resolve and select a schema through the facade catalog.
    SetSchema(CatalogObjectReference),
    /// Resolve and select a graph through the facade catalog.
    SetGraph(SessionSetGraphTarget),
    /// Reset all session characteristics.
    ResetAllCharacteristics,
    /// Reset the current schema.
    ResetSchema,
    /// Reset the current graph.
    ResetGraph,
    /// Reset all session parameters.
    ResetParameters,
    /// Reset the session time zone.
    ResetTimeZone,
    /// Reset one named session parameter.
    ResetParameter(selene_core::DbString),
    /// Terminate the persistent facade session.
    Close,
}

/// Hidden bare transaction operation classified from the prepared plan.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedTransactionControl {
    /// `START TRANSACTION`.
    Start,
    /// `COMMIT`.
    Commit,
    /// `ROLLBACK`.
    Rollback,
}

/// Parse only enough source to recognize one bare transaction control.
///
/// This graph-independent path is reserved for facade sessions whose selected
/// context is already stale and therefore cannot prepare a normal request. A
/// caller either executes the returned control or rejects the non-control; it
/// must not parse the same source again.
#[doc(hidden)]
pub fn parse_transaction_control(
    source: &str,
) -> Result<Option<PreparedTransactionControl>, ExecutorError> {
    let statement = crate::parse(source).map_err(|source| ExecutorError::Parse { source })?;
    Ok(match statement {
        Statement::StartTransaction { .. } => Some(PreparedTransactionControl::Start),
        Statement::Commit { .. } => Some(PreparedTransactionControl::Commit),
        Statement::Rollback { .. } => Some(PreparedTransactionControl::Rollback),
        _ => None,
    })
}

/// Parse only enough source to recognize a bare `SESSION CLOSE`.
///
/// This graph-independent path lets the facade release transaction state and
/// terminate even when its formerly selected graph has been dropped.
#[doc(hidden)]
pub fn parse_session_close(source: &str) -> Result<bool, ExecutorError> {
    let statement = crate::parse(source).map_err(|source| ExecutorError::Parse { source })?;
    Ok(matches!(statement, Statement::SessionClose { .. }))
}

/// Parse one graph-independent facade recovery control.
///
/// The normal parser still performs generated-profile admission. Only close
/// and supported reset controls are transported without a selected graph; no
/// value expression or catalog reference is evaluated here.
#[doc(hidden)]
pub fn parse_graph_independent_session_control(
    source: &str,
) -> Result<Option<PreparedSessionControl>, ExecutorError> {
    let statement = crate::parse(source).map_err(|source| ExecutorError::Parse { source })?;
    Ok(match statement {
        Statement::SessionClose { .. } => Some(PreparedSessionControl::Close),
        Statement::SessionReset { target, .. } => Some(match target {
            SessionResetTarget::AllCharacteristics => {
                PreparedSessionControl::ResetAllCharacteristics
            }
            SessionResetTarget::Schema => PreparedSessionControl::ResetSchema,
            SessionResetTarget::Graph => PreparedSessionControl::ResetGraph,
            SessionResetTarget::Parameters => PreparedSessionControl::ResetParameters,
            SessionResetTarget::TimeZone => PreparedSessionControl::ResetTimeZone,
            SessionResetTarget::Parameter(name) => PreparedSessionControl::ResetParameter(name),
        }),
        _ => None,
    })
}

impl PreparedCatalogRequest {
    /// Return the selected-facade classification from the already analyzed plan.
    #[must_use]
    pub fn kind(&self) -> PreparedCatalogRequestKind {
        match self.plan.category {
            StatementCategory::ReadOnly => PreparedCatalogRequestKind::ReadOnly,
            StatementCategory::DataModifying => PreparedCatalogRequestKind::DataModifying,
            StatementCategory::CatalogModifying => PreparedCatalogRequestKind::CatalogModifying,
            StatementCategory::Maintenance => PreparedCatalogRequestKind::Maintenance,
            StatementCategory::TransactionControl => {
                PreparedCatalogRequestKind::TransactionControl(
                    match self.plan.pipeline.as_slice() {
                        [PipelineOp::Tx(TxOp::Start { .. })] => PreparedTransactionControl::Start,
                        [PipelineOp::Tx(TxOp::Commit { .. })] => PreparedTransactionControl::Commit,
                        [PipelineOp::Tx(TxOp::Rollback { .. })] => {
                            PreparedTransactionControl::Rollback
                        }
                        _ => unreachable!(
                            "transaction-control plans contain one transaction operation"
                        ),
                    },
                )
            }
            StatementCategory::SessionControl => PreparedCatalogRequestKind::SessionControl,
        }
    }

    /// Return a request-independent plan for facade-owned dependency-stamped reuse.
    #[must_use]
    pub fn cached_plan(&self) -> PreparedCatalogPlan {
        PreparedCatalogPlan {
            source: Arc::clone(&self.source),
            plan: Arc::clone(&self.plan),
            parameter_uses: Arc::clone(&self.parameter_uses),
            graph_id: self.graph_id,
            schema_version: self.schema_version,
            catalog: self.catalog.clone(),
        }
    }

    /// Return the target and guard for a session-value assignment.
    #[must_use]
    pub fn session_set_value_target(&self) -> Option<(&selene_core::DbString, bool)> {
        match self.plan.pipeline.as_slice() {
            [
                PipelineOp::Session(SessionOp::SetValue {
                    param,
                    if_not_exists,
                    ..
                }),
            ] => Some((param, *if_not_exists)),
            _ => None,
        }
    }

    /// Return whether this is exactly one database-catalog command.
    #[must_use]
    pub fn is_database_catalog(&self) -> bool {
        database_catalog_command(&self.plan).is_some()
    }

    /// Borrow the intercepted database-catalog command, when present.
    #[must_use]
    pub fn database_catalog_command(&self) -> Option<&DatabaseCatalogCommand> {
        database_catalog_command(&self.plan)
    }

    /// Return whether the prepared statement is read-only.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.plan.category == StatementCategory::ReadOnly
    }

    /// Return the graph identity against which this request was planned.
    #[must_use]
    pub const fn graph_id(&self) -> selene_core::GraphId {
        self.graph_id
    }

    /// Return the graph generation against which this request was planned.
    #[must_use]
    pub const fn graph_generation(&self) -> u64 {
        self.graph_generation
    }

    /// Return the graph schema epoch against which this request was planned.
    #[must_use]
    pub const fn schema_version(&self) -> u64 {
        self.schema_version
    }
}

impl PreparedCatalogPlan {
    /// Validate and bind fresh per-request input without retaining prior values.
    ///
    /// # Errors
    ///
    /// Returns the same parameter-contract or reference diagnostic as a fresh
    /// parse/analyze request preflight.
    pub fn bind(
        &self,
        request: RequestExecutionInput,
        graph: &selene_graph::SeleneGraph,
    ) -> Result<PreparedCatalogRequest, ExecutorError> {
        if graph.graph_id() != self.graph_id {
            return Err(ExecutorError::ImplementationDefined {
                detail: "cached plan belongs to another graph",
            });
        }
        super::request::validate(&request, &self.parameter_uses, graph)?;
        Ok(PreparedCatalogRequest {
            source: Arc::clone(&self.source),
            plan: Arc::clone(&self.plan),
            parameter_uses: Arc::clone(&self.parameter_uses),
            request,
            graph_id: self.graph_id,
            graph_generation: graph.meta.generation,
            schema_version: self.schema_version,
            catalog: self.catalog.clone(),
        })
    }
}

/// Existing request outcome paired with an unpublished graph commit.
#[doc(hidden)]
pub struct PreparedCatalogMutationOutput {
    output: RuntimeExecutionOutcome,
    graph: PreparedGraphCommit,
}

impl PreparedCatalogMutationOutput {
    /// Consume the staging result into the existing outcome and graph bundle.
    #[must_use]
    pub fn into_parts(self) -> (RuntimeExecutionOutcome, PreparedGraphCommit) {
        (self.output, self.graph)
    }
}

impl<'g> Session<'g> {
    /// Compile one selected-facade request without executing its plan.
    #[doc(hidden)]
    pub fn prepare_source_catalog_request(
        &mut self,
        source: &str,
        registry: &dyn ProcedureRegistry,
        request: RequestExecutionInput,
    ) -> Result<PreparedCatalogRequest, ExecutorError> {
        let snapshot = self.graph().read();
        let graph_id = snapshot.graph_id();
        let graph_generation = snapshot.meta.generation;
        drop(snapshot);
        let schema_version = self.graph().schema_version();
        let (result, request, _, prepared_graph) =
            self.with_facade_request(request, false, |session| {
                session.execute_source_with_policy(
                    source,
                    registry,
                    SourceExecutionPolicy::PrepareCatalogSession,
                )
            });
        debug_assert!(prepared_graph.is_none());
        let CatalogSessionOutput::Prepared {
            plan,
            parameter_uses,
        } = result?
        else {
            return Err(ExecutorError::ImplementationDefined {
                detail: "selected request preparation did not return an owned plan",
            });
        };
        Ok(PreparedCatalogRequest {
            source: Arc::from(source),
            plan,
            parameter_uses,
            request,
            graph_id,
            graph_generation,
            schema_version,
            catalog: None,
        })
    }

    /// Recompile a stale prepared request against this session's graph.
    #[doc(hidden)]
    pub fn reprepare_source_catalog_request(
        &mut self,
        prepared: PreparedCatalogRequest,
        registry: &dyn ProcedureRegistry,
    ) -> Result<PreparedCatalogRequest, ExecutorError> {
        self.prepare_source_catalog_request(&prepared.source, registry, prepared.request)
    }

    /// Evaluate and validate one selected session control without mutating the
    /// transient lower-engine session.
    #[doc(hidden)]
    pub fn resolve_prepared_session_control(
        &mut self,
        prepared: PreparedCatalogRequest,
        registry: &dyn ProcedureRegistry,
        skip_if_exists: bool,
    ) -> Result<PreparedSessionControl, ExecutorError> {
        let [PipelineOp::Session(op)] = prepared.plan.pipeline.as_slice() else {
            return Err(ExecutorError::ImplementationDefined {
                detail: "session-control plan did not contain exactly one session operation",
            });
        };
        let (result, _, _, prepared_graph) =
            self.with_facade_request(prepared.request, false, |session| {
                super::pipeline::session::prepare(op, session, registry, skip_if_exists)
            });
        debug_assert!(prepared_graph.is_none());
        result
    }

    /// Execute a prepared request without enabling unpublished writes.
    #[doc(hidden)]
    pub fn execute_prepared_catalog_request(
        &mut self,
        prepared: PreparedCatalogRequest,
        registry: &dyn ProcedureRegistry,
    ) -> Result<CatalogSessionOutput, ExecutorError> {
        let (result, _, runtime, prepared_graph) =
            self.with_facade_request(prepared.request, false, |session| {
                execute_source_plan(
                    &prepared.plan,
                    session,
                    registry,
                    SourceExecutionPolicy::CatalogSession,
                )
            });
        debug_assert!(prepared_graph.is_none());
        result.map(|output| request_output(output, runtime))
    }

    /// Execute a prepared mutation and return its unpublished graph snapshot.
    #[doc(hidden)]
    pub fn execute_prepared_catalog_request_unpublished(
        &mut self,
        prepared: PreparedCatalogRequest,
        registry: &dyn ProcedureRegistry,
    ) -> Result<PreparedCatalogMutationOutput, ExecutorError> {
        if prepared.is_database_catalog()
            || !matches!(
                prepared.plan.category,
                StatementCategory::DataModifying | StatementCategory::CatalogModifying
            )
        {
            return Err(ExecutorError::ImplementationDefined {
                detail: "unpublished execution requires an engine graph mutation plan",
            });
        }
        let (result, _, runtime, prepared_graph) =
            self.with_facade_request(prepared.request, true, |session| {
                execute_source_plan(
                    &prepared.plan,
                    session,
                    registry,
                    SourceExecutionPolicy::CatalogSession,
                )
            });
        let output = request_output(result?, runtime);
        let CatalogSessionOutput::RequestOutcome(output) = output else {
            return Err(ExecutorError::ImplementationDefined {
                detail: "unpublished mutation did not return a request outcome",
            });
        };
        let graph = prepared_graph.ok_or(ExecutorError::ImplementationDefined {
            detail: "unpublished mutation did not prepare a graph commit",
        })?;
        Ok(PreparedCatalogMutationOutput { output, graph })
    }

    pub(super) fn with_facade_request<T>(
        &mut self,
        request: RequestExecutionInput,
        prepare_unpublished: bool,
        execute: impl FnOnce(&mut Self) -> Result<T, ExecutorError>,
    ) -> (
        Result<T, ExecutorError>,
        RequestExecutionInput,
        Arc<RequestRuntime>,
        Option<PreparedGraphCommit>,
    ) {
        let request_runtime = request.runtime();
        let scalar_parameters: std::collections::BTreeMap<_, _> = request
            .parameters
            .iter()
            .map(|(name, parameter)| (name.clone(), parameter.value().clone()))
            .collect();
        let parameters = scalar_parameters
            .iter()
            .map(|(name, value)| (name.clone(), SessionParameterValue::Scalar(value.clone())))
            .collect();
        let prior_parameters = mem::replace(&mut self.parameters, parameters);
        let prior_scalar_parameters = mem::replace(&mut self.scalar_parameters, scalar_parameters);
        let prior_time_zone = self.time_zone.replace(request.time_zone.clone());
        let prior_request = self.request.replace(request);
        let prior_prepare = mem::replace(&mut self.prepare_unpublished, prepare_unpublished);
        let prior_prepared = self.prepared_graph.take();

        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| execute(self)));

        self.parameters = prior_parameters;
        self.scalar_parameters = prior_scalar_parameters;
        self.time_zone = prior_time_zone;
        let request = self
            .request
            .take()
            .expect("facade request remains installed during execution");
        self.request = prior_request;
        self.prepare_unpublished = prior_prepare;
        let prepared = self.prepared_graph.take();
        self.prepared_graph = prior_prepared;

        match outcome {
            Ok(result) => (result, request, request_runtime, prepared),
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

fn request_output(
    output: CatalogSessionOutput,
    runtime: Arc<RequestRuntime>,
) -> CatalogSessionOutput {
    match output {
        CatalogSessionOutput::Statement(output) => CatalogSessionOutput::RequestOutcome(
            RuntimeExecutionOutcome::from_statement(output, runtime.statuses()),
        ),
        output => output,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use selene_core::GraphId;
    use selene_graph::SharedGraph;

    use super::*;
    use crate::{EmptyProcedureRegistry, GqlStatus};

    fn request() -> RequestExecutionInput {
        RequestExecutionInput::new(
            BTreeMap::new(),
            jiff::Timestamp::new(1_788_692_096, 0).unwrap(),
            jiff::tz::TimeZone::UTC,
        )
    }

    #[test]
    fn control_only_parser_classifies_bare_controls_without_a_graph() {
        for (source, expected) in [
            ("START TRANSACTION", PreparedTransactionControl::Start),
            (" COMMIT ", PreparedTransactionControl::Commit),
            ("ROLLBACK", PreparedTransactionControl::Rollback),
        ] {
            assert_eq!(parse_transaction_control(source).unwrap(), Some(expected));
        }
        assert_eq!(parse_transaction_control("RETURN 1").unwrap(), None);
        assert!(matches!(
            parse_transaction_control("RETURN (").unwrap_err(),
            ExecutorError::Parse { .. }
        ));
        assert!(parse_session_close(" SESSION CLOSE ").unwrap());
        assert!(!parse_session_close("RETURN 1").unwrap());
        for source in [
            "SESSION RESET",
            "SESSION RESET SCHEMA",
            "SESSION RESET GRAPH",
        ] {
            assert!(
                parse_graph_independent_session_control(source)
                    .unwrap()
                    .is_some(),
                "{source}"
            );
        }
        assert!(
            parse_graph_independent_session_control("SESSION SET TIME ZONE '+01:00'")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn prepared_mutation_executes_without_publishing_scratch_graph() {
        let graph = SharedGraph::new(GraphId::new(61));
        let registry = EmptyProcedureRegistry;
        let mut session = Session::with_principal(&graph, Arc::from([4_u8, 5]));
        let prepared = session
            .prepare_source_catalog_request("INSERT (:Prepared) RETURN 1", &registry, request())
            .unwrap();
        assert_eq!(prepared.graph_id(), GraphId::new(61));
        assert_eq!(prepared.graph_generation(), 0);

        let staged = session
            .execute_prepared_catalog_request_unpublished(prepared, &registry)
            .unwrap();
        let (output, prepared_graph) = staged.into_parts();

        assert_eq!(graph.read().node_count(), 0);
        assert_eq!(graph.read().meta.generation, 0);
        assert_eq!(prepared_graph.snapshot().node_count(), 1);
        assert_eq!(prepared_graph.snapshot().meta.generation, 1);
        assert_eq!(
            prepared_graph.outcome().principal.as_deref(),
            Some(&[4, 5][..])
        );
        assert!(matches!(output, RuntimeExecutionOutcome::Written { .. }));
    }

    #[test]
    fn selected_maintenance_is_classified_then_rejected_before_execution() {
        let graph = SharedGraph::new(GraphId::new(62));
        let registry = crate::BuiltinProcedureRegistry::new();
        let mut session = Session::new(&graph);

        let prepared = session
            .prepare_source_catalog_request("CALL selene.compact()", &registry, request())
            .expect("selected maintenance is classified without execution");
        assert_eq!(prepared.kind(), PreparedCatalogRequestKind::Maintenance);
        let error = session
            .execute_prepared_catalog_request(prepared, &registry)
            .expect_err("selected maintenance is rejected before execution");

        assert_eq!(error.gqlstatus(), GqlStatus::FEATURE_NOT_SUPPORTED);
        assert_eq!(graph.read().meta.generation, 0);
    }
}
