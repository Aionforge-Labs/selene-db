//! Expression lowering helpers.

mod subqueries;

use crate::{
    SourceSpan, ValueExpr,
    analyze::{AnalyzedStatement, BindingId, ExprId},
    plan::{
        AggregateArg, FilterPredicate, FilterPredicateKind, OrderKey, PlannerError, ProjectExpr,
    },
};

pub(crate) use super::binding_refs::binding_refs_in;
pub(crate) use subqueries::{outer_binding_refs_in_span, populate_plan_subqueries};
pub(super) use subqueries::{outer_binding_uses_in_match, outer_binding_uses_in_span};

/// Build a planned projection expression.
pub(crate) fn project_expr(
    expr: &ValueExpr,
    alias: Option<selene_core::DbString>,
    analyzed: &AnalyzedStatement,
) -> Result<ProjectExpr, PlannerError> {
    let (expr_id, ty) = expr_cell(expr, analyzed)?;
    Ok(ProjectExpr {
        expr: expr.clone(),
        expr_id,
        ty,
        declared_type: None,
        alias,
        binding_refs: binding_refs_in(expr, analyzed)?,
        span: expr.span(),
    })
}

/// Build a planned filter predicate from a boolean expression.
pub(crate) fn filter_predicate(
    expr: &ValueExpr,
    analyzed: &AnalyzedStatement,
) -> Result<FilterPredicate, PlannerError> {
    let (expr_id, ty) = expr_cell(expr, analyzed)?;
    Ok(FilterPredicate {
        expr: expr.clone(),
        expr_id,
        ty,
        binding_refs: binding_refs_in(expr, analyzed)?,
        kind: FilterPredicateKind::Expression,
        index_consumed: false,
        span: expr.span(),
    })
}

/// Build a property-map equality predicate.
pub(crate) fn property_predicate(
    binding: Option<BindingId>,
    key: selene_core::DbString,
    value: &ValueExpr,
    analyzed: &AnalyzedStatement,
) -> Result<FilterPredicate, PlannerError> {
    let (expr_id, ty) = expr_cell(value, analyzed)?;
    let mut binding_refs = binding_refs_in(value, analyzed)?;
    if let Some(binding) = binding {
        ensure_binding_exists(binding, value.span(), analyzed)?;
        binding_refs.push(binding);
        binding_refs.sort();
        binding_refs.dedup();
    }
    Ok(FilterPredicate {
        expr: value.clone(),
        expr_id,
        ty,
        binding_refs,
        kind: FilterPredicateKind::PropertyEquals { binding, key },
        index_consumed: false,
        span: value.span(),
    })
}

/// Build a planned sort key.
pub(crate) fn order_key(
    term: &crate::OrderTerm,
    analyzed: &AnalyzedStatement,
) -> Result<OrderKey, PlannerError> {
    let (expr_id, ty) = expr_cell(&term.expr, analyzed)?;
    Ok(OrderKey {
        expr: term.expr.clone(),
        expr_id,
        ty,
        direction: term.direction,
        nulls: term.nulls,
        binding_refs: binding_refs_in(&term.expr, analyzed)?,
        access: None,
        span: term.span,
    })
}

/// Build a planned aggregate argument.
pub(crate) fn aggregate_arg(
    expr: &ValueExpr,
    analyzed: &AnalyzedStatement,
) -> Result<AggregateArg, PlannerError> {
    let (expr_id, ty) = expr_cell(expr, analyzed)?;
    Ok(AggregateArg {
        expr: expr.clone(),
        expr_id,
        ty,
    })
}

/// Return analyzer expression cell data.
pub(crate) fn expr_cell(
    expr: &ValueExpr,
    analyzed: &AnalyzedStatement,
) -> Result<(ExprId, crate::AnalyzedType), PlannerError> {
    let expr_id = analyzed
        .expression(expr)
        .ok_or(PlannerError::ExpressionTypeMissing { span: expr.span() })?
        .id;
    Ok((expr_id, analyzed.expr_types.get(expr_id).clone()))
}

fn ensure_binding_exists(
    binding: BindingId,
    span: SourceSpan,
    analyzed: &AnalyzedStatement,
) -> Result<(), PlannerError> {
    analyzed
        .scopes
        .declaration(binding)
        .map(|_| ())
        .ok_or(PlannerError::BindingResolutionLost { binding, span })
}

/// Aggregate function names recognised by the planner. Mirrors the parser
/// grammar's `aggregate_op` rule (lower-cased after `lowercase_db_string`). A scalar
/// function call with the same arity (e.g. `char_length(s)`) must not be lifted into
/// `PipelineOp::GroupBy.aggregates`, so this list — not arity — is the gate.
const AGGREGATE_NAMES: &[&str] = &[
    "stddev_samp",
    "stddev_pop",
    "collect_list",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "percentile_cont",
    "percentile_disc",
];

/// Return aggregate metadata when `expr` is a recognised aggregate call.
///
/// `count(*)` and `count(DISTINCT x)` reach the planner via the parser's
/// `aggregate_expr` rule with `star`/`distinct` set, while bare scalar function
/// calls keep both flags false. Either way, the name must appear in
/// [`AGGREGATE_NAMES`] for the planner to treat it as an aggregate.
pub(crate) fn aggregate_name(expr: &ValueExpr) -> Option<(selene_core::DbString, bool, bool)> {
    let ValueExpr::FunctionCall {
        name,
        star,
        distinct,
        ..
    } = expr
    else {
        return None;
    };
    if name.len() != 1 {
        return None;
    }
    let segment = name[0].clone();
    AGGREGATE_NAMES
        .iter()
        .any(|candidate| segment.as_str() == *candidate)
        .then_some((segment, *star, *distinct))
}
