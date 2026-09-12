//! Disjunctive-label expansion rule (BRIEF-155).
//!
//! Rewrites a `JoinTree::Scan` whose `label_predicate` is a flat disjunction
//! of singles (`LabelExpr::Disjunction([Single, Single, …])`) into a
//! `JoinTree::DisjunctiveScan` whose per-label branches each carry
//! `Some(LabelExpr::Single(L_i))`. The downstream index-selection rules
//! (`composite_index_lookup` at slot 6, `in_list_optimization` at slot 7,
//! `range_index_scan` at slot 8) then walk into the branches and pick a
//! per-branch `ScanAccess` independently.
//!
//! Lands at slot 5 in `DEFAULT_RULES`, between `expand_filter_pushdown` and
//! `composite_index_lookup`. Cleanup rules (`constant_folding`,
//! `and_splitting`, `filter_pushdown`, `node_filter_extraction`,
//! `expand_filter_pushdown` at slots 0-4) run BEFORE this rule, so each
//! per-label branch sees its pushed-down property predicates on the first
//! index-rule pass — no per-branch cleanup needed.
//!
//! Gates:
//! - `scan.kind == ScanKind::Node` (F6 — no edge-index optimizer rule fires
//!   at HEAD, so edge-label disjunction expansion would be pure overhead).
//! - `scan.access == ScanAccess::Linear` (defense-in-depth; once a rule has
//!   picked an indexed access, expansion is moot).
//! - `flat_disjunction_singles` returns `Some(labels)` — the label
//!   predicate must be a flat disjunction with `Single` parts only. Mixed
//!   inner branches (`A|B&C`, `A|!B`) and idempotency cases (`Single(L)`)
//!   return `None` per the helper's contract.
//! - At least one per-label branch has an applicable typed, composite, or
//!   in-list index per the active catalog. Without index acceleration the
//!   expansion is pure overhead (N linear scans vs a single linear scan
//!   with post-filter), so the rule stays Linear in that case.

use selene_core::DbString;

use crate::{
    LabelExpr,
    plan::{
        BindingDef, ExecutionPlan, FilterPredicate, IndexCatalog, IndexTarget, JoinTree,
        NodeOrEdgeScan, ScanAccess, ScanKind,
        optimize::{OptimizeContext, Rule, Transformed, binding_refs, cost, walk},
    },
};

use super::index_helpers::{equality_candidates, flat_disjunction_singles};

/// Expand flat-disjunctive-label node scans into per-label
/// `JoinTree::DisjunctiveScan` branches when at least one branch has an
/// applicable index.
pub struct DisjunctiveLabelExpansion;

impl Rule for DisjunctiveLabelExpansion {
    fn name(&self) -> &'static str {
        "disjunctive_label_expansion"
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
            changed |= rewrite_tree(&mut pattern.join_tree, &pattern.bindings, catalog);
        }
        let nested = walk::recurse_rule_subplans(plan, self, ctx);
        changed |= nested.changed;
        Transformed {
            plan: nested.plan,
            changed,
        }
    }
}

fn rewrite_tree(tree: &mut JoinTree, bindings: &[BindingDef], catalog: &dyn IndexCatalog) -> bool {
    match tree {
        // The leaf — try to expand in place.
        JoinTree::Unit | JoinTree::Paths(_) => false,
        JoinTree::Scan(_) => maybe_expand_scan(tree, bindings, catalog),
        // Container shapes — recurse into children.
        JoinTree::Expand { child, .. } => rewrite_tree(child, bindings, catalog),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            rewrite_tree(left, bindings, catalog) | rewrite_tree(right, bindings, catalog)
        }
        // WCO sub-tree intersection elements are scans/expands; the WCO
        // rule (slot 11) runs after this one, so by the time we get here
        // the original `JoinTree::Scan` leaves haven't been reshaped.
        // Safe to skip — WCO operates on cycle-shaped expands, not
        // disjunctive label patterns.
        JoinTree::WorstCaseOptimal { .. } => false,
        // Subplans are walked via `walk::recurse_rule_subplans` in the
        // rule's `rewrite` impl.
        JoinTree::Subplan(_) => false,
        // Already expanded — the F3 idempotency contract on
        // `flat_disjunction_singles` keeps the rule from re-firing.
        JoinTree::DisjunctiveScan { .. } => false,
    }
}

