//! The single physical execution route.
//!
//! Read segments are pull-based operator trees. Effectful operations drain the
//! preceding segment before borrowing the transaction mutably; the next segment
//! resolves candidates against the resulting working graph. There is no decline,
//! retry, value-dependent fallback, or alternate statement executor.

use super::{
    assembly,
    budget::MemoryBudget,
    operator::{BatchExecutionContext, PhysicalOperator},
    page::BatchPage,
    policy::BatchPolicy,
    tracer::trace_operator_to_table,
    tree::build_join_tree,
    unit::{BatchRowSource, BatchSeedRow},
};
use crate::{
    ExecutionPlan, PatternPlan, PipelineOp, SubqueryRegistry,
    analyze::ExprIdLookup,
    runtime::{
        Binding, BindingTable, EvalCtx, ExecutorError, TxContext, pattern, pipeline, plan_runner,
    },
};

/// Vary physical batch shape without adding a production execution option.
#[cfg(test)]
pub(crate) fn execute_with_test_policy(
    plan: &ExecutionPlan,
    ctx: &TxContext<'_, '_>,
    policy: BatchPolicy,
) -> Result<BindingTable, ExecutorError> {
    execute_read_only(plan, None, ctx, policy)
}

pub(crate) fn execute(
    plan: &ExecutionPlan,
    seed: Option<BindingTable>,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let policy = BatchPolicy::default_policy();
    let (table, consumed) = initial_segment(plan, seed, ctx, policy)?;
    execute_pipeline(
        &plan.pipeline[consumed..],
        table,
        ctx,
        &plan.expr_ids,
        &plan.subqueries,
        policy,
    )
}

pub(crate) fn execute_read_only(
    plan: &ExecutionPlan,
    seed: Option<BindingTable>,
    ctx: &TxContext<'_, '_>,
    policy: BatchPolicy,
) -> Result<BindingTable, ExecutorError> {
    if crate::plan::classify_plan(plan).rejects_in_read_only() {
        return Err(pipeline::read_only_write_op_error());
    }
    let (mut table, mut consumed) = initial_segment(plan, seed, ctx, policy)?;
    while consumed < plan.pipeline.len() {
        match &plan.pipeline[consumed] {
            PipelineOp::ExplainPlan { inner, .. } => table = pipeline::explain::execute(inner)?,
            _ => return Err(pipeline::read_only_write_op_error()),
        }
        consumed += 1;
        let (next, count) = read_segment(
            &plan.pipeline[consumed..],
            table,
            ctx,
            &plan.expr_ids,
            &plan.subqueries,
            policy,
        )?;
        table = next;
        consumed += count;
    }
    ctx.check_cancellation()?;
    Ok(table)
}

pub(crate) fn execute_seeded_subplan(
    plan: &ExecutionPlan,
    seed: BindingTable,
    ctx: &TxContext<'_, '_>,
    policy: BatchPolicy,
) -> Result<BindingTable, ExecutorError> {
    execute_read_only(plan, Some(seed), ctx, policy)
}

pub(crate) fn execute_pipeline(
    ops: &[PipelineOp],
    mut table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
    policy: BatchPolicy,
) -> Result<BindingTable, ExecutorError> {
    let mut index = 0;
    while index < ops.len() {
        match &ops[index] {
            PipelineOp::Mutation(op) => {
                table = super::mutation::PhysicalMutation::new(op, expr_ids, subqueries, policy)
                    .execute(table, ctx)?;
                index += 1;
            }
            PipelineOp::Catalog(op) => {
                table = super::catalog::execute(op, table, ctx)?;
                index += 1;
            }
            PipelineOp::Call(call)
                if call.tier != crate::ProcedureTier::Graph
                    || call.mutability != crate::ProcedureMutability::Read =>
            {
                table = super::write_call::execute(call, table, ctx, expr_ids, subqueries, policy)?;
                index += 1;
            }
            PipelineOp::ExplainPlan { inner, .. } => {
                table = pipeline::explain::execute(inner)?;
                index += 1;
            }
            PipelineOp::Union { rhs, .. }
            | PipelineOp::Chain(rhs)
            | PipelineOp::CorrelatedChain(rhs)
                if crate::plan::classify_plan(rhs).effect != crate::plan::LogicalEffect::Query =>
            {
                table = super::write_composition::execute(&ops[index], table, ctx)?;
                index += 1;
            }
            PipelineOp::Tx(_) => {
                return Err(ExecutorError::ImplementationDefined {
                    detail: "TX op surfaced inside execute_pipeline; should be dispatched at statement level",
                });
            }
            PipelineOp::Session(_) => {
                return Err(ExecutorError::ImplementationDefined {
                    detail: "session op surfaced inside execute_pipeline; should be dispatched at statement level",
                });
            }
            _ => {
                let (next, count) =
                    read_segment(&ops[index..], table, ctx, expr_ids, subqueries, policy)?;
                table = next;
                index += count;
            }
        }
        ctx.check_cancellation()?;
    }
    Ok(table)
}

