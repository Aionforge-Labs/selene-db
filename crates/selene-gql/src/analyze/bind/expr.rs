//! Value-expression bind and type-inference handling.

use crate::{
    ExistsBody, IsCheckKind, LimitValue, PipelineStatement, QueryPipeline, ReturnClause, ValueExpr,
    analyze::{
        error::{AnalysisError, ConditionClause, ExpectedType, TypeMismatchContext},
        infer,
        types::{AnalyzedType, ExprId},
    },
};

use super::{
    BindContext,
    element_ref::{
        ElementReferenceRequirement, bind_singleton_element_variable_reference,
        bind_singleton_element_variable_references, validate_singleton_element_variable_reference,
    },
    expr_depth::{check_expr_depth, check_subquery_depth},
    pattern, query,
};
use crate::analyze::{binding::BindingUseKind, scope::ScopeKind};

pub(crate) fn bind_value_expr(
    ctx: &mut BindContext,
    expr: &ValueExpr,
) -> Result<ExprId, AnalysisError> {
    // Why: `ValueExpr` recursion fans out to one stack frame per nesting
    // level; the depth-256 contract enforced by `check_expr_depth` is well
    // above what a small (e.g. 2 MB default, or smaller test-harness) thread
    // stack can carry in debug builds, and the per-level bind frame carries
    // several owned `DbString` values.
    // `stacker::maybe_grow` allocates a fresh 1 MB segment whenever fewer than
    // 256 KB remain; the red zone is sized to comfortably exceed a single
    // unoptimized `bind_value_expr_inner` frame so growth always fires before
    // the next descent can overrun the live segment (rustc/syn/serde pattern).
    stacker::maybe_grow(256 * 1024, 1024 * 1024, || bind_value_expr_inner(ctx, expr))
}

