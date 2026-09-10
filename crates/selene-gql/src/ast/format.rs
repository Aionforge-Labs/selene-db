//! Read-side AST pretty-printer.

mod cast;
mod expr;
mod is_check;
mod keywords;
mod preflight;
mod trim;
mod type_name;
mod working_scope;

use std::fmt::{self, Write as _};

use selene_core::DbString;

use super::format_ident::fmt_ident;
use crate::ast::{
    EdgeDirection, EdgePattern, GqlType, GraphPattern, InlineProcedureCall, LabelExpr, LimitValue,
    MatchClause, NodePattern, NullsPolicy, OrderDirection, OrderTerm, PathMode, PathSelector,
    PatternElement, ProcedureCall, Quantifier, QueryPipeline, ReturnClause, ReturnItem, Statement,
    ValueExpr, WithClause,
};

use expr::fmt_expr;
use keywords::{fmt_match_mode, fmt_path_mode, fmt_path_selector, fmt_set_op};
use preflight::validate_formattable;
use type_name::fmt_type;

pub(crate) use type_name::fmt_type as format_gql_type;

pub(crate) fn format_value_expr(value: &ValueExpr) -> String {
    let mut out = String::new();
    fmt_expr(&mut out, value).expect("formatting an expression into String cannot fail");
    out
}

/// Format a read-side statement as GQL source.
///
/// # Errors
///
/// Returns [`FormatError::Unsupported`] for write-side AST surfaces and
/// [`FormatError::Invalid`] for ASTs that violate parser-level syntax
/// invariants.
pub fn format_read_statement(stmt: &Statement) -> Result<String, FormatError> {
    validate_formattable(stmt)?;

    let mut out = String::new();
    match stmt {
        Statement::Query(pipeline) => fmt_pipeline(&mut out, pipeline)?,
        Statement::Composite { first, rest, .. } => {
            fmt_pipeline(&mut out, first)?;
            for (op, pipeline) in rest {
                write!(out, " {} ", fmt_set_op(*op))?;
                fmt_pipeline(&mut out, pipeline)?;
            }
        }
        Statement::Chained { blocks, .. } => {
            for (index, block) in blocks.iter().enumerate() {
                if index > 0 {
                    out.push_str(" NEXT ");
                }
                fmt_pipeline(&mut out, block)?;
            }
        }
        Statement::Mutate(_) => {
            return Err(FormatError::Unsupported {
                variant: "MutateStatement",
            });
        }
        Statement::Ddl(_) => {
            return Err(FormatError::Unsupported {
                variant: "DdlStatement",
            });
        }
        Statement::Call(_) => {
            return Err(FormatError::Unsupported {
                variant: "ProcedureCall",
            });
        }
        Statement::Explain { .. } => {
            return Err(FormatError::Unsupported {
                variant: "ExplainStatement",
            });
        }
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. } => {
            return Err(FormatError::Unsupported {
                variant: "TransactionControl",
            });
        }
        Statement::SessionSetValue { .. }
        | Statement::SessionSetTimeZone { .. }
        | Statement::SessionSetGraph { .. }
        | Statement::SessionReset { .. }
        | Statement::SessionClose { .. } => {
            return Err(FormatError::Unsupported {
                variant: "SessionControl",
            });
        }
    }
    Ok(out)
}

/// Format one procedure call as canonical GQL source.
///
/// # Errors
///
/// Returns [`FormatError::Unsupported`] when the call contains an AST-only
/// expression surface that the parser cannot read back from formatted source.
pub fn format_procedure_call(call: &ProcedureCall) -> Result<String, FormatError> {
    preflight::validate_procedure_call(call)?;

    let mut out = String::new();
    fmt_call(&mut out, call)?;
    Ok(out)
}

/// Errors raised by the pretty-printer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FormatError {
    /// AST surface is not formattable by this read-side pretty-printer.
    #[error("AST surface {variant} is not yet formattable by the read-side pretty-printer")]
    Unsupported {
        /// Stable AST variant name.
        variant: &'static str,
    },
    /// AST violates a syntax invariant enforced by the parser.
    #[error("AST is not valid GQL: {reason}")]
    Invalid {
        /// Stable reason string for the invalid AST shape.
        reason: &'static str,
    },
    /// Formatting failed.
    #[error("formatting failed")]
    Fmt(#[from] fmt::Error),
}

