//! Batch query driver: plan acceptance and operator assembly (F04-PR02,
//! joins/sets F04-PR03).
//!
//! [`try_execute_prefix`] runs the batchable head of an [`ExecutionPlan`] as
//! a pull-based operator tree and returns the materialized prefix table plus
//! the pipeline index where row execution resumes. The plan runner executes
//! any remaining (suffix) pipeline operators through the row dispatcher on
//! that prefix table, so aggregation, sorting, procedures, path operators,
//! and every other not-yet-batched family keep their exact row behavior
//! while already receiving batch-produced input through the stable
//! [`BindingTable`] interface.
//!
//! What the driver accepts:
//!
//! - Pattern phase: no pattern (unit seed), a single node/edge scan over any
//!   optimizer access path, nested inner one-hop expansions, the
//!   `JoinTree::Unit` anchor, inner hash joins, and left-outer joins over
//!   batchable children. Anything else (variable-length repeats, path
//!   selectors and modes, hash/outer-incompatible shapes, WCO, subplans,
//!   disjunctive scans, optional/questioned forms) declines: the whole plan
//!   stays on the row path. Seeded (correlated) top-level executions decline
//!   for the same reason; correlation *inside* an unseeded execution runs
//!   through nested batch contexts (see [`tree`](super::tree)).
//! - Pipeline prefix: the leading run of `Filter`, `Project`, `Limit`,
//!   non-leading `Match`/`OptionalMatch` (over batchable inner patterns),
//!   set-composition `Union` (all set/multiset variants plus `OTHERWISE`,
//!   with read-only arms), and `NEXT` blocks (`Chain` over a read-only
//!   right block, `CorrelatedChain` over a read-only block with a batchable
//!   pattern). The first other operator ends the prefix; an empty prefix
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
//! - No write-bearing right block runs read-only by narrowing: `Union` /
//!   `Chain` / `CorrelatedChain` arms are accepted only when the logical
//!   effect gate classifies them [`LogicalEffect::Query`]; anything else
//!   ends the prefix and the row dispatcher keeps its exact behavior.
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
    ExecutionPlan, FilterPredicate, PatternPlan, PipelineOp, ProjectExpr, SetOp,
    plan::{BindingTableSchema, LogicalEffect, classify_plan},
    runtime::{BindingTable, EvalCtx, ExecutorError, TxContext},
};

use super::super::{pattern, pipeline, plan_runner};
use super::budget::MemoryBudget;
use super::chain::{BatchChain, BatchCorrelatedChain, BatchMatch};
use super::filter::BatchFilter;
use super::operator::{BatchExecutionContext, PhysicalOperator};
use super::page::BatchPage;
use super::policy::BatchPolicy;
use super::project::BatchProject;
use super::set::BatchSet;
use super::tracer::trace_operator_to_table;
use super::tree::{build_join_tree, tree_is_batchable};
use super::unit::{BatchRowSource, BatchSeedRow};

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

