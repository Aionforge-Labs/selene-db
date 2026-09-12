//! Composite index lookup rule.

use selene_core::DbString;

use crate::plan::{
    BindingDef, ExecutionPlan, IndexKey, IndexTarget, JoinTree, ScanAccess, ScanKind,
    optimize::{OptimizeContext, Rule, Transformed, cost, walk},
};

use super::index_helpers::{
    EqualityCandidate, compatible_value, equality_candidates, single_label,
};

/// Rewrite multi-property equality predicates to composite index access.
///
/// # Duplicate property-key handling
///
/// When multiple equality predicates share the same property key, the first
/// candidate per key is chosen for the index lookup and every other predicate
/// on the same key remains in the residual filter regardless of literal value.
/// Conflicting duplicates therefore produce an empty result via residual
/// rejection, while exact duplicates leave a redundant residual predicate for a
/// future cleanup rule.
pub struct CompositeIndexLookup;

const MAX_COMPOSITE_CANDIDATES: usize = 16;

impl Rule for CompositeIndexLookup {
    fn name(&self) -> &'static str {
        "composite_index_lookup"
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

fn rewrite_tree(
    tree: &mut JoinTree,
    bindings: &[BindingDef],
    catalog: &dyn crate::IndexCatalog,
) -> bool {
    match tree {
        JoinTree::Unit | JoinTree::Paths(_) => false,
        JoinTree::Scan(scan) => rewrite_scan(scan, bindings, catalog),
        JoinTree::Expand { child, .. } => rewrite_tree(child, bindings, catalog),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            rewrite_tree(left, bindings, catalog) | rewrite_tree(right, bindings, catalog)
        }
        JoinTree::WorstCaseOptimal { .. } | JoinTree::Subplan(_) => false,
        // Walk each per-label branch; downstream index rules apply
        // independently per branch (the rule that emits DisjunctiveScan
        // runs at slot 5 — before this rule at slot 6).
        JoinTree::DisjunctiveScan { branches, .. } => {
            branches.iter_mut().fold(false, |changed, branch| {
                rewrite_scan(branch, bindings, catalog) | changed
            })
        }
    }
}

fn rewrite_scan(
    scan: &mut crate::NodeOrEdgeScan,
    bindings: &[BindingDef],
    catalog: &dyn crate::IndexCatalog,
) -> bool {
    if scan.kind != ScanKind::Node || !matches!(scan.access, ScanAccess::Linear) {
        return false;
    }
    let Some(label) = single_label(&scan.label_predicate) else {
        return false;
    };
    let candidates = equality_candidates(&scan.property_predicates, bindings);
    if candidates.len() < 2 {
        return false;
    }
    // Collect every matching composite index in the deterministic
    // largest-subset / ascending-mask order, then (OPT-5) cost-rank them by
    // estimated composite cardinality, cheapest first. The deterministic
    // collection order is the tie-break for equal-cost matches, so plans stay
    // stable. Without stats, the first match in collection order wins —
    // byte-identical to the pre-OPT-5 largest-subset-first heuristic.
    let matches = find_composite_matches(&candidates, label.clone(), catalog);
    let mut best: Option<SelectedComposite> = None;
    for (composite, consumed_indices) in matches {
        // BRIEF-154 §B.2 F7: resolve each component value against its per-column
        // IndexKind. Literals get plan-time kind-checked via `compatible_value`'s
        // literal branch; typed-declared parameters get gated by
        // `gql_type_compatible_with_index_kind`. Any per-component failure
        // (mismatched literal kind, typed-incompatible parameter) skips this
        // candidate (it cannot be probed) and tries the next.
        let mut keys = Vec::with_capacity(composite.properties.len());
        let mut admissible = true;
        for (property, kind) in &composite.properties {
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.key == *property)
                .expect("matched property is present in candidate set");
            let Some(index_key) = compatible_value(candidate.value, *kind) else {
                admissible = false;
                break;
            };
            keys.push((property.clone(), index_key));
        }
        if !admissible {
            continue;
        }
        // OPT-5 cost estimate (None when stats absent → ranks last so the
        // deterministic collection order is preserved).
        let probe_keys: Vec<IndexKey> = keys.iter().map(|(_, k)| k.clone()).collect();
        let property_keys: Vec<DbString> = composite
            .properties
            .iter()
            .map(|(p, _)| p.clone())
            .collect();
        let estimated = cost::composite_cost(
            catalog,
            IndexTarget::Node,
            label.clone(),
            &property_keys,
            &probe_keys,
        );
        let selected = SelectedComposite {
            handle: composite.handle,
            properties: composite.properties,
            keys,
            consumed_indices,
            cost: estimated,
        };
        best = Some(match best {
            Some(current) if !is_cheaper(&selected, &current) => current,
            _ => selected,
        });
    }
    let Some(selected) = best else {
        return false;
    };
    // OPT-5 index-vs-baseline gate: take the composite lookup only when it is
    // strictly cheaper than the residual baseline. Absent stats → no-op.
    if let (Some(index_cost), Some(baseline)) = (
        selected.cost,
        cost::linear_baseline(catalog, IndexTarget::Node, label),
    ) && cost::should_decline_index(index_cost, baseline)
    {
        return false;
    }
    let mut consumed_indices = selected.consumed_indices;
    consumed_indices.sort_unstable();
    consumed_indices.dedup();
    remove_indices(&mut scan.property_predicates, &consumed_indices);
    scan.access = ScanAccess::CompositeLookup {
        handle: selected.handle,
        properties: selected.properties,
        keys: selected.keys,
    };
    true
}

