//! Category-specific statement execution arms.
//!
//! Split out of `statement.rs` so the source-entry logic (caches, policies,
//! facade interception) and the per-category execution arms stay under the
//! file cap. Every function here is called from [`super::statement`] only.

use std::sync::Arc;

use selene_core::{CancellationToken, NodeScanBudget};
use selene_graph::{CommitOutcome, SeleneGraph};

use super::{request, session::materialize_parameter_values};
use crate::{
    ExecutionPlan, GqlStatus, ProcedureRegistry, SourceSpan,
    runtime::{
        BindingTable, ExecutorError, ExecutorWarning, GqlStatusObject, RequestExecutionInput,
        Session, StatementOutput, TxContext, WriteOutcome, batch::control::PhysicalControl,
        execute_plan, request_runtime::RequestRuntime,
    },
};

pub(super) fn execute_read_only(
    plan: &ExecutionPlan,
    session: &mut Session<'_>,
    registry: &dyn ProcedureRegistry,
    request_runtime: &Arc<RequestRuntime>,
) -> Result<StatementOutput, ExecutorError> {
    let providers = session.graph().index_providers();
    let snapshot = session.graph().read();
    if let Some(txn) = session.active_txn.as_ref() {
        validate_request_references(session.request.as_ref(), txn.read())?;
    } else {
        validate_request_references(session.request.as_ref(), snapshot.as_ref())?;
    }
    let session_tz = session.effective_time_zone();
    let request_timestamp = session.effective_request_timestamp();
    let binding_tables = request_runtime.binding_tables();
    let parameters = materialize_parameter_values(
        &session.parameters,
        &session.scalar_parameters,
        &binding_tables,
    )?;
    let (cancellation, deadline, row_cap, node_scan_budget) = resource_limits(session);
    let warning_sink = session.warning_sink.as_ref();
    let table = if let Some(txn) = session.active_txn.as_mut() {
        let mut ctx = TxContext::write_with_owned_parameters_and_runtime(
            snapshot,
            &plan.impl_defined_caps,
            registry,
            txn,
            providers,
            parameters,
            Arc::clone(request_runtime),
            request_timestamp,
        )
        .with_resource_limits(
            cancellation.as_ref(),
            deadline,
            row_cap,
            node_scan_budget.as_ref(),
        )
        .with_warning_sink(warning_sink)
        .with_session_time_zone(session_tz);
        ctx.check_cancellation()?;
        let table = execute_plan(plan, &mut ctx)?;
        note_output_rows(plan, &ctx, table.row_count())?;
        table
    } else {
        let mut ctx = TxContext::read_only_with_owned_parameters_and_runtime(
            snapshot,
            &plan.impl_defined_caps,
            registry,
            providers,
            parameters,
            Arc::clone(request_runtime),
            request_timestamp,
        )
        .with_resource_limits(
            cancellation.as_ref(),
            deadline,
            row_cap,
            node_scan_budget.as_ref(),
        )
        .with_warning_sink(warning_sink)
        .with_session_time_zone(session_tz);
        ctx.check_cancellation()?;
        let table = execute_plan(plan, &mut ctx)?;
        note_output_rows(plan, &ctx, table.row_count())?;
        table
    };
    Ok(output_from_table(plan, table))
}

pub(super) fn execute_write(
    plan: &ExecutionPlan,
    session: &mut Session<'_>,
    registry: &dyn ProcedureRegistry,
    request_runtime: &Arc<RequestRuntime>,
) -> Result<StatementOutput, ExecutorError> {
    if session.active_txn.is_some() {
        return execute_inside_explicit_tx(plan, session, registry, request_runtime);
    }
    execute_auto_commit(plan, session, registry, request_runtime)
}

pub(super) fn execute_maintenance(
    plan: &ExecutionPlan,
    session: &mut Session<'_>,
    registry: &dyn ProcedureRegistry,
    request_runtime: &Arc<RequestRuntime>,
) -> Result<StatementOutput, ExecutorError> {
    if session.active_txn.is_some() {
        return Err(ExecutorError::InvalidTransactionState {
            detail: "maintenance procedure cannot run inside an explicit transaction",
            span: SourceSpan::default(),
        });
    }
    let providers = session.graph().index_providers();
    let snapshot = session.graph().read();
    validate_request_references(session.request.as_ref(), snapshot.as_ref())?;
    let session_tz = session.effective_time_zone();
    let request_timestamp = session.effective_request_timestamp();
    let binding_tables = request_runtime.binding_tables();
    let parameters = materialize_parameter_values(
        &session.parameters,
        &session.scalar_parameters,
        &binding_tables,
    )?;
    let (cancellation, deadline, row_cap, node_scan_budget) = resource_limits(session);
    let warning_sink = session.warning_sink.as_ref();
    let mut ctx = TxContext::maintenance_with_owned_parameters_and_runtime(
        snapshot,
        &plan.impl_defined_caps,
        registry,
        session.graph(),
        providers,
        parameters,
        Arc::clone(request_runtime),
        request_timestamp,
    )
    .with_resource_limits(
        cancellation.as_ref(),
        deadline,
        row_cap,
        node_scan_budget.as_ref(),
    )
    .with_warning_sink(warning_sink)
    .with_session_time_zone(session_tz);
    ctx.check_cancellation()?;
    let table = execute_plan(plan, &mut ctx)?;
    note_output_rows(plan, &ctx, table.row_count())?;
    Ok(output_from_table(plan, table))
}

