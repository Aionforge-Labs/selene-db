//! Single-label scan -> label-index access rule.
//!
//! Rewrites a bare single-label scan (`MATCH (n:Label)` / `()-[e:LABEL]->()`)
//! from [`ScanAccess::Linear`] to [`ScanAccess::LabelIndex`] so the runtime
//! enumerates candidate rows via the intrinsic RoaringBitmap label index
//! rather than scanning every row and post-filtering by label.

use crate::{
    IndexTarget,
    plan::{
        ExecutionPlan, JoinTree, ScanAccess, ScanKind,
        optimize::{OptimizeContext, Rule, Transformed, cost, walk},
    },
};

use super::index_helpers::single_label;

/// Promote a single-label `Linear` scan to label-index access.
///
/// # Gates and ordering
///
/// The rule fires only when the scan's access is still
/// [`ScanAccess::Linear`], its `label_predicate` is a single label, and the
/// catalog reports a label index for `(target, label)`. It must run **after**
/// the property-index rules (`composite_index_lookup`, `in_list_optimization`,
/// `range_index_scan`, `index_order`): those select a strictly-more-selective
/// `TypedIndexRange` / `BitmapUnion` / `CompositeLookup` and require
/// `ScanAccess::Linear` to fire, so the `matches!(access, Linear)` gate here
/// guarantees this rule only claims scans no property-index rule wanted — the
/// bare `(n:Label)` and `(n:Label)`-with-no-indexed-predicate cases. The label
/// predicate is left in place so the executor's residual post-filter still
/// runs (defense-in-depth; for a single-label bitmap it is a no-op).
///
/// # Composition with disjunctive-label expansion (BRIEF-155)
///
/// `disjunctive_label_expansion` (slot 6) splits `(n:A|B|C)` into per-label
/// branches under `JoinTree::DisjunctiveScan`, each with a single-label
/// predicate. This rule recurses into those branches, so each per-label
/// branch independently picks up label-index access. A non-expanded
/// disjunction keeps a `Disjunction` predicate, for which `single_label`
/// returns `None`, so it correctly stays `Linear`.
///
/// # Idempotency
///
/// A second pass sees `access != Linear` and returns the scan unchanged, so
/// the rule converges in one extra iteration.
pub struct LabelScan;

impl Rule for LabelScan {
    fn name(&self) -> &'static str {
        "label_scan"
    }

    fn rewrite(
        &self,
        mut plan: ExecutionPlan,
        ctx: &OptimizeContext<'_>,
    ) -> Transformed<ExecutionPlan> {
        let Some(catalog) = ctx.index_catalog else {
            return Transformed::unchanged(plan);
        };
        let mut changed = false;
        if let Some(pattern) = &mut plan.pattern_plan {
            changed |= rewrite_tree(&mut pattern.join_tree, catalog);
        }
        let nested = walk::recurse_rule_subplans(plan, self, ctx);
        changed |= nested.changed;
        Transformed {
            plan: nested.plan,
            changed,
        }
    }
}

fn rewrite_tree(tree: &mut JoinTree, catalog: &dyn crate::IndexCatalog) -> bool {
    match tree {
        JoinTree::Unit | JoinTree::Paths(_) => false,
        JoinTree::Scan(scan) => rewrite_scan(scan, catalog),
        JoinTree::Expand { child, .. } => rewrite_tree(child, catalog),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            rewrite_tree(left, catalog) | rewrite_tree(right, catalog)
        }
        JoinTree::WorstCaseOptimal { .. } | JoinTree::Subplan(_) => false,
        // Each per-label branch (post disjunctive_label_expansion) is a leaf
        // single-label scan; promote each independently.
        JoinTree::DisjunctiveScan { branches, .. } => {
            branches.iter_mut().fold(false, |changed, branch| {
                rewrite_scan(branch, catalog) | changed
            })
        }
    }
}

fn rewrite_scan(scan: &mut crate::NodeOrEdgeScan, catalog: &dyn crate::IndexCatalog) -> bool {
    // Only fire when no stronger access was already selected by an earlier rule.
    if !matches!(scan.access, ScanAccess::Linear) {
        return false;
    }
    let Some(label) = single_label(&scan.label_predicate) else {
        return false;
    };
    let target = match scan.kind {
        ScanKind::Node => IndexTarget::Node,
        ScanKind::Edge => IndexTarget::Edge,
    };
    let Some(handle) = catalog.label_index(target, label.clone()) else {
        return false;
    };
    // OPT-5 cost gate: promote to LabelIndex only when the label bitmap is
    // strictly smaller than the full-scan baseline (`total_rows`). The Linear
    // alternative here is a *full* scan, so the baseline is the total row count,
    // not the label-scoped residual baseline. When stats are absent (either
    // estimator returns None) keep the structural decision (promote) so the plan
    // is byte-identical to pre-OPT-5 HEAD. The label predicate stays on the IR
    // for transparency; at execution time the LabelIndex candidate source is
    // already label-filtered, so scan collection can avoid the duplicate check.
    if let (Some(index_cost), Some(baseline)) = (
        cost::label_scan_cost(catalog, target, label),
        catalog.total_rows(target),
    ) && cost::should_decline_index(index_cost, baseline)
    {
        return false;
    }
    scan.access = ScanAccess::LabelIndex { handle };
    true
}
