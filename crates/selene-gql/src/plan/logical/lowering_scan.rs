//! Scan and binding helpers for logical lowering.
//!
//! All identities resolve from the frozen semantic tree; source syntax
//! supplies only spans. No helper here mutates source or rederives types.

use crate::{
    ExistsBody, SourceSpan, ValueExpr,
    analyze::{AnalyzedStatement, BindingId},
    plan::PlannerError,
};

/// Bindings referenced by `expr`, sorted and deduplicated.
///
/// Mirrors the row-plan adapter's namespace discipline: a `Variable` resolves
/// through its semantic node (never a parameter lookup), and `EXISTS` /
/// value-subquery bodies contribute their outer-binding uses.
pub(crate) fn binding_refs_in(
    expr: &ValueExpr,
    analyzed: &AnalyzedStatement,
) -> Result<Vec<BindingId>, PlannerError> {
    let mut refs = Vec::new();
    collect_binding_refs_in_expr(expr, analyzed, &mut refs)?;
    refs.sort_by_key(|(binding, _)| *binding);
    refs.dedup_by_key(|(binding, _)| *binding);
    for (binding, span) in &refs {
        ensure_binding_exists(*binding, *span, analyzed)?;
    }
    Ok(refs.into_iter().map(|(binding, _)| binding).collect())
}

/// Resolve one binding name to its semantic identity, when declared.
pub(crate) fn resolve_binding(
    name: &selene_core::DbString,
    analyzed: &AnalyzedStatement,
) -> Option<BindingId> {
    analyzed
        .scopes
        .declarations()
        .iter()
        .find(|decl| decl.name() == *name)
        .map(|decl| decl.id())
}

/// Ensure a semantic binding still resolves during lowering.
pub(crate) fn ensure_binding_exists(
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

fn collect_binding_refs_in_expr(
    expr: &ValueExpr,
    analyzed: &AnalyzedStatement,
    refs: &mut Vec<(BindingId, SourceSpan)>,
) -> Result<(), PlannerError> {
    match expr {
        ValueExpr::Variable { span, .. } => {
            let node = analyzed
                .expression(expr)
                .ok_or(PlannerError::ExpressionTypeMissing { span: *span })?;
            let crate::analyze::semantic::ExpressionKind::Binding(binding) = node.kind else {
                return Err(PlannerError::ExpressionTypeMissing { span: *span });
            };
            refs.push((binding, *span));
        }
        ValueExpr::Exists { body, span, .. } => match body {
            ExistsBody::Match(pattern) => {
                for (binding, _, span) in outer_uses_in_match(pattern, *span, analyzed)? {
                    refs.push((binding, span));
                }
            }
            ExistsBody::Query(pipeline) => {
                for (binding, _, span) in outer_uses_in_span(pipeline.span, analyzed)? {
                    refs.push((binding, span));
                }
            }
        },
        ValueExpr::ValueSubquery { body, .. } => {
            for (binding, _, span) in outer_uses_in_span(body.span, analyzed)? {
                refs.push((binding, span));
            }
        }
        _ => {
            let mut result = Ok(());
            expr.for_each_child(&mut |child| {
                if result.is_ok() {
                    result = collect_binding_refs_in_expr(child, analyzed, refs);
                }
            });
            result?;
        }
    }
    Ok(())
}

/// Outer binding identities referenced inside `span`.
///
/// The analyzer records every reference with its resolved identity and span;
/// a use inside `span` whose declaration sits outside it is an outer
/// reference (the same rule the row adapter uses for `CALL` subquery imports
/// and sort-carrier analysis).
pub(crate) fn outer_refs_in_span(span: SourceSpan, analyzed: &AnalyzedStatement) -> Vec<BindingId> {
    outer_uses_in_span(span, analyzed)
        .map(|uses| {
            uses.into_iter()
                .map(|(binding, _, _)| binding)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// Outer-binding uses whose declaration lives outside `span`.
///
/// The analyzer records every reference with its resolved identity and span;
/// a use inside `span` whose declaration sits outside it is an outer
/// reference (the same rule the row adapter uses for `CALL` subquery imports
/// and sort-carrier analysis).
fn outer_uses_in_span(
    span: SourceSpan,
    analyzed: &AnalyzedStatement,
) -> Result<Vec<(BindingId, selene_core::DbString, SourceSpan)>, PlannerError> {
    let mut out = Vec::new();
    for reference in &analyzed.references {
        if !span_contains(span, reference.span) {
            continue;
        }
        let Some(decl) = analyzed.scopes.declaration(reference.binding) else {
            continue;
        };
        if span_contains(span, decl.span()) {
            continue;
        }
        out.push((reference.binding, reference.name.clone(), reference.span));
    }
    out.sort_by_key(|(binding, _, _)| *binding);
    out.dedup_by_key(|(binding, _, _)| *binding);
    Ok(out)
}

/// Outer-binding uses for one `MATCH` clause pattern.
fn outer_uses_in_match(
    pattern: &crate::MatchClause,
    span: SourceSpan,
    analyzed: &AnalyzedStatement,
) -> Result<Vec<(BindingId, selene_core::DbString, SourceSpan)>, PlannerError> {
    let _ = pattern;
    outer_uses_in_span(span, analyzed)
}

fn span_contains(outer: SourceSpan, inner: SourceSpan) -> bool {
    outer.byte_offset <= inner.byte_offset && inner.end() <= outer.end()
}