pub(super) fn fmt_pipeline(out: &mut String, pipeline: &QueryPipeline) -> fmt::Result {
    let braces = working_scope::prefixes(out, &pipeline.working_scopes)?;
    for (index, statement) in pipeline.statements.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        match statement {
            crate::PipelineStatement::Match(value) => fmt_match(out, value)?,
            crate::PipelineStatement::Filter(value) => {
                out.push_str("FILTER ");
                fmt_expr(out, value)?;
            }
            crate::PipelineStatement::Let(values) => {
                out.push_str("LET ");
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    if let Some(declared_type) = &value.declared_type {
                        write!(
                            out,
                            "VALUE {} :: {} = ",
                            fmt_ident(value.alias.clone()),
                            fmt_type(declared_type)
                        )?;
                    } else {
                        write!(out, "{} = ", fmt_ident(value.alias.clone()))?;
                    }
                    fmt_expr(out, &value.value)?;
                }
            }
            crate::PipelineStatement::For(value) => {
                write!(out, "FOR {} IN ", fmt_ident(value.alias.clone()))?;
                fmt_expr(out, &value.source)?;
                if let Some(position) = &value.position {
                    match position.kind {
                        crate::RowExpansionPositionKind::Ordinality => {
                            out.push_str(" WITH ORDINALITY ");
                        }
                        crate::RowExpansionPositionKind::Offset => {
                            out.push_str(" WITH OFFSET ");
                        }
                    }
                    out.push_str(&fmt_ident(position.alias.clone()));
                }
            }
            crate::PipelineStatement::Sorting(values) => fmt_order(out, values)?,
            crate::PipelineStatement::Limit(value) => {
                out.push_str("LIMIT ");
                fmt_limit(out, value)?;
            }
            crate::PipelineStatement::Offset(value) => {
                out.push_str("OFFSET ");
                fmt_limit(out, value)?;
            }
            crate::PipelineStatement::Return(value) => fmt_return(out, value)?,
            crate::PipelineStatement::With(value) => fmt_with(out, value)?,
            crate::PipelineStatement::Call(value) => fmt_call(out, value)?,
            crate::PipelineStatement::CallSubquery(value) => fmt_inline_call(out, value)?,
        }
    }
    for _ in 0..braces {
        out.push_str(" }");
    }
    Ok(())
}

pub(super) fn fmt_match(out: &mut String, clause: &MatchClause) -> fmt::Result {
    if clause.optional {
        out.push_str("OPTIONAL ");
    }
    out.push_str("MATCH");
    let selector_consumes_mode_and_paths =
        if let Some(PathSelector::CountedShortestGroup { groups }) = clause.selector {
            write!(out, " SHORTEST {groups}")?;
            if clause.path_mode != PathMode::Walk
                || (clause.path_mode == PathMode::Walk && clause.path_mode_explicit)
            {
                write!(out, " {}", fmt_path_mode(clause.path_mode))?;
            }
            if clause.path_or_paths {
                out.push_str(" PATHS");
            }
            out.push_str(" GROUPS");
            true
        } else {
            if let Some(selector) = clause.selector {
                out.push(' ');
                fmt_path_selector(out, selector)?;
            }
            false
        };
    if let Some(mode) = clause.match_mode {
        write!(out, " {}", fmt_match_mode(mode))?;
    }
    if !selector_consumes_mode_and_paths
        && (clause.path_mode != PathMode::Walk
            || (clause.path_mode == PathMode::Walk && clause.path_mode_explicit))
    {
        write!(out, " {}", fmt_path_mode(clause.path_mode))?;
    }
    // ISO §16.6 <path or paths> (Feature G014): faithfully reproduce the
    // explicit PATH/PATHS keyword so a parse->format->parse round trip
    // preserves the AST `path_or_paths` flag (mirrors the explicit-WALK
    // handling above). Canonical plural `PATHS` per §16.6 SR1.
    if clause.path_or_paths && !selector_consumes_mode_and_paths {
        out.push_str(" PATHS");
    }
    out.push(' ');
    for (index, pattern) in clause.patterns.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        fmt_graph_pattern(out, pattern)?;
    }
    if let Some(where_clause) = &clause.where_clause {
        out.push_str(" WHERE ");
        fmt_expr(out, where_clause)?;
    }
    Ok(())
}

fn fmt_graph_pattern(out: &mut String, pattern: &GraphPattern) -> fmt::Result {
    if let Some(binding) = &pattern.path_binding {
        write!(out, "{} = ", fmt_ident(binding.clone()))?;
    }
    for element in &pattern.elements {
        match element {
            PatternElement::Node(node) => fmt_node_pattern(out, node)?,
            PatternElement::Edge(edge) => fmt_edge_pattern(out, edge)?,
        }
    }
    Ok(())
}

