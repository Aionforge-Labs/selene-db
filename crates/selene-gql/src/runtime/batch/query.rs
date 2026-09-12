//! Batch query driver: plan acceptance and operator assembly (F04-PR02).
//!
//! [`try_execute_prefix`] runs the batchable head of an [`ExecutionPlan`] as
//! a pull-based operator tree and returns the materialized prefix table plus
//! the pipeline index where row execution resumes. The plan runner executes
//! any remaining (suffix) pipeline operators through the row dispatcher on
//! that prefix table, so native procedures, path operators, sorting,
//! aggregation, and every other not-yet-batched family keep their exact row
//! behavior while already receiving batch-produced input through the stable
//! [`BindingTable`] interface.
//!
//! What the driver accepts:
//!
//! - Pattern phase: no pattern (unit seed), a single node/edge scan over any
//!   optimizer access path, nested inner one-hop expansions, and the
//!   `JoinTree::Unit` anchor. Anything else (variable-length repeats, path
//!   selectors and modes, hash/outer joins, WCO, subplans, disjunctive scans,
//!   optional/questioned forms) declines: the whole plan stays on the row
//!   path. Seeded (correlated) executions decline for the same reason.
//! - Pipeline prefix: the leading run of `Filter`, `Project`, and `Limit`
//!   operators. The first other operator ends the prefix; an empty prefix
//!   with no pattern declines.
//! - Pattern-level filters (`PatternPlan::filters`) run as a batch filter
//!   between the pattern tree and the pipeline prefix, matching the row
//!   path's post-walk filtering position.
//!
//! What the driver never does:
//!
//! - No limit is pushed below any operator beyond the row path's own
//!   proven-safe pushdown. The pattern phase truncates with exactly the
//!   runner's shared `pattern_row_limit` bound (leading limits and safe
//!   post-return projections) and runs in full otherwise; the page operator
//!   then short-circuits the pull stream at its own pipeline position. A
//!   small `LIMIT` after a multiplicity-producing expansion therefore sees
//!   the same rows the row path's pushdown would have produced — never a
//!   prematurely limited seed, never an untruncated one where the row path
//!   truncates.
//! - No error is suppressed or reordered into a fallback: [`try_execute_prefix`]
//!   returns `Ok(None)` only for shapes it does not cover. Any operator
//!   error aborts with the row path's diagnostic, and the caller runs the
//!   row path only after a decline, never after a failure.
//! - No candidates cross executions: every accepted execution resolves its
//!   typed candidates fresh against the pinned statement snapshot inside
//!   operator `init`. Cached plans carry access paths only, so reuse across
//!   generations re-resolves rather than rebinding stale rows.
//!
//! The driver consumes the [`ExecutionPlan`] rather than lowering the
//! [`LogicalPlan`](crate::plan::LogicalPlan) a second time on purpose: the
//! single-path gate already lowers every statement through logical planning
//! and effect-verifies the row plan against it, and the optimizer access
//! paths the batch scan needs live on the row plan. Re-deriving access
//! selection in a parallel logical-to-batch lowering would duplicate planner
//! authority; the row adapter retires at F04-PR09, when batch lowering can
//! take over the whole plan.

use crate::{
    ExecutionPlan, FilterPredicate, JoinTree, PatternPlan, PipelineOp, ProjectExpr,
    plan::BindingTableSchema,
    runtime::{BindingTable, EvalCtx, ExecutorError, TxContext},
};

use super::super::{pattern, pipeline};
use super::budget::MemoryBudget;
use super::expand::BatchExpand;
use super::filter::BatchFilter;
use super::operator::{BatchExecutionContext, PhysicalOperator};
use super::page::BatchPage;
use super::policy::BatchPolicy;
use super::project::BatchProject;
use super::scan::BatchScan;
use super::tracer::trace_operator_to_table;
use super::unit::BatchSeedRow;

/// Batch-executed head of a plan: the prefix table plus resume position.
///
/// `suffix_from` indexes `plan.pipeline`: operators before it ran in
/// batches, operators from it onward run through the row dispatcher. When it
/// equals the pipeline length the plan ran entirely in batches.
pub(crate) struct PrefixOutcome {
    /// Materialized prefix rows in the prefix output schema.
    pub(crate) table: BindingTable,
    /// Pipeline index where row execution resumes.
    pub(crate) suffix_from: usize,
}

