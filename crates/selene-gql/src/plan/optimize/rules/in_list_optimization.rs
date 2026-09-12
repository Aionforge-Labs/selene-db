//! Small `IN` list typed-index optimization.

use crate::{
    EdgeMatch,
    plan::{
        BindingDef, BindingElement, ExecutionPlan, IndexKey, IndexTarget, JoinTree, ScanAccess,
        ScanKind,
        optimize::{OptimizeContext, Rule, Transformed, binding_refs, cost, walk},
    },
};

use super::index_helpers::{compatible_list_parameter, compatible_value, single_label};

const SMALL_IN_LIST_LIMIT: usize = 16;

/// Rewrite small property `IN` lists to bitmap-union access.
pub struct InListOptimization;

impl Rule for InListOptimization {
    fn name(&self) -> &'static str {
        "in_list_optimization"
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
        JoinTree::Expand { child, edge, .. } => {
            rewrite_tree(child, bindings, catalog) | rewrite_edge(edge, bindings, catalog)
        }
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            rewrite_tree(left, bindings, catalog) | rewrite_tree(right, bindings, catalog)
        }
        JoinTree::WorstCaseOptimal { .. } | JoinTree::Subplan(_) => false,
        // Walk each per-label branch; the disjunctive_label_expansion rule
        // runs at slot 5 (before this rule), so DisjunctiveScan only carries
        // single-label branches by the time we get here.
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
    if !matches!(scan.access, ScanAccess::Linear) {
        return false;
    }
    let Some(label) = single_label(&scan.label_predicate) else {
        return false;
    };
    let target = target_for_scan_kind(scan.kind);
    rewrite_access(
        &mut scan.access,
        &mut scan.property_predicates,
        bindings,
        catalog,
        target,
        label,
    )
}

fn rewrite_edge(
    edge: &mut EdgeMatch,
    bindings: &[BindingDef],
    catalog: &dyn crate::IndexCatalog,
) -> bool {
    if !matches!(edge.access, ScanAccess::Linear) {
        return false;
    }
    let Some(label) = single_label(&edge.label_predicate) else {
        return false;
    };
    rewrite_access(
        &mut edge.access,
        &mut edge.property_predicates,
        bindings,
        catalog,
        IndexTarget::Edge,
        label,
    )
}

fn rewrite_access(
    access: &mut ScanAccess,
    predicates: &mut Vec<crate::FilterPredicate>,
    bindings: &[BindingDef],
    catalog: &dyn crate::IndexCatalog,
    target: IndexTarget,
    label: selene_core::DbString,
) -> bool {
    for index in 0..predicates.len() {
        let pred = &predicates[index];
        let Some(matched) = binding_refs::match_property_predicate(pred, bindings) else {
            continue;
        };
        if !binding_is_target(bindings, matched.binding, target) {
            continue;
        }
        let Some(lookup) = catalog.typed_index(target, label.clone(), matched.key.clone()) else {
            continue;
        };
        let property = matched.key;
        if let binding_refs::PropertyPredicateShape::InListExpression(list_expr) = matched.shape {
            let Some(key) = compatible_list_parameter(list_expr, lookup.kind) else {
                continue;
            };
            predicates.remove(index);
            *access = ScanAccess::BitmapUnion {
                handle: lookup.handle,
                property,
                kind: lookup.kind,
                keys: vec![key],
            };
            return true;
        }
        let binding_refs::PropertyPredicateShape::InList(items) = matched.shape else {
            continue;
        };
        if items.is_empty() || items.len() > SMALL_IN_LIST_LIMIT {
            continue;
        }
        let mut keys = Vec::with_capacity(items.len());
        let mut has_literal = false;
        let mut has_parameter = false;
        let mut all_match = true;
        for item in items {
            let Some(index_key) = compatible_value(item, lookup.kind) else {
                all_match = false;
                break;
            };
            match &index_key {
                IndexKey::Literal(_) => has_literal = true,
                IndexKey::Parameter { .. } => has_parameter = true,
                IndexKey::ParameterList { .. } => {}
            }
            keys.push(index_key);
        }
        // BRIEF-154 Q3: keep the list homogeneous in v1.1. All-literal or
        // all-parameter InLists fire BitmapUnion; mixed lists fall back to
        // Linear (the runtime would otherwise need per-key dispatch and the
        // brief defers that to a future revisit).
        if !all_match || (has_literal && has_parameter) {
            continue;
        }
        // OPT-5 cost gate: take the bitmap-union only when the summed per-key
        // cardinality is strictly cheaper than the residual baseline. Absent
        // stats → no-op (keeps the structural decision). The union returns the
        // same rows the residual `IN`-list filter would, so results are
        // identical regardless of path.
        if let (Some(index_cost), Some(baseline)) = (
            cost::in_list_cost(catalog, target, label.clone(), property.clone(), &keys),
            cost::linear_baseline(catalog, target, label.clone()),
        ) && cost::should_decline_index(index_cost, baseline)
        {
            continue;
        }
        predicates.remove(index);
        *access = ScanAccess::BitmapUnion {
            handle: lookup.handle,
            property,
            kind: lookup.kind,
            keys,
        };
        return true;
    }
    false
}

fn target_for_scan_kind(kind: ScanKind) -> IndexTarget {
    match kind {
        ScanKind::Node => IndexTarget::Node,
        ScanKind::Edge => IndexTarget::Edge,
    }
}

fn binding_is_target(
    bindings: &[BindingDef],
    binding_id: crate::BindingId,
    target: IndexTarget,
) -> bool {
    let element = match target {
        IndexTarget::Node => BindingElement::Node,
        IndexTarget::Edge => BindingElement::Edge,
    };
    bindings
        .iter()
        .any(|binding| binding.binding == binding_id && binding.element == element)
}