fn fmt_node_pattern(out: &mut String, node: &NodePattern) -> fmt::Result {
    out.push('(');
    if let Some(binding) = &node.binding {
        out.push_str(&fmt_ident(binding.clone()));
    }
    if let Some(label) = &node.label_expr {
        fmt_label_expr(out, label)?;
    }
    fmt_properties(out, &node.properties)?;
    if let Some(where_clause) = &node.inline_where {
        out.push_str(" WHERE ");
        fmt_expr(out, where_clause)?;
    }
    out.push(')');
    Ok(())
}

fn fmt_edge_pattern(out: &mut String, edge: &EdgePattern) -> fmt::Result {
    if edge.abbreviated {
        out.push_str(match edge.direction {
            EdgeDirection::Right => "->",
            EdgeDirection::Left => "<-",
            EdgeDirection::Undirected => "~",
            EdgeDirection::LeftOrUndirected => "<~",
            EdgeDirection::UndirectedOrRight => "~>",
            EdgeDirection::LeftOrRight => "<->",
            EdgeDirection::Any => "-",
        });
        return Ok(());
    }
    match edge.direction {
        EdgeDirection::Right | EdgeDirection::Any => out.push_str("-["),
        EdgeDirection::Left | EdgeDirection::LeftOrRight => out.push_str("<-["),
        EdgeDirection::Undirected | EdgeDirection::UndirectedOrRight => out.push_str("~["),
        EdgeDirection::LeftOrUndirected => out.push_str("<~["),
    }
    if let Some(binding) = &edge.binding {
        out.push_str(&fmt_ident(binding.clone()));
    }
    if let Some(label) = &edge.label_expr {
        fmt_label_expr(out, label)?;
    }
    if let Some(quantifier) = edge.quantifier {
        match quantifier {
            Quantifier::GraphPattern {
                min,
                max: Some(max),
            } if max == min => write!(out, "{{{max}}}")?,
            Quantifier::GraphPattern {
                min,
                max: Some(max),
            } => write!(out, "{{{min},{max}}}")?,
            Quantifier::GraphPattern { min, max: None } => write!(out, "{{{min},}}")?,
            Quantifier::Questioned => out.push('?'),
        }
    }
    fmt_properties(out, &edge.properties)?;
    if let Some(where_clause) = &edge.inline_where {
        out.push_str(" WHERE ");
        fmt_expr(out, where_clause)?;
    }
    match edge.direction {
        EdgeDirection::Right | EdgeDirection::LeftOrRight => out.push_str("]->"),
        EdgeDirection::Left | EdgeDirection::Any => out.push_str("]-"),
        EdgeDirection::Undirected | EdgeDirection::LeftOrUndirected => out.push_str("]~"),
        EdgeDirection::UndirectedOrRight => out.push_str("]~>"),
    }
    Ok(())
}

fn fmt_properties(out: &mut String, properties: &[(DbString, ValueExpr)]) -> fmt::Result {
    if properties.is_empty() {
        return Ok(());
    }
    out.push_str(" {");
    for (index, (key, value)) in properties.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        write!(out, "{}: ", fmt_ident(key.clone()))?;
        fmt_expr(out, value)?;
    }
    out.push('}');
    Ok(())
}

fn fmt_label_expr(out: &mut String, label: &LabelExpr) -> fmt::Result {
    match label {
        LabelExpr::Single(label) => write!(out, ":{}", fmt_ident(label.clone())),
        LabelExpr::Conjunction(parts) => fmt_label_parts(out, parts, "&"),
        LabelExpr::Disjunction(parts) => fmt_label_parts(out, parts, "|"),
        LabelExpr::Negation(inner) => {
            out.push_str(":!");
            fmt_label_expr_body(out, inner)
        }
        LabelExpr::Wildcard => {
            out.push_str(":%");
            Ok(())
        }
    }
}

fn fmt_label_expr_body(out: &mut String, label: &LabelExpr) -> fmt::Result {
    match label {
        LabelExpr::Single(label) => write!(out, "{}", fmt_ident(label.clone())),
        _ => fmt_label_expr(out, label),
    }
}

fn fmt_label_parts(out: &mut String, parts: &[LabelExpr], sep: &str) -> fmt::Result {
    out.push(':');
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            out.push_str(sep);
        }
        fmt_label_expr_body(out, part)?;
    }
    Ok(())
}

fn fmt_return(out: &mut String, clause: &ReturnClause) -> fmt::Result {
    out.push_str("RETURN ");
    if clause.distinct {
        out.push_str("DISTINCT ");
    }
    if clause.star {
        out.push('*');
    } else {
        fmt_return_items(out, &clause.items)?;
    }
    fmt_group_having(out, clause.group_by.as_deref(), clause.having.as_ref())
}