/// Execute the batchable head of `plan`, or decline.
///
/// Returns `Ok(None)` when the plan's shape is outside the batch families
/// (the caller runs the row path). Returns `Err` for operator failures with
/// the row path's diagnostics; the caller must not fall back to the row path
/// after an error.
///
/// Only unseeded executions are accepted: correlated per-row plans carry a
/// seed the batch tree does not consume.
pub(crate) fn try_execute_prefix(
    plan: &ExecutionPlan,
    ctx: &TxContext<'_, '_>,
) -> Result<Option<PrefixOutcome>, ExecutorError> {
    execute_prefix(plan, ctx, BatchPolicy::default_policy())
}

/// Execute the prefix with a caller-chosen batch policy (test seam).
///
/// Production always uses the default policy; differentials vary the policy
/// to prove logical results never depend on physical batch shape.
#[cfg(test)]
pub(crate) fn execute_with_test_policy(
    plan: &ExecutionPlan,
    ctx: &TxContext<'_, '_>,
    policy: BatchPolicy,
) -> Result<Option<PrefixOutcome>, ExecutorError> {
    execute_prefix(plan, ctx, policy)
}

fn execute_prefix(
    plan: &ExecutionPlan,
    ctx: &TxContext<'_, '_>,
    policy: BatchPolicy,
) -> Result<Option<PrefixOutcome>, ExecutorError> {
    let eval = EvalCtx {
        tx: ctx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let (prefix, suffix_from) = split_prefix(&plan.pipeline, ctx)?;
    // The row runner's proven-safe pattern pushdown, replicated exactly (see
    // the pattern branch below): truncate in precisely the cases the row path
    // truncates, never otherwise.
    let row_limit = super::super::plan_runner::pattern_row_limit(plan);
    let Some(pattern) = plan.pattern_plan.as_ref() else {
        if prefix.is_empty() {
            return Ok(None);
        }
        let mut root: Box<dyn PhysicalOperator + '_> = if row_limit == Some(0) {
            Box::new(BatchSeedRow::empty_table(BindingTableSchema {
                columns: Vec::new(),
            }))
        } else {
            Box::new(BatchSeedRow::unit())
        };
        if let Some(bound) = row_limit
            && bound > 0
        {
            let bound = u64::try_from(bound).map_err(|_| ExecutorError::ImplementationDefined {
                detail: "batch pattern row limit exceeds the supported range",
            })?;
            root = Box::new(BatchPage::new(root, 0, bound));
        }
        root = apply_prefix(root, &prefix, eval);
        return Ok(Some(execute_tree(root, ctx, suffix_from)?));
    };
    let schema = pattern::schema_for_pattern(pattern);
    // The row runner's proven-safe pattern pushdown, replicated exactly: the
    // pattern phase truncates in precisely the cases the row path truncates
    // (leading limits and safe post-return projections), and never otherwise.
    // A zero bound skips the pattern operators entirely, as the row path
    // skips its walk; downstream prefix operators still run over the empty
    // input on both paths.
    let row_limit = super::super::plan_runner::pattern_row_limit(plan);
    let Some(mut root): Option<Box<dyn PhysicalOperator + '_>> = (if row_limit == Some(0) {
        Some(Box::new(BatchSeedRow::empty_table(schema.clone())))
    } else {
        build_join_tree(&pattern.join_tree, pattern, schema, eval, policy)?
    }) else {
        return Ok(None);
    };
    for predicate in &pattern.filters {
        root = Box::new(BatchFilter::new(root, predicate, eval));
    }
    // The proven-safe pattern pushdown truncates post-filter pattern rows,
    // exactly where the row walk truncates: after pattern filters, before
    // the pipeline prefix. A cap placed below the filters would truncate
    // unfiltered scan rows the row path never truncates.
    if let Some(bound) = row_limit
        && bound > 0
    {
        let bound = u64::try_from(bound).map_err(|_| ExecutorError::ImplementationDefined {
            detail: "batch pattern row limit exceeds the supported range",
        })?;
        root = Box::new(BatchPage::new(root, 0, bound));
    }
    root = apply_prefix(root, &prefix, eval);
    Ok(Some(execute_tree(root, ctx, suffix_from)?))
}

