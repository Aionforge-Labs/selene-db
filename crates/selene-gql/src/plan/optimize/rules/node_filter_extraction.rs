//! Pattern-level node filter extraction.

use crate::plan::{
    BindingElement, ExecutionPlan, FilterPredicate, JoinTree, ScanKind,
    optimize::{OptimizeContext, Rule, Transformed, walk},
};

/// Move single-node-binding filters onto the matching node scan.
pub struct NodeFilterExtraction;

impl Rule for NodeFilterExtraction {
    fn name(&self) -> &'static str {
        "node_filter_extraction"
    }

    fn rewrite(
        &self,
        mut plan: ExecutionPlan,
        ctx: &OptimizeContext<'_>,
    ) -> Transformed<ExecutionPlan> {
        let mut changed = false;
        if let Some(pattern) = &mut plan.pattern_plan {
            changed |= extract_node_filters(pattern);
        }
        let nested = walk::recurse_rule_subplans(plan, self, ctx);
        changed |= nested.changed;
        Transformed {
            plan: nested.plan,
            changed,
        }
    }
}

fn extract_node_filters(pattern: &mut crate::PatternPlan) -> bool {
    let mut changed = false;
    let mut remaining = Vec::with_capacity(pattern.filters.len());
    for pred in pattern.filters.drain(..) {
        let Some(binding) = single_node_binding(pattern.bindings.as_slice(), &pred) else {
            remaining.push(pred);
            continue;
        };
        let mut pending = Some(pred);
        if push_to_node_scan_inner(&mut pattern.join_tree, binding, &mut pending) {
            changed = true;
        } else {
            remaining.push(pending.expect("predicate remains when not pushed"));
        }
    }
    pattern.filters = remaining;
    changed
}

fn single_node_binding(
    bindings: &[crate::BindingDef],
    pred: &FilterPredicate,
) -> Option<crate::BindingId> {
    let [binding] = pred.binding_refs.as_slice() else {
        return None;
    };
    bindings
        .iter()
        .any(|def| def.binding == *binding && def.element == BindingElement::Node)
        .then_some(*binding)
}

fn push_to_node_scan_inner(
    tree: &mut JoinTree,
    binding: crate::BindingId,
    pending: &mut Option<FilterPredicate>,
) -> bool {
    match tree {
        JoinTree::Scan(scan)
            if scan.kind == ScanKind::Node
                && scan.binding == Some(binding)
                && pending.is_some() =>
        {
            scan.property_predicates
                .push(pending.take().expect("pending predicate exists"));
            true
        }
        JoinTree::Unit
        | JoinTree::Scan(_)
        | JoinTree::Paths(_)
        | JoinTree::WorstCaseOptimal { .. }
        | JoinTree::Subplan(_) => false,
        JoinTree::Expand { child, .. } => push_to_node_scan_inner(child, binding, pending),
        JoinTree::HashJoin { left, right, .. } => {
            push_to_node_scan_inner(left, binding, pending)
                || push_to_node_scan_inner(right, binding, pending)
        }
        JoinTree::Outer { left, .. } => push_to_node_scan_inner(left, binding, pending),
        // node_filter_extraction runs at slot 3 (before disjunctive_label_expansion
        // at slot 5), so DisjunctiveScan never appears in this rule's traversal in
        // practice. Defensive `false` matches WorstCaseOptimal arm semantics: the
        // pushdown stops at this leaf shape rather than rewriting per-branch.
        JoinTree::DisjunctiveScan { .. } => false,
    }
}
