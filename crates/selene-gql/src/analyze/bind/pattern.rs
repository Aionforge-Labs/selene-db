//! Graph-pattern bind handling.

use crate::{
    EdgePattern, GqlType, GraphPattern, LabelExpr, MatchClause, MatchMode, NodePattern, PathMode,
    PathSelector, PatternElement, Quantifier,
    analyze::{
        AnalyzedType,
        binding::BindingDeclKind,
        error::{AnalysisError, ConditionClause},
        write_set::{WriteKind, property_keys},
    },
};

use super::{BindContext, expr};

pub(crate) fn bind_match_clause(
    ctx: &mut BindContext,
    clause: &MatchClause,
) -> Result<(), AnalysisError> {
    ctx.use_working_graph(clause.span)?;
    validate_unbounded_legality(clause)?;
    for pattern in &clause.patterns {
        bind_graph_pattern(ctx, pattern, PatternBindingMode::Match)?;
    }
    if let Some(where_clause) = &clause.where_clause {
        expr::bind_condition(ctx, where_clause, ConditionClause::MatchWhere)?;
    }
    Ok(())
}

fn validate_unbounded_legality(clause: &MatchClause) -> Result<(), AnalysisError> {
    let Some(span) = clause.patterns.iter().find_map(first_unbounded_edge_span) else {
        return Ok(());
    };
    if clause.path_mode != PathMode::Walk
        || is_selective(clause.selector)
        || clause.match_mode == Some(MatchMode::DifferentEdges)
    {
        return Ok(());
    }
    Err(AnalysisError::UnboundedRequiresGate {
        mode: clause.path_mode,
        selector: clause.selector,
        span,
    })
}

fn first_unbounded_edge_span(pattern: &GraphPattern) -> Option<crate::SourceSpan> {
    pattern.elements.iter().find_map(|element| match element {
        PatternElement::Edge(EdgePattern {
            quantifier: Some(Quantifier::GraphPattern { min: _, max: None }),
            span,
            ..
        }) => Some(*span),
        PatternElement::Node(_) | PatternElement::Edge(_) => None,
    })
}

fn is_selective(selector: Option<PathSelector>) -> bool {
    // Per ISO 39075:2024 §16.6 SR4: "A <path search prefix> other than <all path
    // search> is selective." So every selector except `ALL` (which selene models as
    // `PathSelector::All`, the absence of this arm) is selective — including the
    // counted shortest path (G019) and counted shortest group (G020) prefixes, which
    // are the primary reason to write an unbounded variable-length pattern.
    matches!(
        selector,
        Some(
            PathSelector::Any { .. }
                | PathSelector::AnyShortest
                | PathSelector::AllShortest
                | PathSelector::CountedShortest { .. }
                | PathSelector::CountedShortestGroup { .. }
        )
    )
}

pub(crate) fn bind_insert_graph_pattern(
    ctx: &mut BindContext,
    statement_index: usize,
    pattern: &GraphPattern,
) -> Result<(), AnalysisError> {
    bind_graph_pattern(ctx, pattern, PatternBindingMode::Insert { statement_index })
}

fn bind_graph_pattern(
    ctx: &mut BindContext,
    pattern: &GraphPattern,
    mode: PatternBindingMode,
) -> Result<(), AnalysisError> {
    if let Some(name) = &pattern.path_binding {
        ctx.declare_or_reuse(BindingDeclKind::PathBinding, name.clone(), pattern.span)?;
    }

    for element in &pattern.elements {
        match element {
            PatternElement::Node(node) => bind_node_pattern(ctx, node, mode)?,
            PatternElement::Edge(edge) => bind_edge_pattern(ctx, edge, mode)?,
        }
    }
    Ok(())
}

