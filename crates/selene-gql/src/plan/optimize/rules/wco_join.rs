//! WCO marker selection rule.

use std::collections::BTreeSet;

use crate::plan::{
    ExecutionPlan, JoinTree,
    optimize::{OptimizeContext, Rule, Transformed, walk},
};

/// Mark simple cyclic expand chains as future WCO execution candidates.
pub struct WcoJoin;

impl Rule for WcoJoin {
    fn name(&self) -> &'static str {
        "wco_join"
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
    if matches!(
        tree,
        JoinTree::Unit
            | JoinTree::Paths(_)
            | JoinTree::WorstCaseOptimal { .. }
            | JoinTree::Subplan(_)
    ) {
        return false;
    }
    if is_cyclic_expand_chain(tree, cap) {
        let original = std::mem::replace(
            tree,
            JoinTree::WorstCaseOptimal {
                intersection: Vec::new(),
                node_id_ordering: Vec::new(),
            },
        );
        *tree = JoinTree::WorstCaseOptimal {
            intersection: vec![original],
            node_id_ordering: Vec::new(),
        };
        return true;
    }
    match tree {
        JoinTree::Expand { child, .. } => rewrite_tree(child, cap),
        JoinTree::HashJoin { left, right, .. } => {
            rewrite_tree(left, cap) | rewrite_tree(right, cap)
        }
        JoinTree::Outer { left, .. } => rewrite_tree(left, cap),
        JoinTree::Unit
        | JoinTree::Scan(_)
        | JoinTree::Paths(_)
        | JoinTree::WorstCaseOptimal { .. }
        | JoinTree::Subplan(_) => false,
        // DisjunctiveScan is a scan-shape leaf; not a cyclic expand chain
        // and not a recursive container WCO can rewrite into.
        JoinTree::DisjunctiveScan { .. } => false,
    }
}

pub(super) fn is_cyclic_expand_chain(tree: &JoinTree, cap: u32) -> bool {
    let mut seen = BTreeSet::new();
    let mut expands = 0u32;
    detect_cycle(tree, cap, &mut expands, &mut seen).unwrap_or(false)
}

pub(super) fn cycle_node_bindings(tree: &JoinTree, cap: u32) -> Option<Vec<crate::BindingId>> {
    let mut seen = BTreeSet::new();
    let mut ordered = Vec::new();
    let mut expands = 0u32;
    collect_cycle_nodes(tree, cap, &mut expands, &mut seen, &mut ordered)?;
    Some(ordered)
}

fn detect_cycle(
    tree: &JoinTree,
    cap: u32,
    expands: &mut u32,
    seen: &mut BTreeSet<crate::BindingId>,
) -> Option<bool> {
    match tree {
        JoinTree::Unit => None,
        JoinTree::Scan(scan) => {
            if let Some(binding) = scan.binding {
                seen.insert(binding);
            }
            Some(false)
        }
        JoinTree::Expand { child, edge, .. } => {
            if !matches!(child.as_ref(), JoinTree::Scan(_) | JoinTree::Expand { .. }) {
                return None;
            }
            if detect_cycle(child, cap, expands, seen)? {
                return Some(true);
            }
            *expands = expands.saturating_add(1);
            if *expands > cap {
                return None;
            }
            if let Some(binding) = edge.right_binding {
                if seen.contains(&binding) {
                    return Some(true);
                }
                seen.insert(binding);
            }
            Some(false)
        }
        JoinTree::HashJoin { .. }
        | JoinTree::Outer { .. }
        | JoinTree::Paths(_)
        | JoinTree::WorstCaseOptimal { .. }
        | JoinTree::Subplan(_) => None,
        // DisjunctiveScan can't appear inside a cyclic expand chain — it
        // replaces a JoinTree::Scan leaf, and Expand requires its child be
        // either Scan or Expand (see line 109 above).
        JoinTree::DisjunctiveScan { .. } => None,
    }
}

fn collect_cycle_nodes(
    tree: &JoinTree,
    cap: u32,
    expands: &mut u32,
    seen: &mut BTreeSet<crate::BindingId>,
    ordered: &mut Vec<crate::BindingId>,
) -> Option<()> {
    match tree {
        JoinTree::Unit => None,
        JoinTree::Scan(scan) => {
            if let Some(binding) = scan.binding
                && seen.insert(binding)
            {
                ordered.push(binding);
            }
            Some(())
        }
        JoinTree::Expand { child, edge, .. } => {
            collect_cycle_nodes(child, cap, expands, seen, ordered)?;
            *expands = expands.saturating_add(1);
            if *expands > cap {
                return None;
            }
            if let Some(binding) = edge.right_binding
                && seen.insert(binding)
            {
                ordered.push(binding);
            }
            Some(())
        }
        JoinTree::HashJoin { .. }
        | JoinTree::Outer { .. }
        | JoinTree::Paths(_)
        | JoinTree::WorstCaseOptimal { .. }
        | JoinTree::Subplan(_) => None,
        // DisjunctiveScan can't appear inside the cycle the collector walks
        // (the chain is anchored on a Scan + Expand sequence per `detect_cycle`).
        JoinTree::DisjunctiveScan { .. } => None,
    }
}