/// Execute one right block with a single-row seed table.
///
/// Correlated-chain seam into block execution. Mirrors the plan runner's seeded row logic with batch routing: a batchable
/// block pattern evaluates through nested batch contexts, the pipeline runs
/// its batchable prefix through a row-source operator, and any remaining
/// operators resume through the read-only row dispatcher. Callers guarantee
/// a batchable block pattern (the driver declines otherwise); reaching an
/// unbatchable pattern here is an internal error, never a silent fallback.
///
/// The seed table carries exactly one row: block patterns observe the first
/// row, as on the row path.
///
/// # Errors
///
/// Returns the block's cancellation, budget, generation, data, and
/// invariant errors with the row path's diagnostics.
pub(crate) fn execute_seeded_subplan(
    plan: &ExecutionPlan,
    seed: BindingTable,
    ctx: &TxContext<'_, '_>,
    policy: BatchPolicy,
) -> Result<BindingTable, ExecutorError> {
    let eval = EvalCtx {
        tx: ctx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let (seed_schema, seed_rows) = seed.into_parts();
    let table = match (&plan.pattern_plan, seed_rows.first()) {
        (Some(pattern_plan), Some(first)) => {
            let target = plan_runner::target_schema(&seed_schema, pattern_plan);
            if !tree_is_batchable(&pattern_plan.join_tree) {
                return Err(ExecutorError::ImplementationDefined {
                    detail: "batch seeded block left a non-batchable pattern without a row fallback",
                });
            }
            let mut exec = BatchExecutionContext::borrowed(
                ctx.snapshot(),
                ctx.batch_cancel(),
                MemoryBudget::unlimited(),
            );
            // The row runner's proven-safe pattern pushdown applies to block
            // patterns exactly as to top-level ones.
            let row_limit = plan_runner::pattern_row_limit(plan);
            let inner = super::tree::trace_subtree(
                &pattern_plan.join_tree,
                pattern_plan,
                &target,
                Some(first.clone()),
                eval,
                policy,
                &mut exec,
            )?;
            exec.close();
            let mut rows = Vec::new();
            for row in inner {
                if pattern::filter_predicates_pass(
                    &pattern_plan.filters,
                    pattern_plan,
                    &row,
                    &target,
                    &eval,
                )? {
                    rows.push(row);
                    if row_limit.is_some_and(|limit| rows.len() >= limit) {
                        break;
                    }
                }
            }
            BindingTable::new(target, rows)
        }
        (Some(_), None) => BindingTable::new(seed_schema, Vec::new()),
        (None, _) => BindingTable::new(seed_schema, seed_rows),
    };
    execute_pipeline_from_table(plan, table, ctx, policy)
}

/// Execute a block pipeline over a materialized table with batch routing.
///
/// The batchable prefix runs through a row-source operator; any remaining
/// operators resume through the read-only row dispatcher with the block's
/// own expression tables.
fn execute_pipeline_from_table(
    plan: &ExecutionPlan,
    table: BindingTable,
    ctx: &TxContext<'_, '_>,
    policy: BatchPolicy,
) -> Result<BindingTable, ExecutorError> {
    let eval = EvalCtx {
        tx: ctx,
        expr_ids: &plan.expr_ids,
        subqueries: &plan.subqueries,
    };
    let (prefix, suffix_from) = split_prefix(&plan.pipeline, ctx)?;
    let root: Box<dyn PhysicalOperator + '_> = Box::new(BatchRowSource::new(table, policy));
    let root = apply_prefix(root, &prefix, eval, policy);
    let mut exec = BatchExecutionContext::borrowed(
        ctx.snapshot(),
        ctx.batch_cancel(),
        MemoryBudget::unlimited(),
    );
    let mut root = root;
    let prefix_table = trace_operator_to_table(root.as_mut(), &mut exec)?;
    let remaining = &plan.pipeline[suffix_from..];
    if remaining.is_empty() {
        return Ok(prefix_table);
    }
    pipeline::execute_pipeline_read_only_with_plan(
        remaining,
        prefix_table,
        ctx,
        &plan.expr_ids,
        &plan.subqueries,
    )
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
        root = apply_prefix(root, &prefix, eval, policy);
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
        build_join_tree(&pattern.join_tree, pattern, schema, eval, policy, None)?
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
    root = apply_prefix(root, &prefix, eval, policy);
    Ok(Some(execute_tree(root, ctx, suffix_from)?))
}

/// One batchable pipeline operator borrowed from the plan.
enum BatchPrefixOp<'p> {
    Filter(&'p FilterPredicate),
    Project(&'p [ProjectExpr], BindingTableSchema),
    Limit(u64, u64),
    Match(&'p PatternPlan),
    OptionalMatch(&'p PatternPlan),
    Union { op: SetOp, rhs: &'p ExecutionPlan },
    Chain(&'p ExecutionPlan),
    CorrelatedChain(&'p ExecutionPlan),
}

/// Split the leading batchable pipeline run from the row suffix.
///
/// Limit amounts resolve here through the row path's resolver, so parameter
/// diagnostics agree exactly. Resolution happens before any rows flow, which
/// is the one ordering difference versus the row path (it resolves each
/// limit at its pipeline position): a plan that is simultaneously
/// pattern-erroneous and limit erroneous reports the limit error first.
/// Both paths still fail the plan; row-identical outcomes are unaffected.
///
/// `Match` arms join only when their inner pattern is batch-buildable;
/// set/chain arms join only when the right block classifies
/// [`LogicalEffect::Query`] (so read-only arm execution is exact) and, for
/// correlated chains, the block pattern is batch-buildable. Anything else
/// ends the prefix and the row dispatcher keeps its exact behavior.
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
            PipelineOp::Match(pattern) => {
                if !tree_is_batchable(&pattern.join_tree) {
                    return Ok((prefix, index));
                }
                prefix.push(BatchPrefixOp::Match(pattern));
            }
            PipelineOp::OptionalMatch(pattern) => {
                if !tree_is_batchable(&pattern.join_tree) {
                    return Ok((prefix, index));
                }
                prefix.push(BatchPrefixOp::OptionalMatch(pattern));
            }
            PipelineOp::Union { op, rhs } => {
                if !subplan_is_read_only(rhs) {
                    return Ok((prefix, index));
                }
                prefix.push(BatchPrefixOp::Union { op: *op, rhs });
            }
            PipelineOp::Chain(rhs) => {
                if !subplan_is_read_only(rhs) {
                    return Ok((prefix, index));
                }
                prefix.push(BatchPrefixOp::Chain(rhs));
            }
            PipelineOp::CorrelatedChain(rhs) => {
                if !subplan_is_read_only(rhs) || !subplan_pattern_is_batchable(rhs) {
                    return Ok((prefix, index));
                }
                prefix.push(BatchPrefixOp::CorrelatedChain(rhs));
            }
            _ => return Ok((prefix, index)),
        }
    }
    Ok((prefix, pipeline.len()))
}

/// True when a right block carries no write effect.
///
/// The logical effect gate resolves procedure effects from registration
/// metadata, so this is precise rather than name-based: only pure-query
/// blocks run through the read-only arm path.
fn subplan_is_read_only(plan: &ExecutionPlan) -> bool {
    classify_plan(plan).effect == LogicalEffect::Query
}

/// True when a right block has no pattern or a batch-buildable one.
///
/// The seeded-subplan helper evaluates block patterns without a row
/// fallback, so an unbatchable block pattern ends the prefix instead.
fn subplan_pattern_is_batchable(plan: &ExecutionPlan) -> bool {
    plan.pattern_plan
        .as_ref()
        .is_none_or(|pattern| tree_is_batchable(&pattern.join_tree))
}

/// Wrap a pattern root with the accepted pipeline prefix, in order.
fn apply_prefix<'e, 'a, 'ctx, 'g, 'plan>(
    mut root: Box<dyn PhysicalOperator + 'e>,
    prefix: &[BatchPrefixOp<'plan>],
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
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
            BatchPrefixOp::Match(pattern) | BatchPrefixOp::OptionalMatch(pattern) => {
                let optional = matches!(op, BatchPrefixOp::OptionalMatch(_));
                let input_schema = root.output_schema().clone();
                let target = pipeline::target_schema(&input_schema, pattern);
                root = Box::new(BatchMatch::new(
                    root,
                    pattern,
                    target,
                    input_schema,
                    optional,
                    eval,
                    policy,
                ));
            }
            BatchPrefixOp::Union { op, rhs } => {
                let schema = root.output_schema().clone();
                root = Box::new(BatchSet::new(root, *op, rhs, schema, eval, policy));
            }
            BatchPrefixOp::Chain(rhs) => {
                root = Box::new(BatchChain::new(root, rhs, eval, policy));
            }
            BatchPrefixOp::CorrelatedChain(rhs) => {
                root = Box::new(BatchCorrelatedChain::new(root, rhs, eval, policy));
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
