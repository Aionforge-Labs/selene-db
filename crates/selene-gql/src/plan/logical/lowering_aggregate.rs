//! Aggregate helpers for logical lowering.
//!
//! Aggregate names mirror the parser's `aggregate_op` rule so scalar calls
//! with the same arity never lift into grouping. All identities resolve from
//! the frozen semantic tree.

use selene_core::{DbString, db_string};

use crate::{
    SourceSpan, ValueExpr,
    analyze::{AnalyzedStatement, ExprId},
    plan::{PlannerError, logical::descriptors::LogicalAggregate},
};

/// Aggregate function names recognised by the planner.
///
/// Mirrors the parser grammar's `aggregate_op` rule (lower-cased). A scalar
/// call with the same arity (for example, `char_length(s)`) must not lift
/// into grouping, so this list — not arity — is the gate.
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
pub(crate) fn aggregate_name(expr: &ValueExpr) -> Option<(DbString, bool, bool)> {
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

/// Collect aggregate applications from return items and an optional HAVING.
pub(crate) fn collect_aggregates(
    items: &[crate::ReturnItem],
    having: Option<&ValueExpr>,
    analyzed: &AnalyzedStatement,
) -> Result<Vec<LogicalAggregate>, PlannerError> {
    let mut out = Vec::new();
    let mut seen: std::collections::BTreeSet<ExprId> = std::collections::BTreeSet::new();
    for item in items {
        collect_aggregates_in_expr(&item.expr, analyzed, &mut out, &mut seen)?;
    }
    if let Some(having) = having {
        collect_aggregates_in_expr(having, analyzed, &mut out, &mut seen)?;
    }
    Ok(out)
}

fn collect_aggregates_in_expr(
    value: &ValueExpr,
    analyzed: &AnalyzedStatement,
    out: &mut Vec<LogicalAggregate>,
    seen: &mut std::collections::BTreeSet<ExprId>,
) -> Result<(), PlannerError> {
    if let Some((function, star, distinct)) = aggregate_name(value) {
        let (aggregate_id, ty) = super::lowering_query::expr_cell(value, analyzed)?;
        if seen.insert(aggregate_id) {
            let output_name = synthesized_aggregate_name(aggregate_id, value.span())?;
            let args = match value {
                ValueExpr::FunctionCall { args, .. } => args
                    .iter()
                    .map(|arg| super::lowering_query::expr_cell(arg, analyzed).map(|(id, _)| id))
                    .collect::<Result<Vec<_>, _>>()?,
                _ => Vec::new(),
            };
            out.push(LogicalAggregate {
                aggregate_id,
                function,
                star,
                distinct,
                args,
                output_name,
                ty,
                origin: value.span(),
            });
        }
        return Ok(());
    }
    match value {
        ValueExpr::Literal(_) | ValueExpr::Variable { .. } | ValueExpr::Parameter { .. } => Ok(()),
        ValueExpr::Exists { .. } | ValueExpr::ValueSubquery { .. } => Ok(()),
        _ => {
            let mut result = Ok(());
            value.for_each_child(&mut |child| {
                if result.is_ok() {
                    result = collect_aggregates_in_expr(child, analyzed, out, seen);
                }
            });
            result
        }
    }
}

/// Synthesize the deterministic `agg_<id>` output column name.
pub(crate) fn synthesized_aggregate_name(
    expr_id: ExprId,
    span: SourceSpan,
) -> Result<DbString, PlannerError> {
    let name = format!("agg_{}", expr_id.get());
    db_string(&name).map_err(|_err| PlannerError::StaticStringConstructionFailed {
        detail: "aggregate synthesized column",
        span,
    })
}