fn bind_value_expr_inner(ctx: &mut BindContext, expr: &ValueExpr) -> Result<ExprId, AnalysisError> {
    if ctx.at_expr_root() {
        check_expr_depth(expr)?;
        check_subquery_depth(expr)?;
    }
    ctx.with_expr_depth(|ctx| {
        let ty = match expr {
            ValueExpr::Literal(literal) => infer::literal(literal),
            ValueExpr::Variable { name, span } => {
                let binding = ctx.resolve(name.clone(), *span, BindingUseKind::Variable)?;
                ctx.binding_type(binding)
            }
            ValueExpr::Parameter {
                name,
                declared_type,
                ..
            } => ctx
                .expr_ids
                .parameter_type(name)
                .or(declared_type.as_ref())
                .cloned()
                .map_or(AnalyzedType::Dynamic, AnalyzedType::Resolved),
            ValueExpr::PropertyAccess { target, .. } => {
                bind_value_expr(ctx, target)?;
                AnalyzedType::Dynamic
            }
            ValueExpr::ListLiteral { items, .. } => {
                let item_types = bind_many_with_spans(ctx, items)?;
                infer::list_literal(&item_types)?
            }
            ValueExpr::RecordLiteral { fields, .. } => {
                let mut field_types = Vec::with_capacity(fields.len());
                for (name, value) in fields {
                    let id = bind_value_expr(ctx, value)?;
                    let ty = match ctx.expr_type(id) {
                        AnalyzedType::Resolved(ty) => ty.clone(),
                        AnalyzedType::Dynamic => crate::GqlType::Any,
                    };
                    field_types.push((name.clone(), ty));
                }
                AnalyzedType::Resolved(crate::GqlType::Record(crate::RecordType::Closed(
                    field_types,
                )))
            }
            ValueExpr::PathConstructor { elements, span } => {
                bind_path_constructor(ctx, elements, *span)?;
                AnalyzedType::Resolved(crate::GqlType::Path)
            }
            ValueExpr::BinaryOp { op, lhs, rhs, .. } => {
                let lhs_id = bind_value_expr(ctx, lhs)?;
                let rhs_id = bind_value_expr(ctx, rhs)?;
                infer::binary(
                    *op,
                    ctx.expr_type(lhs_id),
                    lhs.span(),
                    ctx.expr_type(rhs_id),
                    rhs.span(),
                )?
            }
            ValueExpr::UnaryOp { op, operand, .. } => {
                let operand_id = bind_value_expr(ctx, operand)?;
                infer::unary(*op, ctx.expr_type(operand_id), operand.span())?
            }
            ValueExpr::FunctionCall { name, args, .. } => {
                if is_element_id_function(name) && args.len() == 1 {
                    bind_singleton_element_variable_reference(
                        ctx,
                        &args[0],
                        ElementReferenceRequirement::NodeOrEdge,
                        "ELEMENT_ID argument",
                    )?;
                    AnalyzedType::Resolved(crate::GqlType::String)
                } else {
                    bind_many(ctx, args)?;
                    AnalyzedType::Dynamic
                }
            }
            ValueExpr::DurationBetween { start, end, .. } => {
                bind_value_expr(ctx, start)?;
                bind_value_expr(ctx, end)?;
                AnalyzedType::Resolved(crate::GqlType::Duration)
            }
            ValueExpr::Normalize { source, .. } => {
                let source_id = bind_value_expr(ctx, source)?;
                infer::normalize(ctx.expr_type(source_id), source.span())?
            }
            ValueExpr::Trim {
                character, source, ..
            } => {
                let character_id = character
                    .as_deref()
                    .map(|character| bind_value_expr(ctx, character))
                    .transpose()?;
                let source_id = bind_value_expr(ctx, source)?;
                let character_type = character_id
                    .zip(character.as_deref())
                    .map(|(id, character)| (ctx.expr_type(id), character.span()));
                infer::trim(character_type, ctx.expr_type(source_id), source.span())?
            }
            ValueExpr::IsCheck {
                operand,
                kind,
                span,
                ..
            } => {
                let operand_id = bind_value_expr(ctx, operand)?;
                bind_is_check(ctx, operand, operand_id, kind)?;
                infer::is_check(kind, ctx.expr_type(operand_id), operand.span(), *span)?
            }
            ValueExpr::InList { operand, list, .. } => {
                let operand_id = bind_value_expr(ctx, operand)?;
                let items = bind_many_with_spans(ctx, list)?;
                infer::in_list(ctx.expr_type(operand_id), operand.span(), &items)?
            }
            ValueExpr::InListExpression { operand, list, .. } => {
                let operand_id = bind_value_expr(ctx, operand)?;
                let list_id = bind_value_expr(ctx, list)?;
                infer::in_list_expression(
                    ctx.expr_type(operand_id),
                    operand.span(),
                    ctx.expr_type(list_id),
                    list.span(),
                )?
            }
            ValueExpr::AllDifferent { items, .. } => {
                bind_singleton_element_variable_references(ctx, items, "ALL_DIFFERENT arguments")?;
                AnalyzedType::Resolved(crate::GqlType::Boolean)
            }
            ValueExpr::Same { items, .. } => {
                bind_singleton_element_variable_references(ctx, items, "SAME arguments")?;
                AnalyzedType::Resolved(crate::GqlType::Boolean)
            }
            ValueExpr::PropertyExists { target, .. } => {
                bind_property_exists_target(ctx, target)?;
                AnalyzedType::Resolved(crate::GqlType::Boolean)
            }
            ValueExpr::Case {
                branches,
                else_branch,
                span,
            } => {
                let mut result_types =
                    Vec::with_capacity(branches.len() + usize::from(else_branch.is_some()));
                for (condition, value) in branches {
                    let value_id =
                        ctx.with_child_scope(ScopeKind::CaseBranch, *span, false, |ctx| {
                            bind_condition(ctx, condition, ConditionClause::CaseWhen)?;
                            bind_value_expr(ctx, value)
                        })?;
                    result_types.push((ctx.expr_type(value_id).clone(), value.span()));
                }
                if let Some(value) = else_branch {
                    let value_id =
                        ctx.with_child_scope(ScopeKind::CaseBranch, value.span(), false, |ctx| {
                            bind_value_expr(ctx, value)
                        })?;
                    result_types.push((ctx.expr_type(value_id).clone(), value.span()));
                }
                infer::case_result(&result_types)?
            }
            ValueExpr::Exists { body, span, .. } => {
                bind_exists_body(ctx, body, *span)?;
                AnalyzedType::Resolved(crate::GqlType::Boolean)
            }
            ValueExpr::ValueSubquery { body, span } => bind_value_subquery(ctx, body, *span)?,
            ValueExpr::Cast {
                value, target_type, ..
            } => {
                bind_value_expr(ctx, value)?;
                infer::cast(target_type)?
            }
        };
        ctx.allocate_expr(expr, ty)
    })
}

