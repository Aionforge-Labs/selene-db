//! Query pipeline and clause builders.

use pest::iterators::Pair;
use selene_core::DbString;

use crate::{
    ast::{
        ForStatement, LimitValue, NullsPolicy, OrderDirection, OrderTerm, PipelineStatement,
        QueryPipeline, ReturnClause, ReturnItem, RowExpansionPosition, RowExpansionPositionKind,
        WithClause,
    },
    error::ParserError,
};

use super::{
    Rule, build_typed_param_ref, call, db_string_pair, expr, first_child, let_stmt,
    not_implemented, pattern, span, unexpected_pair,
};

pub(super) fn build_query_pipeline(pair: Pair<'_, Rule>) -> Result<QueryPipeline, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::query_pipeline);
    let first = pair
        .clone()
        .into_inner()
        .next()
        .ok_or_else(ParserError::empty_program)?;
    if matches!(first.as_rule(), Rule::focused_query | Rule::nested_query) {
        return super::scopes::build_focused_or_nested(first);
    }
    build_pipeline_from_children(pair)
}

pub(super) fn build_exists_match_body_pipeline(
    pair: Pair<'_, Rule>,
) -> Result<QueryPipeline, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::exists_match_body);
    build_pipeline_from_children(pair)
}

pub(super) fn build_call_query_pipeline(
    pair: Pair<'_, Rule>,
) -> Result<QueryPipeline, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::call_query_pipeline);
    build_pipeline_from_children(pair)
}

pub(super) fn build_pipeline_from_children(
    pair: Pair<'_, Rule>,
) -> Result<QueryPipeline, ParserError> {
    let source_span = span(&pair);
    let statements = pair
        .into_inner()
        .map(|child| build_pipeline_statement(child))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(QueryPipeline {
        statements,
        span: source_span,
        ..QueryPipeline::default()
    })
}

pub(super) fn build_pipeline_statement(
    pair: Pair<'_, Rule>,
) -> Result<PipelineStatement, ParserError> {
    if matches!(
        pair.as_rule(),
        Rule::pipeline_statement | Rule::post_call_pipeline_statement | Rule::exists_query_tail
    ) {
        return build_pipeline_statement(first_child(pair)?);
    }

    match pair.as_rule() {
        Rule::match_stmt => pattern::build_match_clause(pair).map(PipelineStatement::Match),
        Rule::filter_stmt => build_filter(pair).map(PipelineStatement::Filter),
        Rule::let_stmt => let_stmt::build_let(pair).map(PipelineStatement::Let),
        Rule::for_stmt => build_for(pair).map(PipelineStatement::For),
        Rule::sorting_stmt => build_sorting(pair).map(PipelineStatement::Sorting),
        Rule::offset_stmt => build_limit_or_offset(pair).map(PipelineStatement::Offset),
        Rule::limit_stmt => build_limit_or_offset(pair).map(PipelineStatement::Limit),
        Rule::return_stmt => build_return_clause(pair).map(PipelineStatement::Return),
        Rule::with_stmt => build_with_clause(pair).map(PipelineStatement::With),
        Rule::call_stmt => call::build_pipeline_call(pair),
        _ => Err(unexpected_pair(pair, "expected pipeline statement")),
    }
}