fn bind_node_pattern(
    ctx: &mut BindContext,
    node: &NodePattern,
    mode: PatternBindingMode,
) -> Result<(), AnalysisError> {
    if let Some(name) = &node.binding {
        let kind = match mode {
            PatternBindingMode::Match => BindingDeclKind::NodePattern,
            PatternBindingMode::Insert { .. } => BindingDeclKind::InsertNode,
        };
        let (binding, reused) = ctx.declare_or_reuse_with_labels_info(
            kind,
            name.clone(),
            node.span,
            node.label_expr.clone(),
        )?;
        if let PatternBindingMode::Insert { statement_index } = mode
            && !reused
        {
            ctx.record_write(
                statement_index,
                node.span,
                WriteKind::InsertNode {
                    binding: Some(binding),
                    label_expr: node.label_expr.clone(),
                    property_keys: property_keys(&node.properties),
                },
            );
        }
    } else if let PatternBindingMode::Insert { statement_index } = mode {
        ctx.record_write(
            statement_index,
            node.span,
            WriteKind::InsertNode {
                binding: None,
                label_expr: node.label_expr.clone(),
                property_keys: property_keys(&node.properties),
            },
        );
    }
    if let Some(label) = &node.label_expr {
        bind_label_expr(label);
    }
    for (_, value) in &node.properties {
        expr::bind_value_expr(ctx, value)?;
    }
    if let Some(where_clause) = &node.inline_where {
        expr::bind_condition(ctx, where_clause, ConditionClause::InlineWhere)?;
    }
    Ok(())
}

fn bind_edge_pattern(
    ctx: &mut BindContext,
    edge: &EdgePattern,
    mode: PatternBindingMode,
) -> Result<(), AnalysisError> {
    if let Some(name) = &edge.binding {
        let kind = match mode {
            PatternBindingMode::Match => BindingDeclKind::EdgePattern,
            PatternBindingMode::Insert { .. } => BindingDeclKind::InsertEdge,
        };
        let ty = edge_binding_type(edge, mode);
        let (binding, reused) = ctx.declare_or_reuse_with_labels_typed_info(
            kind,
            name.clone(),
            edge.span,
            ty,
            edge.label_expr.clone(),
        )?;
        if let PatternBindingMode::Insert { statement_index } = mode
            && !reused
        {
            ctx.record_write(
                statement_index,
                edge.span,
                WriteKind::InsertEdge {
                    binding: Some(binding),
                    label_expr: edge.label_expr.clone(),
                    property_keys: property_keys(&edge.properties),
                },
            );
        }
    } else if let PatternBindingMode::Insert { statement_index } = mode {
        ctx.record_write(
            statement_index,
            edge.span,
            WriteKind::InsertEdge {
                binding: None,
                label_expr: edge.label_expr.clone(),
                property_keys: property_keys(&edge.properties),
            },
        );
    }
    if let Some(label) = &edge.label_expr {
        bind_label_expr(label);
    }
    for (_, value) in &edge.properties {
        expr::bind_value_expr(ctx, value)?;
    }
    if let Some(where_clause) = &edge.inline_where {
        expr::bind_condition(ctx, where_clause, ConditionClause::InlineWhere)?;
    }
    Ok(())
}

fn edge_binding_type(edge: &EdgePattern, mode: PatternBindingMode) -> AnalyzedType {
    match (mode, &edge.quantifier) {
        (PatternBindingMode::Match, Some(Quantifier::GraphPattern { .. })) => {
            AnalyzedType::Resolved(GqlType::List(Box::new(GqlType::EdgeRef)))
        }
        _ => AnalyzedType::Resolved(GqlType::EdgeRef),
    }
}

fn bind_label_expr(label: &LabelExpr) {
    match label {
        LabelExpr::Single(_) | LabelExpr::Wildcard => {}
        LabelExpr::Conjunction(values) | LabelExpr::Disjunction(values) => {
            for value in values {
                bind_label_expr(value);
            }
        }
        LabelExpr::Negation(value) => bind_label_expr(value),
    }
}

#[derive(Clone, Copy)]
enum PatternBindingMode {
    Match,
    Insert { statement_index: usize },
}
