use std::time::Instant;

use selene_core::metrics;

use crate::{
    PlannedCall, ProcedureError, ProcedureMutability, ProcedureTier,
    runtime::{
        ExecutorError, GraphContext, MaintenanceContext, MutationContext, ProcedureContext,
        TxContext,
    },
};

pub(super) fn validate_call_tier(call: &PlannedCall) -> Result<(), ExecutorError> {
    let expected = tier_for_mutability(call.mutability);
    if call.tier != expected {
        return Err(procedure_error(
            ProcedureError::TierMismatch {
                expected,
                actual: call.tier,
            },
            call.span,
            None,
        ));
    }
    Ok(())
}

pub(crate) fn build<'borrow, 'ctx, 'g>(
    call: &PlannedCall,
    ctx: &'borrow mut TxContext<'ctx, 'g>,
) -> Result<ProcedureContext<'borrow, 'g>, ExecutorError>
where
    'ctx: 'borrow,
{
    match call.tier {
        ProcedureTier::Graph => Ok(ProcedureContext::Graph(GraphContext::new(
            ctx.snapshot(),
            ctx.impl_defined_caps(),
            ctx.providers(),
            ctx.cancellation_checker(),
            ctx.binding_table_registry(),
        ))),
        ProcedureTier::Mutation => {
            let caps = ctx.impl_defined_caps();
            let cancellation = ctx.cancellation_checker();
            let binding_tables = ctx.binding_table_registry();
            let mutator = ctx.mutator_with_span(
                "mutation-tier procedure requires a write transaction",
                call.span,
            )?;
            Ok(ProcedureContext::Mutation(MutationContext::new(
                mutator,
                caps,
                cancellation,
                binding_tables,
            )))
        }
        ProcedureTier::Maintenance => {
            let graph = ctx.maintenance_graph_with_span(
                "maintenance-tier procedure requires a maintenance statement context",
                call.span,
            )?;
            Ok(ProcedureContext::Maintenance(MaintenanceContext::new(
                graph,
                ctx.impl_defined_caps(),
                ctx.cancellation_checker(),
                ctx.binding_table_registry(),
            )))
        }
    }
}

pub(crate) fn build_read_only<'borrow, 'ctx, 'g>(
    call: &PlannedCall,
    ctx: &'borrow TxContext<'ctx, 'g>,
) -> Result<ProcedureContext<'borrow, 'g>, ExecutorError>
where
    'ctx: 'borrow,
{
    match call.tier {
        ProcedureTier::Graph => Ok(ProcedureContext::Graph(GraphContext::new(
            ctx.snapshot(),
            ctx.impl_defined_caps(),
            ctx.providers(),
            ctx.cancellation_checker(),
            ctx.binding_table_registry(),
        ))),
        ProcedureTier::Mutation => Err(ExecutorError::InvalidTransactionState {
            detail: "mutation-tier procedure requires a write transaction",
            span: call.span,
        }),
        ProcedureTier::Maintenance => {
            let graph = ctx.maintenance_graph_with_span(
                "maintenance-tier procedure requires a maintenance statement context",
                call.span,
            )?;
            Ok(ProcedureContext::Maintenance(MaintenanceContext::new(
                graph,
                ctx.impl_defined_caps(),
                ctx.cancellation_checker(),
                ctx.binding_table_registry(),
            )))
        }
    }
}

pub(super) const fn tier_for_mutability(mutability: ProcedureMutability) -> ProcedureTier {
    match mutability {
        ProcedureMutability::Read => ProcedureTier::Graph,
        ProcedureMutability::SchemaWrite => ProcedureTier::Mutation,
        ProcedureMutability::MaintenanceWrite => ProcedureTier::Maintenance,
    }
}

pub(crate) fn procedure_error(
    source: ProcedureError,
    span: crate::SourceSpan,
    deadline: Option<Instant>,
) -> ExecutorError {
    match source {
        ProcedureError::Cancelled => {
            metrics::counter_inc(metrics::CANCELLATIONS_TOTAL);
            ExecutorError::Cancelled { span }
        }
        ProcedureError::Timeout { elapsed } => {
            metrics::counter_inc(metrics::CANCELLATIONS_TOTAL);
            ExecutorError::Timeout {
                deadline: deadline.unwrap_or_else(Instant::now),
                elapsed,
                span,
            }
        }
        ProcedureError::NodeScanBudgetExceeded { .. } => ExecutorError::ProgramLimitExceeded {
            detail: "node scan budget exceeded",
            span,
        },
        ProcedureError::ProgramLimitExceeded { detail } => {
            ExecutorError::ProgramLimitExceeded { detail, span }
        }
        source => ExecutorError::Procedure { source, span },
    }
}