fn initial_segment(
    plan: &ExecutionPlan,
    seed: Option<BindingTable>,
    ctx: &TxContext<'_, '_>,
    policy: BatchPolicy,
) -> Result<(BindingTable, usize), ExecutorError> {
    let eval = EvalCtx {
        tx: ctx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let root: Box<dyn PhysicalOperator + '_> = match (&plan.pattern_plan, seed) {
        (Some(pattern), Some(seed)) => {
            let (schema, rows) = seed.into_parts();
            let target = plan_runner::target_schema(&schema, pattern);
            match rows.into_iter().next() {
                Some(seed) => pattern_root(
                    pattern,
                    target,
                    Some(seed),
                    eval,
                    policy,
                    plan_runner::pattern_row_limit(plan),
                )?,
                None => Box::new(BatchSeedRow::empty_table(schema)),
            }
        }
        (Some(pattern), None) => pattern_root(
            pattern,
            pattern::schema_for_pattern(pattern),
            None,
            eval,
            policy,
            plan_runner::pattern_row_limit(plan),
        )?,
        (None, Some(seed)) => Box::new(BatchRowSource::new(seed, policy)),
        (None, None) => Box::new(BatchSeedRow::unit()),
    };
    let (root, count) = assembly::build(root, &plan.pipeline, eval, policy)?;
    Ok((execute_tree(root, ctx)?, count))
}

fn read_segment(
    ops: &[PipelineOp],
    table: BindingTable,
    ctx: &TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
    policy: BatchPolicy,
) -> Result<(BindingTable, usize), ExecutorError> {
    if ops.is_empty() {
        return Ok((table, 0));
    }
    let eval = EvalCtx {
        tx: ctx,
        expr_ids,
        subqueries,
    };
    let (root, count) = assembly::build(
        Box::new(BatchRowSource::new(table, policy)),
        ops,
        eval,
        policy,
    )?;
    if count == 0 {
        return Err(ExecutorError::ImplementationDefined {
            detail: "physical read segment made no progress",
        });
    }
    Ok((execute_tree(root, ctx)?, count))
}

pub(crate) fn execute_pattern(
    pattern: &PatternPlan,
    schema: crate::BindingTableSchema,
    seed: Option<Binding>,
    eval: EvalCtx<'_, '_, '_, '_>,
    policy: BatchPolicy,
    limit: Option<usize>,
) -> Result<BindingTable, ExecutorError> {
    let root = pattern_root(pattern, schema, seed, eval, policy, limit)?;
    execute_tree(root, eval.tx)
}

fn pattern_root<'e, 'a: 'e, 'ctx: 'e, 'g: 'e, 'p: 'e>(
    pattern: &'p PatternPlan,
    schema: crate::BindingTableSchema,
    seed: Option<Binding>,
    eval: EvalCtx<'a, 'ctx, 'g, 'p>,
    policy: BatchPolicy,
    limit: Option<usize>,
) -> Result<Box<dyn PhysicalOperator + 'e>, ExecutorError> {
    if limit == Some(0) {
        return Ok(Box::new(BatchSeedRow::empty_table(schema)));
    }
    let mut root = build_join_tree(&pattern.join_tree, pattern, schema, eval, policy, seed)?;
    for predicate in &pattern.filters {
        if predicate.index_consumed {
            continue;
        }
        root = Box::new(super::filter::BatchFilter::new(root, predicate, eval));
    }
    if let Some(bound) = limit {
        let bound = u64::try_from(bound).map_err(|_| ExecutorError::ImplementationDefined {
            detail: "batch pattern row limit exceeds the supported range",
        })?;
        root = Box::new(BatchPage::new(root, 0, bound));
    }
    Ok(root)
}

fn execute_tree(
    mut root: Box<dyn PhysicalOperator + '_>,
    ctx: &TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let mut exec = BatchExecutionContext::borrowed(
        ctx.snapshot(),
        ctx.batch_cancel(),
        MemoryBudget::unlimited(),
    );
    trace_operator_to_table(root.as_mut(), &mut exec)
}