pub(super) fn build_select_pipeline(pair: Pair<'_, Rule>) -> Result<QueryPipeline, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::select_stmt);
    let source_span = span(&pair);
    let mut return_clause = ReturnClause {
        distinct: false,
        star: false,
        items: Vec::new(),
        group_by: None,
        having: None,
        span: source_span,
    };
    // SELECT desugars to a query pipeline whose statements are emitted in
    // semantic order: pre-projection (MATCH, WHERE/FILTER), then RETURN, then
    // post-projection (ORDER BY, OFFSET, LIMIT). Routing WHERE through the
    // post-RETURN slot would filter on projected rows instead of source rows
    // and silently change query semantics whenever projection introduces
    // aliases or aggregation.
    let mut pre_return = Vec::new();
    let mut post_return = Vec::new();
    let mut saw_select_from = false;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::select_kw => {}
            Rule::distinct_kw => return_clause.distinct = true,
            Rule::all_kw => {}
            Rule::return_star => return_clause.star = true,
            Rule::projection_list => return_clause.items = build_projection_list(child)?,
            Rule::select_from => {
                saw_select_from = true;
                let select_from_span = span(&child);
                let from_child = child
                    .into_inner()
                    .find(|nested| matches!(nested.as_rule(), Rule::match_stmt | Rule::ident))
                    .ok_or_else(|| {
                        ParserError::syntax("SELECT FROM is missing source", select_from_span, None)
                    })?;
                if from_child.as_rule() == Rule::match_stmt {
                    pre_return.push(PipelineStatement::Match(pattern::build_match_clause(
                        from_child,
                    )?));
                } else {
                    return Err(not_implemented(
                        &from_child,
                        "SELECT FROM graph-name resolution is not yet supported",
                    ));
                }
            }
            Rule::where_clause => {
                pre_return.push(PipelineStatement::Filter(build_where(child)?));
            }
            Rule::group_by_clause => return_clause.group_by = Some(build_group_by(child)?),
            Rule::having_clause => return_clause.having = Some(build_having(child)?),
            Rule::sorting_stmt => {
                post_return.push(PipelineStatement::Sorting(build_sorting(child)?));
            }
            Rule::offset_stmt => {
                post_return.push(PipelineStatement::Offset(build_limit_or_offset(child)?));
            }
            Rule::limit_stmt => {
                post_return.push(PipelineStatement::Limit(build_limit_or_offset(child)?));
            }
            _ => return Err(unexpected_pair(child, "unexpected SELECT child")),
        }
    }

    if return_clause.star && !saw_select_from {
        return Err(ParserError::syntax(
            "SELECT * requires a FROM clause",
            source_span,
            Some("add FROM MATCH (...) or project explicit SELECT items".into()),
        ));
    }
    if return_clause.star && return_clause.group_by.is_some() {
        return Err(ParserError::syntax(
            "SELECT * cannot specify GROUP BY",
            source_span,
            Some("project explicit SELECT items before grouping".into()),
        ));
    }

    let mut statements = pre_return;
    statements.push(PipelineStatement::Return(return_clause));
    statements.extend(post_return);
    Ok(QueryPipeline {
        statements,
        span: source_span,
        select_origin: Some(source_span),
        ..QueryPipeline::default()
    })
}

pub(super) fn build_filter(pair: Pair<'_, Rule>) -> Result<crate::ast::ValueExpr, ParserError> {
    expr_from_first(pair, "FILTER is missing expression")
}

fn build_where(pair: Pair<'_, Rule>) -> Result<crate::ast::ValueExpr, ParserError> {
    expr_from_first(pair, "WHERE is missing expression")
}

fn build_having(pair: Pair<'_, Rule>) -> Result<crate::ast::ValueExpr, ParserError> {
    expr_from_first(pair, "HAVING is missing expression")
}

fn expr_from_first(
    pair: Pair<'_, Rule>,
    missing: &'static str,
) -> Result<crate::ast::ValueExpr, ParserError> {
    let pair_span = span(&pair);
    let expr_pair = pair
        .into_inner()
        .find(|child| child.as_rule() == Rule::expr)
        .ok_or_else(|| ParserError::syntax(missing, pair_span, None))?;
    expr::build_value_expr(expr_pair)
}