fn fmt_with(out: &mut String, clause: &WithClause) -> fmt::Result {
    out.push_str("WITH ");
    if clause.distinct {
        out.push_str("DISTINCT ");
    }
    fmt_return_items(out, &clause.items)?;
    fmt_group_having(out, clause.group_by.as_deref(), clause.having.as_ref())?;
    if let Some(where_clause) = &clause.where_clause {
        out.push_str(" WHERE ");
        fmt_expr(out, where_clause)?;
    }
    Ok(())
}

fn fmt_return_items(out: &mut String, items: &[ReturnItem]) -> fmt::Result {
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        fmt_expr(out, &item.expr)?;
        if let Some(alias) = &item.alias {
            write!(out, " AS {}", fmt_ident(alias.clone()))?;
        }
    }
    Ok(())
}

fn fmt_group_having(
    out: &mut String,
    group_by: Option<&[ValueExpr]>,
    having: Option<&ValueExpr>,
) -> fmt::Result {
    if let Some(group_by) = group_by {
        out.push_str(" GROUP BY ");
        if group_by.is_empty() {
            out.push_str("()");
        } else {
            for (index, item) in group_by.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                fmt_expr(out, item)?;
            }
        }
    }
    if let Some(having) = having {
        out.push_str(" HAVING ");
        fmt_expr(out, having)?;
    }
    Ok(())
}

fn fmt_order(out: &mut String, terms: &[OrderTerm]) -> fmt::Result {
    out.push_str("ORDER BY ");
    for (index, term) in terms.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        fmt_expr(out, &term.expr)?;
        if term.direction == OrderDirection::Desc {
            out.push_str(" DESC");
        }
        if let Some(nulls) = term.nulls {
            out.push_str(match nulls {
                NullsPolicy::NullsFirst => " NULLS FIRST",
                NullsPolicy::NullsLast => " NULLS LAST",
            });
        }
    }
    Ok(())
}

fn fmt_limit(out: &mut String, value: &LimitValue) -> fmt::Result {
    match value {
        LimitValue::Count(value, _) => write!(out, "{value}"),
        LimitValue::Parameter {
            name,
            declared_type,
            ..
        } => fmt_parameter(out, name.clone(), declared_type.as_ref()),
    }
}

fn fmt_call(out: &mut String, call: &ProcedureCall) -> fmt::Result {
    if call.optional {
        out.push_str("OPTIONAL ");
    }
    out.push_str("CALL ");
    for (index, part) in call.name.iter().enumerate() {
        if index > 0 {
            out.push('.');
        }
        out.push_str(&fmt_ident(part.clone()));
    }
    out.push('(');
    for (index, arg) in call.args.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        fmt_expr(out, arg)?;
    }
    out.push(')');
    if !call.yield_items.is_empty() {
        out.push_str(" YIELD ");
        for (index, item) in call.yield_items.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            match &item.column {
                crate::YieldColumn::Star => out.push('*'),
                crate::YieldColumn::Named(name) => out.push_str(&fmt_ident(name.clone())),
            }
            if let Some(alias) = &item.alias {
                write!(out, " AS {}", fmt_ident(alias.clone()))?;
            }
        }
    }
    Ok(())
}

fn fmt_inline_call(out: &mut String, call: &InlineProcedureCall) -> fmt::Result {
    if call.optional {
        out.push_str("OPTIONAL ");
    }
    out.push_str("CALL ");
    if let Some(scope) = &call.variable_scope {
        out.push('(');
        for (index, name) in scope.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&fmt_ident(name.clone()));
        }
        out.push_str(") ");
    }
    out.push_str("{ ");
    fmt_pipeline(out, &call.body)?;
    out.push_str(" }");
    if !call.yield_items.is_empty() {
        out.push_str(" YIELD ");
        fmt_yield_items(out, &call.yield_items)?;
    }
    Ok(())
}

fn fmt_yield_items(out: &mut String, items: &[crate::YieldItem]) -> fmt::Result {
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        match &item.column {
            crate::YieldColumn::Star => out.push('*'),
            crate::YieldColumn::Named(name) => out.push_str(&fmt_ident(name.clone())),
        }
        if let Some(alias) = &item.alias {
            write!(out, " AS {}", fmt_ident(alias.clone()))?;
        }
    }
    Ok(())
}

pub(super) fn fmt_parameter(
    out: &mut String,
    name: DbString,
    declared_type: Option<&GqlType>,
) -> fmt::Result {
    write!(out, "${}", fmt_ident(name))?;
    if let Some(declared_type) = declared_type {
        write!(out, " :: {}", fmt_type(declared_type))?;
    }
    Ok(())
}
