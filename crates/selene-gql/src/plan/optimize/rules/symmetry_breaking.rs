//! WCO symmetry-breaking metadata rule.

use std::cmp::Ordering;

use crate::plan::{
    ExecutionPlan, JoinTree, NodeIdOrdering,
    optimize::{OptimizeContext, Rule, Transformed, walk},
};

use super::wco_join;

/// Populate structural node-id ordering on WCO markers.
pub struct SymmetryBreaking;

impl Rule for SymmetryBreaking {
    fn name(&self) -> &'static str {
        "symmetry_breaking"
    }

    fn rewrite(
        &self,
        mut plan: ExecutionPlan,
        ctx: &OptimizeContext<'_>,
    ) -> Transformed<ExecutionPlan> {
        let mut changed = false;
        if let Some(pattern) = &mut plan.pattern_plan {
            changed |= rewrite_tree(
                &mut pattern.join_tree,
                ctx.impl_defined_caps.max_wco_traversal_nodes,
            );
        }
        let nested = walk::recurse_rule_subplans(plan, self, ctx);
        changed |= nested.changed;
        Transformed {
            plan: nested.plan,
            changed,
        }
    }
}

fn rewrite_tree(tree: &mut JoinTree, cap: u32) -> bool {
    match tree {
        JoinTree::Unit | JoinTree::Paths(_) => false,
        JoinTree::WorstCaseOptimal {
            intersection,
            node_id_ordering,
        } => {
            if !node_id_ordering.is_empty() {
                return false;
            }
            let Some(first) = intersection.first() else {
                return false;
            };
            let Some(mut bindings) = wco_join::cycle_node_bindings(first, cap) else {
                return false;
            };
            bindings.sort();
            bindings.dedup();
            if bindings.len() < 2 {
                return false;
            }
            for pair in bindings.windows(2) {
                node_id_ordering.push(NodeIdOrdering {
                    left: pair[0],
                    ordering: Ordering::Less,
                    right: pair[1],
                });
            }
            true
        }
        JoinTree::Expand { child, .. } => rewrite_tree(child, cap),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            rewrite_tree(left, cap) | rewrite_tree(right, cap)
        }
        JoinTree::Scan(_) | JoinTree::Subplan(_) => false,
        // DisjunctiveScan branches are scan-shape leaves; no WCO under them.
        JoinTree::DisjunctiveScan { .. } => false,
    }
}
