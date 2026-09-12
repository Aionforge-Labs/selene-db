//! Clause collection and finite-result / shape gates for path lowering.
//!
//! The unbounded-quantifier gate mirrors the analyzer's ISO §16.4 rule: an
//! unbounded quantifier under `WALK` without a selective prefix or
//! `DIFFERENT EDGES` is rejected here with
//! [`crate::plan::PlannerError::UnboundedPathRequiresGate`] instead of
//! receiving an arbitrary runtime hop cap. Shape gates reuse the historical
//! `NotImplemented` tags (`"empty graph pattern"`, `"non-alternating graph
//! pattern"`, `"edge without target"`) so existing diagnostics stay stable.

use crate::{
    GraphPattern, LabelExpr, MatchClause, PatternElement, Quantifier, SourceSpan, Statement,
    analyze::AnalyzedStatement,
    plan::{PlannerError, logical::path::automaton::is_selective_selector},
};

use super::limits::PathLoweringLimits;

/// Collect `MATCH` clauses in lowering order.
///
/// Covers top-level query, composite, chained, and mutation pipelines, plus
/// `EXPLAIN` inner statements, inline `CALL` subquery bodies (including
/// GP03 explicit-import forms), and `EXISTS`/value-subquery graph patterns.
/// Top-level clauses come first in pipeline order; nested bodies follow in
/// source order. Every clause lowers to automata from semantic descriptors,
/// so path/group-variable metadata (conditional singletons, scopes) survives
/// lowering without a second path algorithm.
pub(super) fn collect_match_clauses(analyzed: &AnalyzedStatement) -> Vec<(usize, &MatchClause)> {
    let mut out = Vec::new();
    collect_statement_clauses(analyzed.source(), &mut out);
    // Index in collection order (clause order, then pattern order downstream).
    out.into_iter().enumerate().collect()
}

fn collect_statement_clauses<'a>(statement: &'a Statement, out: &mut Vec<&'a MatchClause>) {
    match statement {
        Statement::Query(pipeline) => collect_query_pipeline_clauses(pipeline, out),
        Statement::Composite { first, rest, .. } => {
            collect_query_pipeline_clauses(first, out);
            for (_, pipeline) in rest {
                collect_query_pipeline_clauses(pipeline, out);
            }
        }
        Statement::Chained { blocks, .. } => {
            for block in blocks {
                collect_query_pipeline_clauses(block, out);
            }
        }
        Statement::Mutate(pipeline) => {
            for statement in &pipeline.statements {
                match statement {
                    crate::MutationStatement::Match(clause) => {
                        out.push(clause);
                        collect_match_clause_exprs(clause, out);
                    }
                    crate::MutationStatement::Filter(value) => {
                        collect_value_expr_clauses(value, out);
                    }
                    crate::MutationStatement::Insert(_)
                    | crate::MutationStatement::Set(_)
                    | crate::MutationStatement::Remove(_)
                    | crate::MutationStatement::Delete(_) => {}
                }
            }
        }
        Statement::Ddl(_) | Statement::Call(_) => {}
        Statement::Explain { inner, .. } => {
            collect_statement_clauses(inner, out);
        }
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::SessionSetValue { .. }
        | Statement::SessionSetTimeZone { .. }
        | Statement::SessionSetGraph { .. }
        | Statement::SessionReset { .. }
        | Statement::SessionClose { .. } => {}
    }
}

fn collect_query_pipeline_clauses<'a>(
    pipeline: &'a crate::QueryPipeline,
    out: &mut Vec<&'a MatchClause>,
) {
    for statement in &pipeline.statements {
        match statement {
            crate::PipelineStatement::Match(clause) => {
                out.push(clause);
                collect_match_clause_exprs(clause, out);
            }
            crate::PipelineStatement::Filter(value) => {
                collect_value_expr_clauses(value, out);
            }
            crate::PipelineStatement::Let(bindings) => {
                for binding in bindings {
                    collect_value_expr_clauses(&binding.value, out);
                }
            }
            crate::PipelineStatement::For(statement) => {
                collect_value_expr_clauses(&statement.source, out);
            }
            crate::PipelineStatement::Sorting(terms) => {
                for term in terms {
                    collect_value_expr_clauses(&term.expr, out);
                }
            }
            crate::PipelineStatement::Limit(_) | crate::PipelineStatement::Offset(_) => {}
            crate::PipelineStatement::Return(clause) => {
                for item in &clause.items {
                    collect_value_expr_clauses(&item.expr, out);
                }
                if let Some(keys) = &clause.group_by {
                    for key in keys {
                        collect_value_expr_clauses(key, out);
                    }
                }
                if let Some(having) = &clause.having {
                    collect_value_expr_clauses(having, out);
                }
            }
            crate::PipelineStatement::With(clause) => {
                for item in &clause.items {
                    collect_value_expr_clauses(&item.expr, out);
                }
                if let Some(keys) = &clause.group_by {
                    for key in keys {
                        collect_value_expr_clauses(key, out);
                    }
                }
                if let Some(having) = &clause.having {
                    collect_value_expr_clauses(having, out);
                }
                if let Some(where_clause) = &clause.where_clause {
                    collect_value_expr_clauses(where_clause, out);
                }
            }
            crate::PipelineStatement::Call(call) => {
                for arg in &call.args {
                    collect_value_expr_clauses(arg, out);
                }
            }
            crate::PipelineStatement::CallSubquery(call) => {
                // GP03 imports do not change collection: the body patterns
                // lower with their import-resolved identities.
                collect_query_pipeline_clauses(&call.body, out);
            }
        }
    }
}