fn build_for(pair: Pair<'_, Rule>) -> Result<ForStatement, ParserError> {
    let source_span = span(&pair);
    let mut alias = None;
    let mut source = None;
    let mut position = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::for_kw | Rule::in_kw => {}
            Rule::ident if alias.is_none() => alias = Some(db_string_pair(child)?),
            Rule::expr => source = Some(expr::build_value_expr(child)?),
            Rule::for_position => position = Some(build_for_position(child)?),
            _ => return Err(unexpected_pair(child, "unexpected FOR child")),
        }
    }
    Ok(ForStatement {
        source: source.ok_or_else(|| {
            ParserError::syntax("FOR is missing source expression", source_span, None)
        })?,
        alias: alias.ok_or_else(|| {
            ParserError::syntax("FOR is missing binding variable", source_span, None)
        })?,
        position,
        span: source_span,
    })
}

fn build_for_position(pair: Pair<'_, Rule>) -> Result<RowExpansionPosition, ParserError> {
    let source_span = span(&pair);
    let mut kind = None;
    let mut alias = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::with_kw => {}
            Rule::for_position_ordinality => kind = Some(RowExpansionPositionKind::Ordinality),
            Rule::for_position_offset => kind = Some(RowExpansionPositionKind::Offset),
            Rule::ident => alias = Some(db_string_pair(child)?),
            _ => return Err(unexpected_pair(child, "unexpected FOR position child")),
        }
    }
    Ok(RowExpansionPosition {
        kind: kind.ok_or_else(|| {
            ParserError::syntax(
                "FOR position is missing ORDINALITY or OFFSET",
                source_span,
                None,
            )
        })?,
        alias: alias.ok_or_else(|| {
            ParserError::syntax(
                "FOR position is missing binding variable",
                source_span,
                None,
            )
        })?,
    })
}

fn build_sorting(pair: Pair<'_, Rule>) -> Result<Vec<OrderTerm>, ParserError> {
    pair.into_inner()
        .filter(|child| child.as_rule() == Rule::order_term)
        .map(|child| build_order_term(child))
        .collect()
}

fn build_order_term(pair: Pair<'_, Rule>) -> Result<OrderTerm, ParserError> {
    let source_span = span(&pair);
    let mut expr_value = None;
    let mut direction = OrderDirection::Asc;
    let mut nulls = None;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::expr => expr_value = Some(expr::build_value_expr(child)?),
            Rule::sort_dir if child.as_str().eq_ignore_ascii_case("DESC") => {
                direction = OrderDirection::Desc;
            }
            Rule::sort_dir => direction = OrderDirection::Asc,
            Rule::nulls_order if child.as_str().to_ascii_uppercase().contains("FIRST") => {
                nulls = Some(NullsPolicy::NullsFirst);
            }
            Rule::nulls_order => nulls = Some(NullsPolicy::NullsLast),
            _ => return Err(unexpected_pair(child, "unexpected ORDER BY child")),
        }
    }

    Ok(OrderTerm {
        expr: expr_value.ok_or_else(|| {
            ParserError::syntax("ORDER BY term is missing expression", source_span, None)
        })?,
        direction,
        nulls,
        span: source_span,
    })
}

fn build_limit_or_offset(pair: Pair<'_, Rule>) -> Result<LimitValue, ParserError> {
    let source_span = span(&pair);
    let child = pair
        .into_inner()
        .find(|child| child.as_rule() == Rule::limit_value)
        .ok_or_else(|| ParserError::syntax("LIMIT/OFFSET is missing value", source_span, None))?;
    let inner = first_child(child)?;
    match inner.as_rule() {
        Rule::uint => inner
            .as_str()
            .parse::<u64>()
            .map(|value| LimitValue::Count(value, span(&inner)))
            .map_err(|error| {
                ParserError::syntax(format!("invalid LIMIT/OFFSET: {error}"), span(&inner), None)
            }),
        Rule::typed_param_ref => {
            let (name, declared_type, span) = build_typed_param_ref(inner)?;
            Ok(LimitValue::Parameter {
                name,
                declared_type,
                span,
            })
        }
        _ => Err(unexpected_pair(inner, "expected LIMIT/OFFSET value")),
    }
}