fn execute_inside_explicit_tx(
    plan: &ExecutionPlan,
    session: &mut Session<'_>,
    registry: &dyn ProcedureRegistry,
    request_runtime: &Arc<RequestRuntime>,
) -> Result<StatementOutput, ExecutorError> {
    if let Some(request) = session.request.as_ref() {
        let txn = session
            .active_txn
            .as_ref()
            .ok_or(ExecutorError::ImplementationDefined {
                detail: "explicit-TX path entered without active transaction",
            })?;
        if let Err(error) = request::validate_references(request, txn.read()) {
            session.aborted = true;
            return Err(error);
        }
    }
    let providers = session.graph().index_providers();
    let snapshot = session.graph().read();
    let session_tz = session.effective_time_zone();
    let request_timestamp = session.effective_request_timestamp();
    let binding_tables = request_runtime.binding_tables();
    let parameters = materialize_parameter_values(
        &session.parameters,
        &session.scalar_parameters,
        &binding_tables,
    )?;
    let (cancellation, deadline, row_cap, node_scan_budget) = resource_limits(session);
    let warning_sink = session.warning_sink.as_ref();
    let txn = session
        .active_txn
        .as_mut()
        .ok_or(ExecutorError::ImplementationDefined {
            detail: "explicit-TX path entered without active transaction",
        })?;
    let mut ctx = TxContext::write_with_owned_parameters_and_runtime(
        snapshot,
        &plan.impl_defined_caps,
        registry,
        txn,
        providers,
        parameters,
        Arc::clone(request_runtime),
        request_timestamp,
    )
    .with_resource_limits(
        cancellation.as_ref(),
        deadline,
        row_cap,
        node_scan_budget.as_ref(),
    )
    .with_warning_sink(warning_sink)
    .with_session_time_zone(session_tz);
    let result = ctx
        .check_cancellation()
        .and_then(|()| execute_plan(plan, &mut ctx))
        .and_then(|table| {
            note_output_rows(plan, &ctx, table.row_count())?;
            Ok(table)
        });
    if result.is_err() {
        session.aborted = true;
    }
    result.map(|table| output_from_table(plan, table))
}

fn execute_auto_commit(
    plan: &ExecutionPlan,
    session: &mut Session<'_>,
    registry: &dyn ProcedureRegistry,
    request_runtime: &Arc<RequestRuntime>,
) -> Result<StatementOutput, ExecutorError> {
    let providers = session.graph().index_providers();
    let snapshot = session.graph().read();
    let principal = session.principal();
    let session_tz = session.effective_time_zone();
    let request_timestamp = session.effective_request_timestamp();
    let binding_tables = request_runtime.binding_tables();
    let parameters = materialize_parameter_values(
        &session.parameters,
        &session.scalar_parameters,
        &binding_tables,
    )?;
    let mut txn = session.graph().begin_write();
    let reference_result = validate_request_references(session.request.as_ref(), txn.read());
    let (cancellation, deadline, row_cap, node_scan_budget) = resource_limits(session);
    let warning_sink = session.warning_sink.as_ref();
    let result = reference_result.and_then(|()| {
        let mut ctx = TxContext::write_with_owned_parameters_and_runtime(
            snapshot,
            &plan.impl_defined_caps,
            registry,
            &mut txn,
            providers,
            parameters,
            Arc::clone(request_runtime),
            request_timestamp,
        )
        .with_resource_limits(
            cancellation.as_ref(),
            deadline,
            row_cap,
            node_scan_budget.as_ref(),
        )
        .with_warning_sink(warning_sink)
        .with_session_time_zone(session_tz);
        ctx.check_cancellation()
            .and_then(|()| execute_plan(plan, &mut ctx))
            .and_then(|table| {
                note_output_rows(plan, &ctx, table.row_count())?;
                Ok(table)
            })
    });
    match result {
        Ok(table) => {
            let (outcome, prepared) = if session.prepare_unpublished {
                let prepared = txn.prepare_unpublished(principal, None).map_err(|source| {
                    ExecutorError::GraphMutation {
                        source,
                        span: SourceSpan::default(),
                    }
                })?;
                (prepared.outcome(), Some(prepared))
            } else {
                let outcome = txn.commit_with_principal(principal).map_err(|source| {
                    ExecutorError::GraphMutation {
                        source,
                        span: SourceSpan::default(),
                    }
                })?;
                (outcome, None)
            };
            emit_commit_warnings(&outcome, session, request_runtime);
            session.prepared_graph = prepared;
            Ok(write_output_from_commit(plan, table, outcome))
        }
        Err(error) => {
            txn.rollback();
            Err(error)
        }
    }
}