fn maybe_expand_scan(
    tree: &mut JoinTree,
    bindings: &[BindingDef],
    catalog: &dyn IndexCatalog,
) -> bool {
    // Extract everything we need for the eligibility check by re-borrowing
    // the scan immutably first; this avoids holding a mutable borrow over
    // the catalog probes (which doesn't matter for soundness but keeps
    // the control flow obvious).
    let JoinTree::Scan(scan) = &*tree else {
        return false;
    };
    if scan.kind != ScanKind::Node {
        return false;
    }
    if !matches!(scan.access, ScanAccess::Linear) {
        return false;
    }
    let Some(labels) = flat_disjunction_singles(&scan.label_predicate) else {
        return false;
    };
    if !any_branch_has_applicable_index(&labels, &scan.property_predicates, bindings, catalog) {
        return false;
    }
    // OPT-5 cost gate: expand only when the summed per-label cardinality across
    // branches is strictly cheaper than a single full Linear scan (`total_rows`).
    // When stats are absent the gate is a no-op (keeps the structural decision),
    // so the plan matches pre-OPT-5 HEAD. Expansion never changes the row
    // multiset (each branch keeps its single-label predicate + residual
    // filters), so this only chooses between row-equivalent paths.
    if let (Some(expand_cost), Some(baseline)) = (
        cost::disjunctive_cost(catalog, IndexTarget::Node, &labels),
        catalog.total_rows(IndexTarget::Node),
    ) && cost::should_decline_index(expand_cost, baseline)
    {
        return false;
    }
    // Eligibility confirmed — promote to mutable to clone-and-replace.
    let JoinTree::Scan(scan) = tree else {
        unreachable!("just matched as Scan(_) above");
    };
    let original = scan.clone();
    let branches: Vec<NodeOrEdgeScan> = labels
        .iter()
        .map(|label| {
            let mut clone = original.clone();
            clone.label_predicate = Some(LabelExpr::Single(label.clone()));
            clone
        })
        .collect();
    let scan_anchor = original;
    *tree = JoinTree::DisjunctiveScan {
        branches,
        scan_anchor,
    };
    true
}

/// Return whether at least one per-label branch has an applicable typed,
/// composite, or in-list index against the scan's property predicates.
///
/// The probe is plan-time only — `IndexCatalog::{typed_index,
/// composite_index}` are O(1) hash lookups, so the cost is `O(N · P)`
/// where `N = labels.len()` and `P = predicate count`. ariadne's 13-label
/// case = 13 probes per cache-miss; amortized via the source-text plan
/// cache (BRIEF-114).
fn any_branch_has_applicable_index(
    labels: &[DbString],
    predicates: &[FilterPredicate],
    bindings: &[BindingDef],
    catalog: &dyn IndexCatalog,
) -> bool {
    let eq_candidates = equality_candidates(predicates, bindings);
    let eq_keys: Vec<DbString> = eq_candidates
        .iter()
        .map(|candidate| candidate.key.clone())
        .collect();
    labels.iter().any(|label| {
        // Single-property typed index — covers equality, comparison,
        // BETWEEN, and InList shapes uniformly: if a typed_index exists
        // for any property predicate key, at least one downstream rule
        // (range_index_scan / in_list_optimization) will fire on the
        // branch.
        for pred in predicates {
            let Some(matched) = binding_refs::match_property_predicate(pred, bindings) else {
                continue;
            };
            if catalog
                .typed_index(IndexTarget::Node, label.clone(), matched.key)
                .is_some()
            {
                return true;
            }
        }
        // Composite index — needs ≥ 2 equality candidates.
        if eq_keys.len() >= 2
            && catalog
                .composite_index(IndexTarget::Node, label.clone(), &eq_keys)
                .is_some()
        {
            return true;
        }
        false
    })
}

// Helper unit tests (`flat_disjunction_singles` shape coverage) live in
// `tests/optimize_disjunctive_label_expansion.rs` rather than inline here.