/// A composite-index candidate with its resolved probe keys and estimated cost.
struct SelectedComposite {
    handle: crate::IndexHandle,
    properties: Vec<(DbString, crate::IndexKind)>,
    keys: Vec<(DbString, IndexKey)>,
    consumed_indices: Vec<usize>,
    /// Estimated output rows; `None` when no statistic is available.
    cost: Option<u64>,
}

/// Whether `candidate` should replace `current` as the best composite match.
///
/// A candidate with a known cost beats one with a lower cost; a candidate
/// without a cost never displaces an existing pick (so the deterministic
/// collection order — the first match — wins when stats are absent or equal).
fn is_cheaper(candidate: &SelectedComposite, current: &SelectedComposite) -> bool {
    match (candidate.cost, current.cost) {
        (Some(c), Some(cur)) => c < cur,
        // Candidate has no stat: never displaces (keep collection-order winner).
        (None, _) => false,
        // Current has no stat but candidate does: only meaningful when the
        // collection order put the no-stat one first; keep it (deterministic).
        (Some(_), None) => false,
    }
}

/// Enumerate every matching composite index in deterministic order: largest
/// subset first, and within each size masks in ascending order. Each entry
/// pairs the matched composite handle/metadata with the
/// candidate-`property_predicates` indices it consumes.
///
/// Pre-OPT-5 the rule took the *first* entry (pure largest-subset-first
/// heuristic). OPT-5 now cost-ranks all entries and uses this collection order
/// only as the tie-break, so equal-cardinality matches (and the no-stats case)
/// remain byte-identical to the old behavior.
fn find_composite_matches(
    candidates: &[EqualityCandidate],
    label: DbString,
    catalog: &dyn crate::IndexCatalog,
) -> Vec<(crate::CompositeIndexHandle, Vec<usize>)> {
    let n = candidates.len();
    if n > MAX_COMPOSITE_CANDIDATES {
        return Vec::new();
    }
    let mut matches = Vec::new();
    // From largest subset down to size 2; within each size, iterate masks in
    // ascending order for deterministic plans when multiple indexes match.
    for size in (2..=n).rev() {
        let mut mask = (1u64 << size) - 1;
        while mask < (1u64 << n) {
            let subset_keys: Vec<DbString> = (0..n)
                .filter(|i| (mask >> i) & 1 == 1)
                .map(|i| candidates[i].key.clone())
                .collect();
            if let Some(composite) =
                catalog.composite_index(crate::IndexTarget::Node, label.clone(), &subset_keys)
            {
                let consumed: Vec<usize> = composite
                    .properties
                    .iter()
                    .map(|(property, _kind)| {
                        candidates
                            .iter()
                            .find(|candidate| candidate.key == *property)
                            .map(|candidate| candidate.index)
                            .expect("matched property in candidate set")
                    })
                    .collect();
                matches.push((composite, consumed));
            }
            // Gosper's hack: next mask with the same popcount, lexicographically.
            let c = mask & mask.wrapping_neg();
            let r = mask + c;
            mask = (((r ^ mask) >> 2) / c) | r;
        }
    }
    matches
}

fn remove_indices(predicates: &mut Vec<crate::FilterPredicate>, indices: &[usize]) {
    let mut cursor = 0usize;
    predicates.retain(|_| {
        let remove = indices.binary_search(&cursor).is_ok();
        cursor += 1;
        !remove
    });
}
