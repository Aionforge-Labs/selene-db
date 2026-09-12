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

/// Collect top-level `MATCH` clauses in pipeline order.
///
/// Covers query, composite, chained, and mutation pipelines. `EXISTS` and
/// `CALL`-subquery graph patterns stay with F03-PR04 full family coverage.
pub(super) fn collect_match_clauses(analyzed: &AnalyzedStatement) -> Vec<(usize, &MatchClause)> {
    let mut out = Vec::new();
    let mut clause_index = 0usize;
    match analyzed.source() {
        Statement::Query(pipeline) => {
            for statement in &pipeline.statements {
                if let crate::PipelineStatement::Match(clause) = statement {
                    out.push((clause_index, clause));
                    clause_index += 1;
                }
            }
        }
        Statement::Composite { first, rest, .. } => {
            for statement in &first.statements {
                if let crate::PipelineStatement::Match(clause) = statement {
                    out.push((clause_index, clause));
                    clause_index += 1;
                }
            }
            for (_, pipeline) in rest {
                for statement in &pipeline.statements {
                    if let crate::PipelineStatement::Match(clause) = statement {
                        out.push((clause_index, clause));
                        clause_index += 1;
                    }
                }
            }
        }
        Statement::Chained { blocks, .. } => {
            for block in blocks {
                for statement in &block.statements {
                    if let crate::PipelineStatement::Match(clause) = statement {
                        out.push((clause_index, clause));
                        clause_index += 1;
                    }
                }
            }
        }
        Statement::Mutate(pipeline) => {
            for statement in &pipeline.statements {
                if let crate::MutationStatement::Match(clause) = statement {
                    out.push((clause_index, clause));
                    clause_index += 1;
                }
            }
        }
        Statement::Ddl(_)
        | Statement::Call(_)
        | Statement::Explain { .. }
        | Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::SessionSetValue { .. }
        | Statement::SessionSetTimeZone { .. }
        | Statement::SessionSetGraph { .. }
        | Statement::SessionReset { .. }
        | Statement::SessionClose { .. } => {}
    }
    out
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