fn bind_exists_body(
    ctx: &mut BindContext,
    body: &ExistsBody,
    span: crate::SourceSpan,
) -> Result<(), AnalysisError> {
    ctx.with_child_scope(ScopeKind::Subquery, span, false, |ctx| match body {
        ExistsBody::Match(clause) => pattern::bind_match_clause(ctx, clause),
        ExistsBody::Query(pipeline) => query::bind_query_pipeline(ctx, pipeline),
    })
}

fn bind_value_subquery(
    ctx: &mut BindContext,
    body: &QueryPipeline,
    span: crate::SourceSpan,
) -> Result<AnalyzedType, AnalysisError> {
    validate_value_subquery_shape(body, span)?;
    ctx.with_child_scope(ScopeKind::Subquery, span, false, |ctx| {
        query::bind_query_pipeline(ctx, body)
    })?;
    Ok(AnalyzedType::DYNAMIC)
}

fn bind_property_exists_target(
    ctx: &mut BindContext,
    target: &ValueExpr,
) -> Result<(), AnalysisError> {
    bind_singleton_element_variable_reference(
        ctx,
        target,
        ElementReferenceRequirement::NodeOrEdge,
        "PROPERTY_EXISTS target",
    )
}

fn bind_path_constructor(
    ctx: &mut BindContext,
    elements: &[ValueExpr],
    span: crate::SourceSpan,
) -> Result<(), AnalysisError> {
    if elements.is_empty() || elements.len().is_multiple_of(2) {
        return Err(AnalysisError::InvalidReference {
            message: "PATH constructor requires node, edge, node, ... elements".to_owned(),
            span,
        });
    }
    for (position, element) in elements.iter().enumerate() {
        let element_id = bind_value_expr(ctx, element)?;
        let expected = if position % 2 == 0 {
            crate::GqlType::NodeRef
        } else {
            crate::GqlType::EdgeRef
        };
        match ctx.expr_type(element_id) {
            AnalyzedType::Dynamic | AnalyzedType::Resolved(crate::GqlType::Null) => {}
            AnalyzedType::Resolved(found) if *found == expected => {}
            AnalyzedType::Resolved(found) => {
                return Err(AnalysisError::TypeMismatch {
                    context: TypeMismatchContext::PathConstructorElement { position },
                    expected: ExpectedType::Specific(expected),
                    found: found.clone(),
                    span: element.span(),
                });
            }
        }
    }
    Ok(())
}

fn is_element_id_function(name: &crate::NonEmpty<selene_core::DbString>) -> bool {
    name.len() == 1 && name.first().as_str().eq_ignore_ascii_case("element_id")
}

fn validate_value_subquery_shape(
    body: &QueryPipeline,
    span: crate::SourceSpan,
) -> Result<(), AnalysisError> {
    let Some(return_index) = body
        .statements
        .iter()
        .rposition(|statement| matches!(statement, PipelineStatement::Return(_)))
    else {
        return Err(value_shape_error(
            "VALUE subquery requires a RETURN clause (ISO §20.6; iso_gql_clause20.txt:1143)",
            span,
        ));
    };
    let PipelineStatement::Return(return_clause) = &body.statements[return_index] else {
        unreachable!("rposition selected a RETURN");
    };
    if return_clause.star || return_clause.items.len() != 1 {
        return Err(value_shape_error(
            "VALUE subquery RETURN must contain exactly one item (ISO §20.6; iso_gql_clause20.txt:1143)",
            return_clause.span,
        ));
    }
    if has_effective_final_limit_one(body, return_index)
        || direct_aggregate_without_group_by(return_clause)
    {
        return Ok(());
    }
    Err(value_shape_error(
        "VALUE subquery must use LIMIT 1 or a direct aggregate without GROUP BY (ISO §20.6; iso_gql_clause20.txt:1143)",
        return_clause.span,
    ))
}

