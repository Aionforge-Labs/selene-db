//! Binding-table pipeline executor.

mod aggregate;
mod call;
mod call_subquery;
mod catalog;
pub(crate) mod catalog_index;
mod chain;
mod distinct;
mod explain;
mod filter;
mod group_by;
mod let_op;
mod limit;
mod match_op;
mod mutation;
mod order_by;
mod project;
pub(crate) mod session;
mod top_k;
pub(crate) mod tx;
mod union;
mod unwind;

use crate::{
    PipelineOp, SubqueryRegistry,
    analyze::ExprIdLookup,
    runtime::{
        Binding, BindingTable, EvalCtx, ExecutorError, TxContext, plan_runner,
        value_key::RuntimeEqKey,
    },
};

pub(super) fn row_key(row: &Binding) -> RuntimeEqKey {
    RuntimeEqKey::from_row(row.values().to_vec())
}

/// Shared limit/offset resolution for the row and batch page operators.
pub(crate) use limit::resolve_amount;
/// Shared projection schema derivation for the row and batch projectors.
pub(crate) use project::schema_for_items;

/// Execute a sequence of pipeline operations against a binding table.
pub fn execute_pipeline(
    pipeline: &[PipelineOp],
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let expr_ids = ExprIdLookup::default();
    let subqueries = SubqueryRegistry::default();
    let (expr_ids, subqueries) = ctx.plan_metadata().unwrap_or((&expr_ids, &subqueries));
    execute_pipeline_with_plan(pipeline, table, ctx, expr_ids, subqueries)
}

/// Execute `pipeline` with read-write transaction access.
///
/// Thin entry point over the single [`dispatch_pipeline`] driver (GQLRT-10):
/// the read-write and read-only paths share one op-dispatch loop so a per-op
/// behavior change, a new read-safe [`PipelineOp`], or a cancellation-cadence
/// edit can no longer be applied to one path and silently forgotten in the
/// other.
pub(crate) fn execute_pipeline_with_plan(
    pipeline: &[PipelineOp],
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    dispatch_pipeline(
        pipeline,
        table,
        TxCtxRef::ReadWrite(ctx),
        expr_ids,
        subqueries,
    )
}

/// Execute `pipeline` with read-only transaction access.
///
/// Write-bearing ops ([`PipelineOp::Mutation`], [`PipelineOp::Catalog`],
/// [`PipelineOp::Tx`], [`PipelineOp::Session`]) are rejected with
/// [`ExecutorError::InvalidTransactionState`]; see [`dispatch_pipeline`].
pub(crate) fn execute_pipeline_read_only_with_plan(
    pipeline: &[PipelineOp],
    table: BindingTable,
    ctx: &TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    dispatch_pipeline(
        pipeline,
        table,
        TxCtxRef::ReadOnly(ctx),
        expr_ids,
        subqueries,
    )
}

/// Transaction-context capability handed to [`dispatch_pipeline`].
///
/// The read-write and read-only pipeline entry points differ only in whether
/// they hold `&mut TxContext` or `&TxContext`, which ops they may run, and
/// which per-op function each divergent op calls. Wrapping the two receivers in
/// one enum lets a single dispatch loop branch on the capability for the handful
/// of divergent ops while sharing every read op verbatim. Read ops borrow the
/// context immutably via [`TxCtxRef::shared`]; the write/divergent ops match the
/// enum to recover the exact receiver each path needs.
enum TxCtxRef<'r, 'a, 'g> {
    /// Read-write access: write ops execute against the mutable context.
    ReadWrite(&'r mut TxContext<'a, 'g>),
    /// Read-only access: write ops are rejected with
    /// [`ExecutorError::InvalidTransactionState`].
    ReadOnly(&'r TxContext<'a, 'g>),
}

impl<'a, 'g> TxCtxRef<'_, 'a, 'g> {
    /// Borrow the transaction context immutably (the shared access every read
    /// op needs and the cadence check uses).
    fn shared(&self) -> &TxContext<'a, 'g> {
        match self {
            Self::ReadWrite(ctx) => ctx,
            Self::ReadOnly(ctx) => ctx,
        }
    }

    /// `true` when write-bearing ops must be rejected.
    fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly(_))
    }

    /// Build the per-op [`EvalCtx`] for a read op, borrowing the context
    /// immutably.
    ///
    /// Constructed per read op rather than once per loop iteration: an
    /// `EvalCtx` holds `&TxContext`, so hoisting it across the loop body would
    /// keep that immutable borrow alive into the divergent write ops, which
    /// need `&mut TxContext`. Centralizing the *shape* here keeps the
    /// construction in one place while leaving the borrow correctly scoped to
    /// each read op.
    fn eval_ctx<'s, 'plan>(
        &'s self,
        expr_ids: &'plan ExprIdLookup,
        subqueries: &'plan SubqueryRegistry,
    ) -> EvalCtx<'s, 'a, 'g, 'plan> {
        EvalCtx {
            tx: self.shared(),
            expr_ids,
            subqueries,
        }
    }
}

