//! Batch join-tree assembly shared by the driver and correlated operators.
//!
//! [`build_join_tree`] lowers one [`JoinTree`] into a pull-based operator
//! tree over a single pinned snapshot. Accepted shapes are the primitive
//! families plus the F04-PR03 join family: single scans, nested inner
//! one-hop expansions, the `Unit` anchor, inner hash joins, and left-outer
//! joins, and complete logical path programs. WCO, subplans and disjunctive
//! scans still decline; paths never fall back to a second evaluator.
//!
//! [`tree_is_batchable`] is the pure acceptance predicate over the same
//! shapes. The driver consults it before committing to batch execution for
//! subtrees that run without a row fallback (outer-join right sides and
//! non-leading match patterns): a non-batchable nested shape declines the
//! whole plan while the row path is still available, never mid-execution.
//!
//! [`trace_subtree`] evaluates one batchable subtree to materialized rows in
//! a nested execution context ([`BatchExecutionContext::nested`]), for
//! per-input-row correlated evaluation. The nested context closes without
//! touching the parent pin, and its scratch budget balances before it
//! closes; the caller reserves every row it keeps.

use crate::{
    JoinTree, PatternPlan,
    plan::BindingTableSchema,
    runtime::{Binding, EvalCtx, ExecutorError},
};

use super::{
    binding_batch::BatchBuffer,
    expand::BatchExpand,
    join::BatchHashJoin,
    operator::{BatchExecutionContext, PhysicalOperator},
    outer::BatchOuterJoin,
    policy::BatchPolicy,
    scan::BatchScan,
    unit::BatchSeedRow,
};

pub(crate) fn contains_paths(tree: &JoinTree) -> bool {
    match tree {
        JoinTree::Paths(_) => true,
        JoinTree::Expand { child, .. } => contains_paths(child),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            contains_paths(left) || contains_paths(right)
        }
        JoinTree::WorstCaseOptimal { intersection, .. } => intersection.iter().any(contains_paths),
        JoinTree::Subplan(plan) => plan
            .pattern_plan
            .as_ref()
            .is_some_and(|p| contains_paths(&p.join_tree)),
        JoinTree::Unit | JoinTree::Scan(_) | JoinTree::DisjunctiveScan { .. } => false,
    }
}

/// True when `tree` lowers to batch operators without a row fallback.
///
/// This must stay in lockstep with [`build_join_tree`]: any shape built
/// there (with any seed) reports true here, and anything else reports
/// false. Disjunctive scans report false: the optimizer-emitted union point
/// stays on the row path in this slice.
pub(crate) fn tree_is_batchable(tree: &JoinTree) -> bool {
    match tree {
        JoinTree::Unit | JoinTree::Scan(_) | JoinTree::Paths(_) => true,
        JoinTree::Expand { child, .. } => tree_is_batchable(child),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            tree_is_batchable(left) && tree_is_batchable(right)
        }
        JoinTree::WorstCaseOptimal { .. }
        | JoinTree::Subplan(_)
        | JoinTree::DisjunctiveScan { .. } => false,
    }
}

/// Build a batch operator subtree for one join tree, or decline.
///
/// `seed` threads correlated bindings into every scan of the subtree (both
/// hash-join sides share it, exactly as the row walk shares its seed
/// environment); `None` builds the unseeded top-level shape. Callers
/// pre-check nested no-fallback subtrees with [`tree_is_batchable`]; a
/// declining shape here returns `Ok(None)`.
///
/// # Errors
///
/// Returns the executor error for key-resolution failures only. Operator
/// `init` (not building) reports resolution, cancellation, and budget
/// failures.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_join_tree<'e, 'a, 'ctx, 'g, 'plan>(
    tree: &'plan JoinTree,
    pattern: &'plan PatternPlan,
    schema: BindingTableSchema,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
    seed: Option<Binding>,
) -> Result<Option<Box<dyn PhysicalOperator + 'e>>, ExecutorError>
where
    'a: 'e,
    'ctx: 'e,
    'g: 'e,
    'plan: 'e,
{
    match tree {
        JoinTree::Paths(program) => Ok(Some(Box::new(
            crate::runtime::product_path::BatchPath::new(program, eval, schema, seed, policy),
        ))),
        JoinTree::Scan(scan) => {
            let operator = BatchScan::new(scan, pattern, schema, eval, policy);
            Ok(Some(Box::new(match seed {
                Some(seed) => operator.with_seed(seed),
                None => operator,
            })))
        }
        JoinTree::Expand {
            child,
            edge,
            direction,
        } => {
            let Some(built) = build_join_tree(child, pattern, schema.clone(), eval, policy, seed)?
            else {
                return Ok(None);
            };
            Ok(Some(Box::new(BatchExpand::new(
                built, edge, *direction, pattern, schema, eval, policy,
            ))))
        }
        JoinTree::Unit => Ok(Some(Box::new(BatchSeedRow::null_row(schema)))),
        JoinTree::HashJoin {
            left,
            right,
            key,
            build_side,
        } => {
            // Both sides share one seed, exactly as the row walk evaluates
            // build and probe under the same seed environment. Children pass
            // positionally as left/right; the operator recovers probe-major
            // output order from `build_side`.
            let Some(left_op) =
                build_join_tree(left, pattern, schema.clone(), eval, policy, seed.clone())?
            else {
                return Ok(None);
            };
            let Some(right_op) =
                build_join_tree(right, pattern, schema.clone(), eval, policy, seed)?
            else {
                return Ok(None);
            };
            Ok(Some(Box::new(BatchHashJoin::new(
                left_op,
                right_op,
                key,
                *build_side,
                schema,
                policy,
            ))))
        }
        JoinTree::Outer {
            left,
            right,
            key,
            right_filters,
        } => {
            let Some(built) = build_join_tree(left, pattern, schema.clone(), eval, policy, seed)?
            else {
                return Ok(None);
            };
            if !tree_is_batchable(right) {
                return Ok(None);
            }
            Ok(Some(Box::new(BatchOuterJoin::new(
                built,
                right,
                pattern,
                key,
                right_filters,
                schema,
                eval,
                policy,
            ))))
        }
        JoinTree::WorstCaseOptimal { .. }
        | JoinTree::Subplan(_)
        | JoinTree::DisjunctiveScan { .. } => Ok(None),
    }
}

