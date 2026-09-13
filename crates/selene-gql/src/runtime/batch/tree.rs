//! Physical join-tree assembly. Every executable shape has exactly one route.

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
use crate::{
    JoinTree, PatternPlan,
    plan::BindingTableSchema,
    runtime::{Binding, EvalCtx, ExecutorError},
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

pub(crate) fn build_join_tree<'e, 'a: 'e, 'ctx: 'e, 'g: 'e, 'p: 'e>(
    tree: &'p JoinTree,
    pattern: &'p PatternPlan,
    schema: BindingTableSchema,
    eval: EvalCtx<'a, 'ctx, 'g, 'p>,
    policy: BatchPolicy,
    seed: Option<Binding>,
) -> Result<Box<dyn PhysicalOperator + 'e>, ExecutorError> {
    Ok(match tree {
        JoinTree::Paths(program) => Box::new(crate::runtime::product_path::BatchPath::new(
            program, eval, schema, seed, policy,
        )),
        JoinTree::Scan(scan) => {
            let operator = BatchScan::new(scan, pattern, schema, eval, policy);
            Box::new(match seed {
                Some(seed) => operator.with_seed(seed),
                None => operator,
            })
        }
        JoinTree::Expand {
            child,
            edge,
            direction,
        } => {
            let child = build_join_tree(child, pattern, schema.clone(), eval, policy, seed)?;
            Box::new(BatchExpand::new(
                child, edge, *direction, pattern, schema, eval, policy,
            ))
        }
        JoinTree::Unit => Box::new(BatchSeedRow::null_row(schema)),
        JoinTree::HashJoin {
            left,
            right,
            key,
            build_side,
        } => {
            let left = build_join_tree(left, pattern, schema.clone(), eval, policy, seed.clone())?;
            let right = build_join_tree(right, pattern, schema.clone(), eval, policy, seed)?;
            Box::new(BatchHashJoin::new(
                left,
                right,
                key,
                *build_side,
                schema,
                policy,
            ))
        }
        JoinTree::Outer {
            left,
            right,
            key,
            right_filters,
        } => {
            let left = build_join_tree(left, pattern, schema.clone(), eval, policy, seed)?;
            Box::new(BatchOuterJoin::new(
                left,
                right,
                pattern,
                key,
                right_filters,
                schema,
                eval,
                policy,
            ))
        }
        JoinTree::WorstCaseOptimal { intersection, .. } => {
            // The optimizer's phase-A marker wraps an ordinary physical tree;
            // it does not implement a second join algorithm.
            let [inner] = intersection.as_slice() else {
                return Err(ExecutorError::ImplementationDefined {
                    detail: if intersection.is_empty() {
                        "WorstCaseOptimal with empty intersection"
                    } else {
                        "WorstCaseOptimal with multiple intersections"
                    },
                });
            };
            return build_join_tree(inner, pattern, schema, eval, policy, seed);
        }
        JoinTree::Subplan(_) | JoinTree::DisjunctiveScan { .. } => Box::new(
            super::tree_source::BatchTreeSource::new(tree, pattern, schema, eval, policy, seed),
        ),
    })
}

/// Trace a correlated subtree in a nested context without releasing the parent pin.
pub(crate) fn trace_subtree(
    tree: &JoinTree,
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    seed: Option<Binding>,
    eval: EvalCtx<'_, '_, '_, '_>,
    policy: BatchPolicy,
    parent: &mut BatchExecutionContext<'_>,
) -> Result<Vec<Binding>, ExecutorError> {
    let mut root = build_join_tree(tree, pattern, schema.clone(), eval, policy, seed)?;
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
