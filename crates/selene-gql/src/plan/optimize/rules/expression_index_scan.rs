//! Conservative expression equality access. Error equivalence outranks selectivity.

use crate::plan::{
    ExecutionPlan,
    optimize::{OptimizeContext, cost},
};
use crate::{BinaryOp, FilterPredicateKind, JoinTree, ScanAccess, ScanKind, ValueExpr};

pub(super) fn rewrite(plan: &mut ExecutionPlan, ctx: &OptimizeContext<'_>) -> bool {
    let (Some(catalog), Some(analyzed), Some(pattern)) =
        (ctx.index_catalog, ctx.analyzed, plan.pattern_plan.as_mut())
    else {
        return false;
    };
    // Other scans, prefilters and sibling predicates can expose errors on rows
    // this expression would discard. Do not speculate across those boundaries.
    if !pattern.filters.is_empty() {
        return false;
    }
    let JoinTree::Scan(scan) = &mut pattern.join_tree else {
        return false;
    };
    if scan.kind != ScanKind::Node || !matches!(scan.access, ScanAccess::Linear) {
        return false;
    }
    let Some(label) = super::index_helpers::single_label(&scan.label_predicate) else {
        return false;
    };
    let [predicate] = scan.property_predicates.as_slice() else {
        return false;
    };
    if predicate.kind != FilterPredicateKind::Expression {
        return false;
    }
    let ValueExpr::BinaryOp {
        op: BinaryOp::Eq,
        lhs,
        rhs,
        ..
    } = &predicate.expr
    else {
        return false;
    };
    let (source, literal) = match (lhs.as_ref(), rhs.as_ref()) {
        (source, ValueExpr::Literal(literal)) | (ValueExpr::Literal(literal), source) => {
            (source, literal)
        }
        _ => return false,
    };
    let Some((binding, expression)) =
        crate::analyze::index_expression::from_source(analyzed, source)
    else {
        return false;
    };
    if Some(binding) != scan.binding || expression.operations.is_empty() {
        return false;
    }
    let Some(value) = cost::literal_to_value(literal) else {
        return false;
    };
    let Some((lookup, count)) = catalog.expression_index(&label, &expression, &value) else {
        return false;
    };
    if !super::index_helpers::literal_matches_kind(literal, lookup.kind) {
        return false;
    }
    if catalog
        .label_cardinality(crate::IndexTarget::Node, label)
        .is_some_and(|rows| count >= rows)
    {
        return false;
    }
    scan.access = ScanAccess::ExpressionLookup {
        handle: lookup.handle,
        expression,
        value,
    };
    true
}
