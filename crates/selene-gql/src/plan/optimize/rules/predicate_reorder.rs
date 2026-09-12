//! Selectivity-based predicate reordering.

use std::cmp::Ordering;

use crate::plan::{
    BindingDef, ExecutionPlan, FilterPredicate, JoinTree,
    optimize::{OptimizeContext, Rule, Transformed, selectivity, walk},
};

/// Reorder predicate buckets by estimated selectivity.
pub struct PredicateReorder;

impl Rule for PredicateReorder {
    fn name(&self) -> &'static str {
        "predicate_reorder"
    }

    fn rewrite(
        &self,
        mut plan: ExecutionPlan,
        ctx: &OptimizeContext<'_>,
    ) -> Transformed<ExecutionPlan> {
        let mut changed = false;
        if let Some(pattern) = &mut plan.pattern_plan {
            let scan_ctx = selectivity::ScanContext {
                bindings: &pattern.bindings,
                catalog: ctx.index_catalog,
            };
            changed |= reorder_bucket(&mut pattern.filters, ctx, &scan_ctx);
            changed |= reorder_tree(&mut pattern.join_tree, &pattern.bindings, ctx);
        }
        let nested = walk::recurse_rule_subplans(plan, self, ctx);
        changed |= nested.changed;
        Transformed {
            plan: nested.plan,
            changed,
        }
    }
}

fn reorder_tree(tree: &mut JoinTree, bindings: &[BindingDef], ctx: &OptimizeContext<'_>) -> bool {
    let scan_ctx = selectivity::ScanContext {
        bindings,
        catalog: ctx.index_catalog,
    };
    match tree {
        JoinTree::Unit | JoinTree::Paths(_) => false,
        JoinTree::Scan(scan) => reorder_bucket(&mut scan.property_predicates, ctx, &scan_ctx),
        JoinTree::Expand { child, edge, .. } => {
            reorder_tree(child, bindings, ctx)
                | reorder_bucket(&mut edge.property_predicates, ctx, &scan_ctx)
                | reorder_bucket(&mut edge.right_property_predicates, ctx, &scan_ctx)
        }
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            reorder_tree(left, bindings, ctx) | reorder_tree(right, bindings, ctx)
        }
        JoinTree::WorstCaseOptimal { .. } | JoinTree::Subplan(_) => false,
        // Reorder each branch's residual predicates independently. After
        // index-rule rewrites, each branch's `property_predicates` carries
        // only the residual (non-consumed) filters; selectivity ordering
        // applies per branch.
        JoinTree::DisjunctiveScan { branches, .. } => {
            branches.iter_mut().fold(false, |changed, branch| {
                reorder_bucket(&mut branch.property_predicates, ctx, &scan_ctx) | changed
            })
        }
    }
}

fn reorder_bucket(
    predicates: &mut [FilterPredicate],
    ctx: &OptimizeContext<'_>,
    scan_ctx: &selectivity::ScanContext<'_>,
) -> bool {
    // Precompute each predicate's selectivity estimate exactly once, keyed by
    // its (bucket-unique) `expr_id`. The old comparator called
    // `selectivity::estimate` — a linear binding scan plus two catalog probes —
    // for every comparison (O(n log n) probes); now it is O(n) probes and the
    // comparator is an O(1) map lookup. `f64` is not `Ord`, so the comparator
    // still uses `partial_cmp` (no `sort_by_cached_key`).
    let estimates: std::collections::HashMap<u32, f64> = predicates
        .iter()
        .map(|pred| {
            (
                pred.expr_id.get(),
                selectivity::estimate(pred, ctx, scan_ctx),
            )
        })
        .collect();
    let estimate_of = |pred: &FilterPredicate| estimates[&pred.expr_id.get()];

    let before: Vec<u32> = predicates.iter().map(|pred| pred.expr_id.get()).collect();
    predicates.sort_by(|left, right| {
        estimate_of(left)
            .partial_cmp(&estimate_of(right))
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.expr_id.get().cmp(&right.expr_id.get()))
    });
    predicates
        .iter()
        .map(|pred| pred.expr_id.get())
        .ne(before.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExprId, FilterPredicateKind, Literal, SourceSpan, analyze::AnalyzedType};

    fn ne_predicate(expr_id: u32) -> FilterPredicate {
        let span = SourceSpan::new(0, 1);
        FilterPredicate {
            expr: crate::ValueExpr::BinaryOp {
                op: crate::BinaryOp::Ne,
                lhs: Box::new(crate::ValueExpr::Literal(Literal::Integer(1, span))),
                rhs: Box::new(crate::ValueExpr::Literal(Literal::Integer(2, span))),
                span,
            },
            expr_id: ExprId::new(expr_id),
            ty: AnalyzedType::DYNAMIC,
            binding_refs: Vec::new(),
            kind: FilterPredicateKind::Expression,
            index_consumed: false,
            span,
        }
    }

    #[test]
    fn predicate_reorder_canonical_under_equal_selectivity() {
        let mut predicates = vec![ne_predicate(3), ne_predicate(1), ne_predicate(2)];
        let ctx = OptimizeContext::default();
        let scan_ctx = selectivity::ScanContext {
            bindings: &[],
            catalog: None,
        };

        let changed = reorder_bucket(&mut predicates, &ctx, &scan_ctx);

        // ExprId is stable within a plan but can be re-issued by rules that
        // synthesize expressions; the tie-break only canonicalizes one run.
        assert!(changed);
        assert_eq!(
            predicates
                .iter()
                .map(|predicate| predicate.expr_id.get())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
