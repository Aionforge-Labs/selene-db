//! Pattern and scan summaries for optimizer snapshots.

use std::collections::BTreeMap;

use crate::{
    analyze::BindingId,
    plan::{
        BindingDef, EdgeMatch, FilterPredicate, JoinTree, NodeOrEdgeScan, OrderAccess, OrderKey,
        PipelineOp, ScanAccess, ScanKind,
    },
};

use super::{PatternSnapshot, PlanSnapshot, ScanSnapshot, bounds_detail::bounds_detail_for_access};

pub(super) fn pattern_snapshot(
    pattern: &crate::PatternPlan,
    pipeline: &[PipelineOp],
) -> PatternSnapshot {
    let bindings = binding_map(&pattern.bindings);
    let mut binding_names = bindings.values().cloned().collect::<Vec<_>>();
    binding_names.sort();
    binding_names.dedup();
    let mut scans = Vec::new();
    collect_scans(&pattern.join_tree, &bindings, &mut scans);
    PatternSnapshot {
        binding_count: pattern.bindings.len(),
        binding_names,
        join_tree_shape: join_tree_shape(&pattern.join_tree, &bindings),
        pattern_filter_count: pattern.filters.len(),
        scans,
        order_access: collect_order_access(pipeline),
    }
}

pub(super) fn join_tree_shape(tree: &JoinTree, bindings: &BTreeMap<BindingId, String>) -> String {
    match tree {
        JoinTree::Unit => "Unit".to_owned(),
        JoinTree::Scan(scan) => format!("Scan({})", binding_name(scan.binding, bindings, "_")),
        JoinTree::Expand { child, edge, .. } => format!(
            "{}->Expand({}->{})",
            join_tree_shape(child, bindings),
            binding_name(edge.binding, bindings, "_"),
            binding_name(edge.right_binding, bindings, "_")
        ),
        JoinTree::Paths(program) => format!("ProductPaths(patterns={})", program.automata.len()),
        JoinTree::HashJoin { left, right, .. } => format!(
            "HashJoin({}, {})",
            join_tree_shape(left, bindings),
            join_tree_shape(right, bindings)
        ),
        JoinTree::Outer { left, right, .. } => format!(
            "Outer({}, {})",
            join_tree_shape(left, bindings),
            join_tree_shape(right, bindings)
        ),
        JoinTree::WorstCaseOptimal {
            intersection,
            node_id_ordering,
        } => format!(
            "WCO(intersection={}, orderings={})",
            intersection.len(),
            node_id_ordering.len()
        ),
        JoinTree::Subplan(plan) => {
            format!(
                "Subplan({})",
                PlanSnapshot::from_plan(plan, Vec::new()).compact()
            )
        }
        JoinTree::DisjunctiveScan { branches, .. } => {
            format!("Disjunctive({} branches)", branches.len())
        }
    }
}

pub(super) fn binding_map(bindings: &[BindingDef]) -> BTreeMap<BindingId, String> {
    bindings
        .iter()
        .map(|binding| (binding.binding, binding.name.as_str().to_owned()))
        .collect()
}