pub(super) fn build_return_clause(pair: Pair<'_, Rule>) -> Result<ReturnClause, ParserError> {
    let source_span = span(&pair);
    let mut clause = ReturnClause {
        distinct: false,
        star: false,
        items: Vec::new(),
        group_by: None,
        having: None,
        span: source_span,
    };

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::return_kw => {}
            Rule::distinct_kw => clause.distinct = true,
            Rule::all_kw => {}
            Rule::return_star => clause.star = true,
            Rule::projection_list => clause.items = build_projection_list(child)?,
            Rule::group_by_clause => clause.group_by = Some(build_group_by(child)?),
            Rule::having_clause => clause.having = Some(build_having(child)?),
            Rule::no_bindings => {
                return Err(ParserError::not_implemented(
                    "RETURN NO BINDINGS is an ISO specification device, not user GQL syntax",
                    span(&child),
                    Some("use FINISH for write pipelines that intentionally omit a result"),
                ));
            }
            _ => return Err(unexpected_pair(child, "unexpected RETURN child")),
        }
    }

    if !clause.star && clause.items.is_empty() {
        return Err(ParserError::syntax(
            "RETURN requires at least one projection item",
            source_span,
            None,
        ));
    }
    if clause.star && clause.group_by.is_some() {
        return Err(ParserError::syntax(
            "RETURN * cannot specify GROUP BY",
            source_span,
            Some("project explicit RETURN items before grouping".into()),
        ));
    }
    Ok(clause)
}

fn build_with_clause(pair: Pair<'_, Rule>) -> Result<WithClause, ParserError> {
    let source_span = span(&pair);
    let mut clause = WithClause {
        distinct: false,
        items: Vec::new(),
        group_by: None,
        having: None,
        where_clause: None,
        span: source_span,
    };

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::with_kw => {}
            Rule::distinct_kw => clause.distinct = true,
            Rule::projection_list => clause.items = build_projection_list(child)?,
            Rule::group_by_clause => clause.group_by = Some(build_group_by(child)?),
            Rule::having_clause => clause.having = Some(build_having(child)?),
            Rule::where_clause => clause.where_clause = Some(build_where(child)?),
            _ => return Err(unexpected_pair(child, "unexpected WITH child")),
        }
    }
    Ok(clause)
}

fn build_projection_list(pair: Pair<'_, Rule>) -> Result<Vec<ReturnItem>, ParserError> {
    pair.into_inner()
        .filter(|child| child.as_rule() == Rule::projection)
        .map(|child| build_return_item(child))
        .collect()
}

fn build_return_item(pair: Pair<'_, Rule>) -> Result<ReturnItem, ParserError> {
    let source_span = span(&pair);
    let mut value = None;
    let mut alias = None;

    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::expr => value = Some(expr::build_value_expr(child)?),
            Rule::alias => alias = Some(build_alias(child)?),
            _ => {
                return Err(unexpected_pair(
                    child,
                    "expected RETURN expression or alias",
                ));
            }
        }
    }

    Ok(ReturnItem {
        expr: value.ok_or_else(|| {
            ParserError::syntax("RETURN item is missing expression", source_span, None)
        })?,
        alias,
        span: source_span,
    })
}

fn build_alias(pair: Pair<'_, Rule>) -> Result<DbString, ParserError> {
    let source_span = span(&pair);
    let ident = pair
        .into_inner()
        .find(|child| child.as_rule() == Rule::ident)
        .ok_or_else(|| ParserError::syntax("AS alias is missing identifier", source_span, None))?;
    db_string_pair(ident)
}

fn build_group_by(pair: Pair<'_, Rule>) -> Result<Vec<crate::ast::ValueExpr>, ParserError> {
    pair.into_inner()
        .filter(|child| child.as_rule() == Rule::group_by_item)
        .map(|item| expr_from_first(item, "GROUP BY item is missing expression"))
        .collect()
}