fn direct_aggregate_without_group_by(clause: &ReturnClause) -> bool {
    clause.group_by.is_none()
        && clause.items.first().is_some_and(|item| {
            matches!(
                &item.expr,
                ValueExpr::FunctionCall { name, .. } if is_aggregate_name(name.first())
            )
        })
}

fn has_effective_final_limit_one(body: &QueryPipeline, return_index: usize) -> bool {
    body.statements
        .iter()
        .skip(return_index.saturating_add(1))
        .try_fold(false, |seen_limit_one, statement| match statement {
            PipelineStatement::Limit(limit) => Some(matches!(limit, LimitValue::Count(1, _))),
            PipelineStatement::Offset(_) | PipelineStatement::Sorting(_) => Some(seen_limit_one),
            _ => None,
        })
        .unwrap_or(false)
}

pub(super) fn is_aggregate_name(name: &selene_core::DbString) -> bool {
    let name = name.as_str();
    [
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "collect_list",
        "stddev_pop",
        "stddev_samp",
        "percentile_cont",
        "percentile_disc",
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn value_shape_error(message: &'static str, span: crate::SourceSpan) -> AnalysisError {
    AnalysisError::ValueSubqueryShapeViolation {
        message: message.to_owned(),
        span,
    }
}

pub(crate) fn bind_condition(
    ctx: &mut BindContext,
    expr: &ValueExpr,
    clause: ConditionClause,
) -> Result<ExprId, AnalysisError> {
    let id = bind_value_expr(ctx, expr)?;
    infer::condition(ctx.expr_type(id), expr.span(), clause)?;
    Ok(id)
}

fn bind_many(
    ctx: &mut BindContext,
    values: &[ValueExpr],
) -> Result<Vec<AnalyzedType>, AnalysisError> {
    values
        .iter()
        .map(|value| {
            let id = bind_value_expr(ctx, value)?;
            Ok(ctx.expr_type(id).clone())
        })
        .collect()
}

fn bind_many_with_spans(
    ctx: &mut BindContext,
    values: &[ValueExpr],
) -> Result<Vec<(AnalyzedType, crate::SourceSpan)>, AnalysisError> {
    values
        .iter()
        .map(|value| {
            let id = bind_value_expr(ctx, value)?;
            Ok((ctx.expr_type(id).clone(), value.span()))
        })
        .collect()
}

fn bind_is_check(
    ctx: &mut BindContext,
    operand: &ValueExpr,
    operand_id: ExprId,
    kind: &IsCheckKind,
) -> Result<(), AnalysisError> {
    match kind {
        IsCheckKind::SourceOf(value) => {
            bind_source_destination(ctx, operand, operand_id, value, "IS SOURCE OF")
        }
        IsCheckKind::DestinationOf(value) => {
            bind_source_destination(ctx, operand, operand_id, value, "IS DESTINATION OF")
        }
        IsCheckKind::Directed => validate_singleton_element_variable_reference(
            ctx,
            operand,
            operand_id,
            ElementReferenceRequirement::Edge,
            "IS DIRECTED",
        ),
        IsCheckKind::Labeled(_) => validate_singleton_element_variable_reference(
            ctx,
            operand,
            operand_id,
            ElementReferenceRequirement::NodeOrEdge,
            "IS LABELED",
        ),
        IsCheckKind::Null
        | IsCheckKind::TruthValue(_)
        | IsCheckKind::Typed(_)
        | IsCheckKind::Normalized(_) => Ok(()),
    }
}

fn bind_source_destination(
    ctx: &mut BindContext,
    node: &ValueExpr,
    node_id: ExprId,
    edge: &ValueExpr,
    context: &'static str,
) -> Result<(), AnalysisError> {
    validate_singleton_element_variable_reference(
        ctx,
        node,
        node_id,
        ElementReferenceRequirement::Node,
        context,
    )?;
    let edge_id = bind_value_expr(ctx, edge)?;
    validate_singleton_element_variable_reference(
        ctx,
        edge,
        edge_id,
        ElementReferenceRequirement::Edge,
        context,
    )
}