fn collect_match_clause_exprs<'a>(clause: &'a MatchClause, out: &mut Vec<&'a MatchClause>) {
    if let Some(where_clause) = &clause.where_clause {
        collect_value_expr_clauses(where_clause, out);
    }
    for pattern in &clause.patterns {
        for element in &pattern.elements {
            match element {
                crate::PatternElement::Node(node) => {
                    if let Some(where_clause) = &node.inline_where {
                        collect_value_expr_clauses(where_clause, out);
                    }
                    for (_, value) in &node.properties {
                        collect_value_expr_clauses(value, out);
                    }
                }
                crate::PatternElement::Edge(edge) => {
                    if let Some(where_clause) = &edge.inline_where {
                        collect_value_expr_clauses(where_clause, out);
                    }
                    for (_, value) in &edge.properties {
                        collect_value_expr_clauses(value, out);
                    }
                }
            }
        }
    }
}

fn collect_value_expr_clauses<'a>(value: &'a crate::ValueExpr, out: &mut Vec<&'a MatchClause>) {
    match value {
        crate::ValueExpr::Exists { body, .. } => match body {
            crate::ExistsBody::Match(clause) => {
                out.push(clause.as_ref());
                collect_match_clause_exprs(clause.as_ref(), out);
            }
            crate::ExistsBody::Query(pipeline) => {
                collect_query_pipeline_clauses(pipeline.as_ref(), out);
            }
        },
        crate::ValueExpr::ValueSubquery { body, .. } => {
            collect_query_pipeline_clauses(body.as_ref(), out);
        }
        _ => {
            value.for_each_child(&mut |child| {
                collect_value_expr_clauses(child, out);
            });
        }
    }
}

/// Require strictly alternating node/edge/node elements ending on a node.
pub(super) fn check_alternating(pattern: &GraphPattern) -> Result<(), PlannerError> {
    let mut expect_node = true;
    for element in &pattern.elements {
        match element {
            PatternElement::Node(_) if expect_node => expect_node = false,
            PatternElement::Edge(_) if !expect_node => expect_node = true,
            PatternElement::Node(node) => {
                return Err(PlannerError::NotImplemented {
                    feature: "non-alternating graph pattern",
                    span: node.span,
                });
            }
            PatternElement::Edge(edge) => {
                return Err(PlannerError::NotImplemented {
                    feature: "non-alternating graph pattern",
                    span: edge.span,
                });
            }
        }
    }
    if expect_node {
        let span = pattern.elements.last().map_or(pattern.span, element_origin);
        return Err(PlannerError::NotImplemented {
            feature: "edge without target",
            span,
        });
    }
    Ok(())
}

/// Return one element's source origin.
pub(super) fn element_origin(element: &PatternElement) -> SourceSpan {
    match element {
        PatternElement::Node(node) => node.span,
        PatternElement::Edge(edge) => edge.span,
    }
}

/// Reject unbounded quantifiers without an ISO §16.4 finite-result gate.
pub(super) fn check_unbounded_gate(
    clause: &MatchClause,
    pattern: &GraphPattern,
) -> Result<(), PlannerError> {
    for element in &pattern.elements {
        let PatternElement::Edge(edge) = element else {
            continue;
        };
        let Some(Quantifier::GraphPattern { min: _, max: None }) = edge.quantifier else {
            continue;
        };
        if clause.path_mode != crate::PathMode::Walk
            || is_selective_selector(clause.selector)
            || clause.match_mode == Some(crate::MatchMode::DifferentEdges)
        {
            continue;
        }
        return Err(PlannerError::UnboundedPathRequiresGate {
            mode: clause.path_mode,
            selector: clause.selector,
            match_mode: clause.match_mode,
            span: edge.span,
        });
    }
    Ok(())
}

/// Reject pathological label-disjunction arity before any branch allocation.
pub(super) fn check_label_arity(
    pattern: &GraphPattern,
    limits: &PathLoweringLimits,
) -> Result<(), PlannerError> {
    for element in &pattern.elements {
        let (label, span) = match element {
            PatternElement::Node(node) => (&node.label_expr, node.span),
            PatternElement::Edge(edge) => (&edge.label_expr, edge.span),
        };
        if let Some(LabelExpr::Disjunction(parts)) = label {
            let arity = parts.len() as u32;
            if arity > limits.max_label_branches {
                return Err(PlannerError::ProgramLimitExceeded {
                    limit_name: "max_path_label_branches",
                    limit: limits.max_label_branches,
                    actual: arity,
                    span,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{EmptyProcedureRegistry, analyze, parse};

    use super::super::lowering::lower_path_automata_with_defaults;

    /// The lowering backstop behind the analyzer gate: a semantic tree whose
    /// source lost its ISO §16.4 gate still fails with the rule error — never
    /// with an arbitrary hop-cap program limit.
    #[test]
    fn ungated_unbounded_fails_lowering_without_hop_cap() {
        let statement = parse("MATCH TRAIL (a)-[r:K*]->(b) RETURN r").expect("parses");
        let mut analyzed =
            analyze(statement, &EmptyProcedureRegistry, None).expect("gated input analyzes");
        analyzed.corrupt_for_test(|source, _| {
            let crate::Statement::Query(query) = source else {
                panic!("expected query statement");
            };
            for stmt in &mut query.statements {
                if let crate::PipelineStatement::Match(clause) = stmt {
                    clause.path_mode = crate::PathMode::Walk;
                    clause.path_mode_explicit = false;
                    clause.selector = None;
                    clause.match_mode = None;
                }
            }
        });
        let err = lower_path_automata_with_defaults(&analyzed)
            .expect_err("ungated unbounded must fail lowering");
        assert!(
            matches!(
                err,
                crate::plan::PlannerError::UnboundedPathRequiresGate { .. }
            ),
            "got {err:?}"
        );
        assert_eq!(err.gqlstatus(), crate::GqlStatus::SYNTAX_ERROR);
    }
}