/// Temporary generic-row caller seam, deleted with that dispatcher in F04-PR09.
/// The path itself always uses the physical batch implementation, never fallback.
pub(crate) fn path_from_row_dispatch(
    tree: &JoinTree,
    env: crate::runtime::pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Vec<Binding>, ExecutorError> {
    let mut ctx = BatchExecutionContext::borrowed(
        env.ctx.tx.snapshot(),
        env.ctx.tx.batch_cancel(),
        super::budget::MemoryBudget::unlimited(),
    );
    trace_subtree(
        tree,
        env.pattern,
        env.schema,
        env.seed.cloned(),
        *env.ctx,
        BatchPolicy::default_policy(),
        &mut ctx,
    )
}

/// One-hop primitive adapter for generic-row parents; no second edge evaluator.
pub(crate) fn expand_from_row_dispatch(
    child: &JoinTree,
    edge: &crate::EdgeMatch,
    direction: crate::EdgeDirection,
    env: crate::runtime::pattern::WalkContext<'_, '_, '_, '_, '_, '_>,
) -> Result<Vec<Binding>, ExecutorError> {
    let rows = crate::runtime::pattern::walk_join_tree(child, env)?;
    let policy = BatchPolicy::default_policy();
    let source = super::unit::BatchRowSource::new(
        crate::BindingTable::new(env.schema.clone(), rows),
        policy,
    );
    let mut root = BatchExpand::new(
        Box::new(source),
        edge,
        direction,
        env.pattern,
        env.schema.clone(),
        *env.ctx,
        policy,
    );
    let mut ctx = BatchExecutionContext::borrowed(
        env.ctx.tx.snapshot(),
        env.ctx.tx.batch_cancel(),
        super::budget::MemoryBudget::unlimited(),
    );
    super::tracer::trace_operator_to_table(&mut root, &mut ctx).map(|table| table.into_parts().1)
}

/// Evaluate one batchable subtree to materialized rows for a single seed.
///
/// Builds the subtree with `seed`, pulls it to completion in a nested
/// execution context, and returns the rows in `schema` order. Pattern-level
/// filters are the caller's responsibility (the row walk separates tree
/// evaluation from pattern filtering the same way).
///
/// # Errors
///
/// Returns `ImplementationDefined` when the subtree declines batch building
/// (callers pre-check with [`tree_is_batchable`]), plus the subtree's
/// cancellation, budget, generation, and data errors. No partial rows are
/// returned on failure.
pub(crate) fn trace_subtree(
    tree: &JoinTree,
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    seed: Option<Binding>,
    eval: EvalCtx<'_, '_, '_, '_>,
    policy: BatchPolicy,
    parent: &mut BatchExecutionContext<'_>,
) -> Result<Vec<Binding>, ExecutorError> {
    let Some(mut root) = build_join_tree(tree, pattern, schema.clone(), eval, policy, seed)? else {
        return Err(ExecutorError::ImplementationDefined {
            detail: "batch subtree left a non-batchable shape without a row fallback",
        });
    };
    let mut nested = parent.nested()?;
    if let Err(err) = root.init(&mut nested) {
        root.close(&mut nested);
        return Err(err);
    }
    let mut buffer = BatchBuffer::new();
    let mut rows = Vec::new();
    loop {
        match root.next_batch(&mut nested, &mut buffer) {
            Ok(Some(batch)) => {
                rows.reserve(batch.logical_rows());
                for index in 0..batch.logical_rows() {
                    rows.push(Binding::new(batch.logical_row(index)));
                }
                nested.budget_mut().release(batch.estimated_bytes());
                batch.recycle(&mut buffer);
            }
            Ok(None) => break,
            Err(err) => {
                root.close(&mut nested);
                return Err(err);
            }
        }
    }
    root.close(&mut nested);
    Ok(rows)
}