fn validate_request_references(
    request_input: Option<&RequestExecutionInput>,
    graph: &SeleneGraph,
) -> Result<(), ExecutorError> {
    request_input.map_or(Ok(()), |request| {
        request::validate_references(request, graph)
    })
}

fn emit_commit_warnings(
    outcome: &CommitOutcome,
    session: &Session<'_>,
    request_runtime: &RequestRuntime,
) {
    for warning in &outcome.warnings {
        let warning = ExecutorWarning {
            code: GqlStatus::VALIDATION_MODE_RELAXED_WRITE,
            message: warning.warning.violation.to_string(),
            span: SourceSpan::default(),
        };
        request_runtime.record_status(GqlStatusObject::new(warning.code, warning.message.clone()));
        if let Some(sink) = session.warning_sink.as_ref() {
            sink.borrow_mut().emit(warning);
        }
    }
}

fn note_output_rows(
    plan: &ExecutionPlan,
    ctx: &TxContext<'_, '_>,
    row_count: usize,
) -> Result<(), ExecutorError> {
    if !plan.output_schema.columns.is_empty() {
        ctx.note_result_rows(row_count)?;
    }
    Ok(())
}

fn resource_limits(
    session: &Session<'_>,
) -> (
    Option<CancellationToken>,
    Option<std::time::Instant>,
    Option<usize>,
    Option<NodeScanBudget>,
) {
    (
        session.cancellation.clone(),
        session.deadline,
        session.row_cap,
        session.max_nodes_scanned.map(NodeScanBudget::new),
    )
}

pub(super) fn execute_transaction_control(
    plan: &ExecutionPlan,
    session: &mut Session<'_>,
) -> Result<StatementOutput, ExecutorError> {
    let [crate::PipelineOp::Tx(op)] = plan.pipeline.as_slice() else {
        return Err(ExecutorError::ImplementationDefined {
            detail: "transaction-control plan must contain exactly one TX op",
        });
    };
    PhysicalControl::Transaction(op).execute(session, &crate::EmptyProcedureRegistry)
}

pub(super) fn execute_session_control(
    plan: &ExecutionPlan,
    session: &mut Session<'_>,
    registry: &dyn ProcedureRegistry,
) -> Result<StatementOutput, ExecutorError> {
    let [crate::PipelineOp::Session(op)] = plan.pipeline.as_slice() else {
        return Err(ExecutorError::ImplementationDefined {
            detail: "session-control plan must contain exactly one session op",
        });
    };
    PhysicalControl::Session(op).execute(session, registry)
}

fn output_from_table(plan: &ExecutionPlan, table: BindingTable) -> StatementOutput {
    if plan.output_schema.columns.is_empty() {
        StatementOutput::Empty
    } else {
        StatementOutput::Rows(table)
    }
}

