//! Bounded ISO procedure heads and focused/nested linear queries.

use super::{Rule, catalog_ddl, db_string_pair, first_child, query, span, unexpected_pair};
use crate::{
    CatalogObjectReference, GraphExpression, IdentifierForm, ParserError, QueryPipeline,
    WorkingScopeClause,
};
use pest::iterators::Pair;

pub(super) fn build_specification(pair: Pair<'_, Rule>) -> Result<QueryPipeline, ParserError> {
    let source_span = span(&pair);
    let mut scopes = Vec::new();
    let mut pipeline = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::at_schema_clause => {
                let clause_span = span(&child);
                let path = child
                    .into_inner()
                    .find(|part| part.as_rule() == Rule::absolute_catalog_path)
                    .ok_or_else(ParserError::empty_program)?;
                scopes.push(WorkingScopeClause::At {
                    reference: catalog_ddl::build_absolute_path(path)?,
                    span: clause_span,
                });
            }
            Rule::query_pipeline => pipeline = Some(query::build_query_pipeline(child)?),
            Rule::composite_query | Rule::chained_query => {
                return Err(ParserError::not_implemented(
                    "composition inside an AT or nested query specification is not implemented",
                    span(&child),
                    None,
                ));
            }
            _ => return Err(unexpected_pair(child, "expected query specification")),
        }
    }
    let mut pipeline = pipeline.ok_or_else(ParserError::empty_program)?;
    scopes.append(&mut pipeline.working_scopes);
    pipeline.working_scopes = scopes;
    pipeline.span = source_span;
    Ok(pipeline)
}

pub(super) fn build_focused_or_nested(pair: Pair<'_, Rule>) -> Result<QueryPipeline, ParserError> {
    let source_span = span(&pair);
    if pair.as_rule() == Rule::nested_query {
        let mut pipeline = build_specification(first_child(pair)?)?;
        pipeline
            .working_scopes
            .insert(0, WorkingScopeClause::Nested(source_span));
        pipeline.span = source_span;
        return Ok(pipeline);
    }
    let mut scopes = Vec::new();
    let mut pipeline = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::use_graph_clause => {
                let clause_span = span(&child);
                let expression = child
                    .into_inner()
                    .find(|part| part.as_rule() == Rule::scope_graph_expression)
                    .ok_or_else(ParserError::empty_program)?;
                scopes.push(WorkingScopeClause::Use {
                    expression: build_graph_expression(expression)?,
                    span: clause_span,
                });
            }
            Rule::nested_query => pipeline = Some(build_focused_or_nested(child)?),
            Rule::focused_query_body => {
                pipeline = Some(query::build_pipeline_from_children(child)?)
            }
            Rule::mutation_pipeline => {
                return Err(ParserError::not_implemented(
                    "focused data-modifying statements are not implemented",
                    span(&child),
                    None,
                ));
            }
            _ => return Err(unexpected_pair(child, "expected focused query body")),
        }
    }
    let mut pipeline = pipeline.ok_or_else(ParserError::empty_program)?;
    scopes.append(&mut pipeline.working_scopes);
    pipeline.working_scopes = scopes;
    pipeline.span = source_span;
    Ok(pipeline)
}

fn build_graph_expression(pair: Pair<'_, Rule>) -> Result<GraphExpression, ParserError> {
    let origin = span(&pair);
    let child = first_child(pair)?;
    Ok(match child.as_rule() {
        Rule::session_current_graph => GraphExpression::Current {
            property: first_child(child)?.as_rule() == Rule::current_property_graph_kw,
            span: origin,
        },
        Rule::scope_graph_variable => {
            let name = child
                .into_inner()
                .find(|part| part.as_rule() == Rule::ident)
                .ok_or_else(ParserError::empty_program)
                .and_then(db_string_pair)?;
            GraphExpression::Variable { name, span: origin }
        }
        Rule::graph_reference => {
            let reference = catalog_ddl::build_graph_reference(child)?;
            let may_reference_binding =
                !reference.absolute && reference.leaf().form == IdentifierForm::Regular;
            GraphExpression::Reference {
                reference,
                may_reference_binding,
            }
        }
        Rule::scope_relative_graph => {
            let segment = catalog_ddl::catalog_segment(first_child(child)?)?;
            GraphExpression::Reference {
                reference: CatalogObjectReference {
                    absolute: false,
                    segments: vec![segment],
                    span: origin,
                },
                may_reference_binding: false,
            }
        }
        Rule::typed_param_ref => {
            return Err(ParserError::not_implemented(
                "graph reference parameters are not implemented (GV60)",
                origin,
                None,
            ));
        }
        _ => return Err(unexpected_pair(child, "expected graph expression")),
    })
}