pub(super) fn binding_refs(refs: &[BindingId], bindings: &BTreeMap<BindingId, String>) -> String {
    refs.iter()
        .map(|binding| {
            bindings
                .get(binding)
                .cloned()
                .unwrap_or_else(|| format!("#{}", binding.get()))
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn collect_order_access(pipeline: &[PipelineOp]) -> Vec<Option<String>> {
    let mut access = Vec::new();
    for op in pipeline {
        match op {
            PipelineOp::OrderBy(keys) => access.extend(keys.iter().map(order_access)),
            PipelineOp::TopK { keys, .. } => access.extend(keys.iter().map(order_access)),
            PipelineOp::Union { rhs, .. }
            | PipelineOp::Chain(rhs)
            | PipelineOp::CorrelatedChain(rhs) => {
                access.extend(collect_order_access(&rhs.pipeline));
            }
            PipelineOp::CallSubquery(call) => {
                access.extend(collect_order_access(&call.body.pipeline));
            }
            PipelineOp::ExplainPlan { inner, .. } => {
                access.extend(collect_order_access(&inner.pipeline));
            }
            PipelineOp::Filter(_)
            | PipelineOp::Project(_)
            | PipelineOp::Let(_)
            | PipelineOp::Unwind { .. }
            | PipelineOp::TrimOrderCarriers { .. }
            | PipelineOp::Limit { .. }
            | PipelineOp::GroupBy { .. }
            | PipelineOp::Distinct
            | PipelineOp::Match(_)
            | PipelineOp::OptionalMatch(_)
            | PipelineOp::Call(_)
            | PipelineOp::Mutation(_)
            | PipelineOp::Catalog(_)
            | PipelineOp::Tx(_)
            | PipelineOp::Session(_) => {}
        }
    }
    access
}

fn collect_scans(
    tree: &JoinTree,
    bindings: &BTreeMap<BindingId, String>,
    scans: &mut Vec<ScanSnapshot>,
) {
    match tree {
        JoinTree::Unit | JoinTree::Paths(_) => {}
        JoinTree::Scan(scan) => scans.push(scan_snapshot(scan, bindings)),
        JoinTree::Expand { child, edge, .. } => {
            collect_scans(child, bindings, scans);
            scans.push(edge_snapshot(edge, bindings));
            if !edge.right_property_predicates.is_empty() {
                scans.push(ScanSnapshot {
                    binding: binding_name(edge.right_binding, bindings, "<anonymous-node>"),
                    kind: "Node",
                    access: "Linear",
                    residual_predicates: edge.right_property_predicates.len(),
                    consumed_predicates: consumed_count(&edge.right_property_predicates),
                    bounds_detail: None,
                });
            }
        }
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            collect_scans(left, bindings, scans);
            collect_scans(right, bindings, scans);
        }
        JoinTree::WorstCaseOptimal { intersection, .. } => {
            for child in intersection {
                collect_scans(child, bindings, scans);
            }
        }
        JoinTree::Subplan(plan) => {
            if let Some(pattern) = &plan.pattern_plan {
                collect_scans(&pattern.join_tree, &binding_map(&pattern.bindings), scans);
            }
        }
        JoinTree::DisjunctiveScan { branches, .. } => {
            // Render one ScanSnapshot per branch so EXPLAIN exposes each
            // branch's independently-selected ScanAccess (acceptance bar
            // #1; Q5 transparent rendering).
            for branch in branches {
                scans.push(scan_snapshot(branch, bindings));
            }
        }
    }
}

fn scan_snapshot(scan: &NodeOrEdgeScan, bindings: &BTreeMap<BindingId, String>) -> ScanSnapshot {
    ScanSnapshot {
        binding: binding_name(scan.binding, bindings, "<anonymous>"),
        kind: scan_kind(scan.kind),
        access: scan_access(&scan.access),
        residual_predicates: scan.property_predicates.len(),
        consumed_predicates: consumed_count(&scan.property_predicates),
        bounds_detail: bounds_detail_for_access(&scan.access),
    }
}

fn edge_snapshot(edge: &EdgeMatch, bindings: &BTreeMap<BindingId, String>) -> ScanSnapshot {
    ScanSnapshot {
        binding: binding_name(edge.binding, bindings, "<anonymous-edge>"),
        kind: "Edge",
        access: scan_access(&edge.access),
        residual_predicates: edge.property_predicates.len(),
        consumed_predicates: consumed_count(&edge.property_predicates),
        bounds_detail: bounds_detail_for_access(&edge.access),
    }
}

fn binding_name(
    binding: Option<BindingId>,
    bindings: &BTreeMap<BindingId, String>,
    anonymous: &str,
) -> String {
    binding
        .and_then(|binding| bindings.get(&binding).cloned())
        .unwrap_or_else(|| anonymous.to_owned())
}

fn consumed_count(predicates: &[FilterPredicate]) -> usize {
    predicates
        .iter()
        .filter(|predicate| predicate.index_consumed)
        .count()
}

fn scan_kind(kind: ScanKind) -> &'static str {
    match kind {
        ScanKind::Node => "Node",
        ScanKind::Edge => "Edge",
    }
}

fn scan_access(access: &ScanAccess) -> &'static str {
    match access {
        ScanAccess::Linear => "Linear",
        ScanAccess::ExpressionLookup { .. } => "ExpressionLookup",
        ScanAccess::LabelIndex { .. } => "LabelIndex",
        ScanAccess::TypedIndexRange { .. } => "TypedIndexRange",
        ScanAccess::BitmapUnion { .. } => "BitmapUnion",
        ScanAccess::CompositeLookup { .. } => "CompositeLookup",
    }
}

fn order_access(key: &OrderKey) -> Option<String> {
    key.access.as_ref().map(|access| match access {
        OrderAccess::TypedIndex { direction, .. } => format!("TypedIndex({direction:?})"),
    })
}