fn write_output_from_commit(
    plan: &ExecutionPlan,
    table: BindingTable,
    outcome: CommitOutcome,
) -> StatementOutput {
    let rows = if plan.output_schema.columns.is_empty() {
        None
    } else {
        Some(table)
    };
    StatementOutput::Written(WriteOutcome::from_commit(outcome, rows))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        panic::{AssertUnwindSafe, catch_unwind},
    };

    use selene_core::{GraphId, NodeId, Value, db_string};
    use selene_graph::SharedGraph;

    use crate::{
        EmptyProcedureRegistry, GqlType,
        runtime::{RequestExecutionInput, RequestParameter, Session, StatementOutput},
    };

    fn seed_live_node(graph: &SharedGraph) {
        Session::new(graph)
            .execute_source("INSERT (:Live) FINISH", &EmptyProcedureRegistry)
            .unwrap();
    }

    fn node_request() -> RequestExecutionInput {
        RequestExecutionInput::new(
            BTreeMap::from([(
                db_string("value").unwrap(),
                RequestParameter::new(GqlType::NodeRef, Value::NodeRef(NodeId::new(1))),
            )]),
            jiff::Timestamp::new(1_788_692_096, 0).unwrap(),
            jiff::tz::TimeZone::UTC,
        )
    }

    fn integer_request() -> RequestExecutionInput {
        RequestExecutionInput::new(
            BTreeMap::from([(
                db_string("request_only").unwrap(),
                RequestParameter::new(GqlType::Integer, Value::Int(11)),
            )]),
            jiff::Timestamp::new(1_788_692_096, 0).unwrap(),
            jiff::tz::TimeZone::UTC,
        )
    }

    fn delete_live_node(graph: &SharedGraph) {
        Session::new(graph)
            .execute_source("MATCH (n:Live) DELETE n FINISH", &EmptyProcedureRegistry)
            .unwrap();
    }

    #[test]
    fn read_revalidates_references_against_its_exact_snapshot() {
        let graph = SharedGraph::new(GraphId::new(81_101));
        seed_live_node(&graph);
        let mut session = Session::new(&graph).with_before_statement_execution(|| {
            delete_live_node(&graph);
        });

        let error = session
            .execute_source_catalog_request(
                "RETURN $value.name",
                &EmptyProcedureRegistry,
                node_request(),
            )
            .unwrap_err();

        assert_eq!(error.gqlstatus().as_str(), "22G11");
        assert_eq!(graph.read().node_count(), 0);
    }

    #[test]
    fn auto_commit_reference_failure_rolls_back_without_publication() {
        let graph = SharedGraph::new(GraphId::new(81_102));
        seed_live_node(&graph);
        let before = graph.read();
        let generation = before.meta.generation;
        let next_node_id = before.meta.next_node_id;
        drop(before);
        let mut session = Session::new(&graph).with_before_statement_execution(|| {
            delete_live_node(&graph);
        });

        let error = session
            .execute_source_catalog_request(
                "INSERT (:ShouldNotPublish {name: $value.name}) FINISH",
                &EmptyProcedureRegistry,
                node_request(),
            )
            .unwrap_err();

        assert_eq!(error.gqlstatus().as_str(), "22G11");
        let after = graph.read();
        assert_eq!(after.meta.generation, generation + 1);
        assert_eq!(after.meta.next_node_id, next_node_id);
        assert_eq!(after.node_count(), 0);
    }

    #[test]
    fn explicit_transaction_reference_failure_aborts_before_mutation() {
        let graph = SharedGraph::new(GraphId::new(81_103));
        seed_live_node(&graph);
        let generation = graph.read().meta.generation;
        let mut session = Session::new(&graph);
        session.start_transaction().unwrap();
        session
            .execute_source("MATCH (n:Live) DELETE n FINISH", &EmptyProcedureRegistry)
            .unwrap();

        let error = session
            .execute_source_catalog_request(
                "INSERT (:ShouldNotPublish {name: $value.name}) FINISH",
                &EmptyProcedureRegistry,
                node_request(),
            )
            .unwrap_err();

        assert_eq!(error.gqlstatus().as_str(), "22G11");
        assert!(session.is_aborted());
        session.rollback_transaction().unwrap();
        let after = graph.read();
        assert_eq!(after.meta.generation, generation);
        assert_eq!(after.node_count(), 1);
    }

    #[test]
    fn request_fields_restore_before_panic_resumes() {
        let graph = SharedGraph::new(GraphId::new(81_104));
        let mut session = Session::new(&graph);
        session.bind_parameter(db_string("prior").unwrap(), Value::Int(7));
        session
            .execute_source("SESSION SET TIME ZONE '+03:00'", &EmptyProcedureRegistry)
            .unwrap();
        let mut session = session.with_before_statement_execution(|| {
            panic!("injected request execution panic");
        });

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = session.execute_source_catalog_request(
                "RETURN $request_only",
                &EmptyProcedureRegistry,
                integer_request(),
            );
        }));
        assert!(panic.is_err());

        let output = session
            .execute_source("RETURN $prior, CURRENT_TIMESTAMP", &EmptyProcedureRegistry)
            .unwrap();
        let StatementOutput::Rows(table) = output else {
            panic!("expected row output");
        };
        assert_eq!(table.rows()[0].values().first(), Some(&Value::Int(7)));
        let Some(Value::ZonedDateTime(current)) = table.rows()[0].values().get(1) else {
            panic!("expected zoned current timestamp");
        };
        assert_eq!(current.offset().seconds(), 3 * 3_600);
        let leaked = session
            .execute_source("RETURN $request_only", &EmptyProcedureRegistry)
            .unwrap_err();
        assert_eq!(leaked.gqlstatus().as_str(), "22G03");
    }
}