/// One batchable pipeline operator borrowed from the plan.
enum BatchPrefixOp<'p> {
    Filter(&'p FilterPredicate),
    Project(&'p [ProjectExpr], BindingTableSchema),
    Limit(u64, u64),
}

/// Split the leading batchable pipeline run from the row suffix.
///
/// Limit amounts resolve here through the row path's resolver, so parameter
/// diagnostics agree exactly. Resolution happens before any rows flow, which
/// is the one ordering difference versus the row path (it resolves each
/// limit at its pipeline position): a plan that is simultaneously
/// pattern-erroneous and limit erroneous reports the limit error first.
/// Both paths still fail the plan; row-identical outcomes are unaffected.
fn split_prefix<'p>(
    pipeline: &'p [PipelineOp],
    ctx: &TxContext<'_, '_>,
) -> Result<(Vec<BatchPrefixOp<'p>>, usize), ExecutorError> {
    let mut prefix = Vec::new();
    for (index, op) in pipeline.iter().enumerate() {
        match op {
            PipelineOp::Filter(predicate) => prefix.push(BatchPrefixOp::Filter(predicate)),
            PipelineOp::Project(items) => {
                let schema = pipeline::schema_for_items(items);
                prefix.push(BatchPrefixOp::Project(items, schema));
            }
            PipelineOp::Limit { offset, count } => {
                let offset = pipeline::resolve_amount(offset, ctx)?;
                let count = pipeline::resolve_amount(count, ctx)?;
                prefix.push(BatchPrefixOp::Limit(offset, count));
            }
            _ => return Ok((prefix, index)),
        }
    }
    Ok((prefix, pipeline.len()))
}

/// Build a batch operator subtree for one join tree, or decline.
///
/// Accepted shapes mirror the primitive families only; every other variant
/// (repeats, selectors, modes, joins, WCO, subplans, disjunctive scans,
/// questioned/optional forms) returns `Ok(None)`.
fn build_join_tree<'e, 'a, 'ctx, 'g, 'plan>(
    tree: &'plan JoinTree,
    pattern: &'plan PatternPlan,
    schema: BindingTableSchema,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
) -> Result<Option<Box<dyn PhysicalOperator + 'e>>, ExecutorError>
where
    'a: 'e,
    'ctx: 'e,
    'g: 'e,
    'plan: 'e,
{
    match tree {
        JoinTree::Scan(scan) => Ok(Some(Box::new(BatchScan::new(
            scan, pattern, schema, eval, policy,
        )))),
        JoinTree::Expand {
            child,
            edge,
            direction,
        } => {
            let Some(built) = build_join_tree(child, pattern, schema.clone(), eval, policy)? else {
                return Ok(None);
            };
            Ok(Some(Box::new(BatchExpand::new(
                built, edge, *direction, pattern, schema, eval, policy,
            ))))
        }
        JoinTree::Unit => Ok(Some(Box::new(BatchSeedRow::null_row(schema)))),
        JoinTree::Questioned { .. }
        | JoinTree::Repeat { .. }
        | JoinTree::PathSearch { .. }
        | JoinTree::PathModeFilter { .. }
        | JoinTree::MatchModeFilter { .. }
        | JoinTree::HashJoin { .. }
        | JoinTree::Outer { .. }
        | JoinTree::WorstCaseOptimal { .. }
        | JoinTree::Subplan(_)
        | JoinTree::DisjunctiveScan { .. } => Ok(None),
    }
}

/// Wrap a pattern root with the accepted pipeline prefix, in order.
fn apply_prefix<'e, 'a, 'ctx, 'g, 'plan>(
    mut root: Box<dyn PhysicalOperator + 'e>,
    prefix: &[BatchPrefixOp<'plan>],
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
) -> Box<dyn PhysicalOperator + 'e>
where
    'a: 'e,
    'ctx: 'e,
    'g: 'e,
    'plan: 'e,
{
    for op in prefix {
        match op {
            BatchPrefixOp::Filter(predicate) => {
                root = Box::new(BatchFilter::new(root, predicate, eval));
            }
            BatchPrefixOp::Project(items, schema) => {
                root = Box::new(BatchProject::new(root, items, schema.clone(), eval));
            }
            BatchPrefixOp::Limit(offset, count) => {
                root = Box::new(BatchPage::new(root, *offset, *count));
            }
        }
    }
    root
}

/// Pull an assembled operator tree to a prefix table.
///
/// The execution borrows the statement snapshot, so batch operators observe
/// the same working graph the row path would (including explicit-transaction
/// writes); cancellation, deadlines, and scan budgets come from the statement
/// context with identical GQLSTATUS mapping. Errors close the tree and abort
/// without a partial table.
fn execute_tree(
    mut root: Box<dyn PhysicalOperator + '_>,
    ctx: &TxContext<'_, '_>,
    suffix_from: usize,
) -> Result<PrefixOutcome, ExecutorError> {
    let mut exec = BatchExecutionContext::borrowed(
        ctx.snapshot(),
        ctx.batch_cancel(),
        MemoryBudget::unlimited(),
    );
    let table = trace_operator_to_table(root.as_mut(), &mut exec)?;
    Ok(PrefixOutcome { table, suffix_from })
}