/// The single per-row pipeline dispatch loop shared by the read-write and
/// read-only entry points (GQLRT-10).
///
/// Read ops are identical across capabilities and borrow the context immutably.
/// The divergent ops branch on [`TxCtxRef`]: `Union`/`Chain`/`Call`/
/// `CallSubquery` call their read-write or read-only sibling; `Mutation`/
/// `Catalog` execute under read-write and are rejected under read-only; `Tx`/
/// `Session` are statement-level ops that should never reach here, so under
/// read-write they surface [`ExecutorError::ImplementationDefined`] (preserving
/// their distinct diagnostics) and under read-only they are rejected like any
/// other write op. The exhaustive match (no wildcard) forces every new
/// [`PipelineOp`] variant to be classified here, in one place. Cancellation is
/// checked once per op at the loop tail for both capabilities.
fn dispatch_pipeline(
    pipeline: &[PipelineOp],
    mut table: BindingTable,
    mut ctx: TxCtxRef<'_, '_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    for op in pipeline {
        table = match op {
            PipelineOp::Filter(predicate) => {
                let eval_ctx = ctx.eval_ctx(expr_ids, subqueries);
                filter::execute(predicate, table, &eval_ctx)?
            }
            PipelineOp::Project(items) => {
                let eval_ctx = ctx.eval_ctx(expr_ids, subqueries);
                project::execute(items, table, &eval_ctx)?
            }
            PipelineOp::Let(items) => {
                let eval_ctx = ctx.eval_ctx(expr_ids, subqueries);
                let_op::execute(items, table, &eval_ctx)?
            }
            PipelineOp::Unwind {
                source,
                alias,
                position,
                span,
            } => {
                let eval_ctx = ctx.eval_ctx(expr_ids, subqueries);
                unwind::execute(
                    source,
                    alias.clone(),
                    position.clone(),
                    *span,
                    table,
                    &eval_ctx,
                )?
            }
            PipelineOp::TrimOrderCarriers { projected_width } => {
                order_by::trim_carriers(*projected_width, table)
            }
            PipelineOp::OrderBy(keys) => {
                let eval_ctx = ctx.eval_ctx(expr_ids, subqueries);
                order_by::execute(keys, table, &eval_ctx)?
            }
            PipelineOp::Limit { offset, count } => {
                limit::execute(offset, count, table, ctx.shared())?
            }
            PipelineOp::TopK {
                keys,
                offset,
                count,
            } => {
                let eval_ctx = ctx.eval_ctx(expr_ids, subqueries);
                top_k::execute(keys, offset, count, table, ctx.shared(), &eval_ctx)?
            }
            PipelineOp::GroupBy { keys, aggregates } => {
                let eval_ctx = ctx.eval_ctx(expr_ids, subqueries);
                group_by::execute(keys, aggregates, table, &eval_ctx)?
            }
            PipelineOp::Distinct => distinct::execute(table, ctx.shared())?,
            PipelineOp::Match(pattern) => {
                match_op::execute(pattern, table, ctx.shared(), expr_ids, subqueries)?
            }
            PipelineOp::OptionalMatch(pattern) => {
                match_op::execute_optional(pattern, table, ctx.shared(), expr_ids, subqueries)?
            }
            PipelineOp::ExplainPlan { inner, .. } => explain::execute(inner)?,
            PipelineOp::Union { op, rhs } => match &mut ctx {
                TxCtxRef::ReadWrite(ctx) => union::execute(*op, rhs, table, ctx)?,
                TxCtxRef::ReadOnly(ctx) => union::execute_read_only(*op, rhs, table, ctx)?,
            },
            PipelineOp::Chain(rhs) => match &mut ctx {
                TxCtxRef::ReadWrite(ctx) => chain::execute(rhs, table, ctx)?,
                TxCtxRef::ReadOnly(ctx) => plan_runner::execute_plan_read_only(rhs, ctx)?,
            },
            PipelineOp::CorrelatedChain(rhs) => match &mut ctx {
                TxCtxRef::ReadWrite(ctx) => chain::execute_correlated(rhs, table, ctx)?,
                TxCtxRef::ReadOnly(ctx) => chain::execute_correlated_read_only(rhs, table, ctx)?,
            },
            PipelineOp::Call(call) => match &mut ctx {
                TxCtxRef::ReadWrite(ctx) => call::execute(call, table, ctx, expr_ids, subqueries)?,
                TxCtxRef::ReadOnly(ctx) => {
                    call::execute_read_only(call, table, ctx, expr_ids, subqueries)?
                }
            },
            PipelineOp::CallSubquery(call) => match &mut ctx {
                TxCtxRef::ReadWrite(ctx) => call_subquery::execute(call, table, ctx)?,
                TxCtxRef::ReadOnly(ctx) => call_subquery::execute_read_only(call, table, ctx)?,
            },
            PipelineOp::Mutation(mutation) => match &mut ctx {
                TxCtxRef::ReadWrite(ctx) => {
                    mutation::execute(mutation, table, ctx, expr_ids, subqueries)?
                }
                TxCtxRef::ReadOnly(_) => return Err(read_only_write_op_error()),
            },
            PipelineOp::Catalog(catalog) => match &mut ctx {
                TxCtxRef::ReadWrite(ctx) => catalog::execute(catalog, table, ctx)?,
                TxCtxRef::ReadOnly(_) => return Err(read_only_write_op_error()),
            },
            PipelineOp::Tx(_) => {
                if ctx.is_read_only() {
                    return Err(read_only_write_op_error());
                }
                return Err(ExecutorError::ImplementationDefined {
                    detail: "TX op surfaced inside execute_pipeline; should be dispatched at statement level",
                });
            }
            PipelineOp::Session(_) => {
                if ctx.is_read_only() {
                    return Err(read_only_write_op_error());
                }
                return Err(ExecutorError::ImplementationDefined {
                    detail: "session op surfaced inside execute_pipeline; should be dispatched at statement level",
                });
            }
        };
        ctx.shared().check_cancellation()?;
    }
    Ok(table)
}

/// The error raised when a write-bearing pipeline op runs under read-only
/// access (a correlated subquery body). Centralized so every rejection site
/// reports the identical GQLSTATUS/diagnostic.
fn read_only_write_op_error() -> ExecutorError {
    ExecutorError::InvalidTransactionState {
        detail: "write pipeline op invoked from read-only subquery",
        span: crate::SourceSpan::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use selene_core::{GraphId, Value};

    use crate::{
        EmptyProcedureRegistry, ExecutionPlan, analyze, parse, plan,
        runtime::{ExecutorError, TxContext, plan_runner::execute_plan_read_only},
    };

    fn planned(source: &str) -> ExecutionPlan {
        let statement = parse(source).expect("test input parses");
        let analyzed =
            analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
        plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
    }

    fn read_only_ctx(plan: &ExecutionPlan) -> TxContext<'_, '_> {
        TxContext::read_only(
            Arc::new(selene_graph::SeleneGraph::new(GraphId::new(7710))),
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            &[],
        )
    }

    /// GQLRT-10: a `Mutation` op routed through the read-only dispatcher is
    /// rejected with the centralized read-only-write diagnostic — not executed.
    #[test]
    fn read_only_dispatch_rejects_mutation_op() {
        let plan = planned("INSERT (:Person)");
        let ctx = read_only_ctx(&plan);

        let err = execute_plan_read_only(&plan, &ctx)
            .expect_err("mutation op must be rejected on the read-only path");

        assert!(matches!(
            err,
            ExecutorError::InvalidTransactionState {
                detail: "write pipeline op invoked from read-only subquery",
                ..
            }
        ));
    }

    /// GQLRT-10: a `Catalog` op is rejected on the read-only path with the same
    /// diagnostic as every other write-bearing op (the unified rejection arm).
    #[test]
    fn read_only_dispatch_rejects_catalog_op() {
        let plan = planned("CREATE NODE TYPE :Person ()");
        let ctx = read_only_ctx(&plan);

        let err = execute_plan_read_only(&plan, &ctx)
            .expect_err("catalog op must be rejected on the read-only path");

        assert!(matches!(
            err,
            ExecutorError::InvalidTransactionState {
                detail: "write pipeline op invoked from read-only subquery",
                ..
            }
        ));
    }

    /// GQLRT-10: a read op still runs to completion through the read-only
    /// dispatcher (the shared read arms are reachable, not collateral of the
    /// write-op rejection).
    #[test]
    fn read_only_dispatch_runs_read_op() {
        let plan = planned("RETURN 1 AS n");
        let ctx = read_only_ctx(&plan);

        let table = execute_plan_read_only(&plan, &ctx).expect("read op runs read-only");

        assert_eq!(table.row_count(), 1);
        assert_eq!(table.rows()[0].values(), &[Value::Int(1)]);
    }
}
