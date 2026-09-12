//! Transport logical path decisions into the physical plan, without relowering.

use crate::{
    AnalyzedStatement, BindingTableColumn, BindingTableSchema, GraphPattern, MatchClause,
    PathAutomaton, PathConditions, PathProgram, PathSemanticElement, PatternElement, PlannerError,
};

/// Resolve source expression payloads only after checking their semantic IDs.
pub(crate) fn conditions(
    source: &GraphPattern,
    automaton: &PathAutomaton,
    analyzed: &AnalyzedStatement,
) -> Result<Vec<PathConditions>, PlannerError> {
    let invalid = || PlannerError::NotImplemented {
        feature: "path predicate semantic identity mismatch",
        span: source.span,
    };
    if source.span != automaton.origin || source.elements.len() != automaton.semantic.elements.len()
    {
        return Err(invalid());
    }
    source
        .elements
        .iter()
        .zip(&automaton.semantic.elements)
        .map(|(source, semantic)| {
            let (properties, inline, ids, inline_id) = match (source, semantic) {
                (PatternElement::Node(n), PathSemanticElement::Node(t)) => (
                    &n.properties,
                    &n.inline_where,
                    &t.property_predicates,
                    t.inline_where,
                ),
                (PatternElement::Edge(e), PathSemanticElement::Edge(t)) => (
                    &e.properties,
                    &e.inline_where,
                    &t.property_predicates,
                    t.inline_where,
                ),
                _ => return Err(invalid()),
            };
            if properties.len() != ids.len()
                || properties
                    .iter()
                    .zip(ids)
                    .any(|((_, e), id)| analyzed.expr_ids.get(e) != Some(*id))
                || inline.as_ref().and_then(|e| analyzed.expr_ids.get(e)) != inline_id
            {
                return Err(invalid());
            }
            Ok(PathConditions {
                properties: properties.clone(),
                inline: inline.clone(),
            })
        })
        .collect()
}

pub(super) fn lower(
    clause: &MatchClause,
    analyzed: &AnalyzedStatement,
    context: super::PathLowering<'_>,
) -> Result<PathProgram, PlannerError> {
    let automata: Vec<_> = context
        .paths
        .automata
        .iter()
        .filter(|a| a.match_mode.origin == clause.span)
        .cloned()
        .collect();
    if automata.len() != clause.patterns.len() || automata.is_empty() {
        return Err(PlannerError::NotImplemented {
            feature: "logical path clause missing from physical lowering",
            span: clause.span,
        });
    }
    let mut bindings = Vec::new();
    let mut input_bindings = Vec::new();
    let mut columns = Vec::new();
    let mut predicates = Vec::new();
    for (automaton, source) in automata.iter().zip(&clause.patterns) {
        for element in &automaton.semantic.elements {
            if let PathSemanticElement::Edge(edge) = element
                && let crate::EdgeQuantifierKind::Bounded { max, .. } = edge.quantifier
                && max > context.max_quantifier
            {
                return Err(PlannerError::ProgramLimitExceeded {
                    limit_name: "max_quantifier",
                    limit: context.max_quantifier,
                    actual: max,
                    span: edge.origin,
                });
            }
        }
        for id in automaton.semantic.named_bindings() {
            if bindings.contains(&id) {
                continue;
            }
            let decl =
                analyzed
                    .scopes
                    .declaration(id)
                    .ok_or(PlannerError::BindingResolutionLost {
                        binding: id,
                        span: automaton.origin,
                    })?;
            bindings.push(id);
            if decl.span().byte_offset < clause.span.byte_offset
                || decl.span().end() > clause.span.end()
            {
                input_bindings.push(id);
            }
            columns.push(BindingTableColumn {
                name: Some(decl.name()),
                hidden: None,
                ty: decl.ty().clone(),
            });
        }
        predicates.push(conditions(source, automaton, analyzed)?);
    }
    Ok(PathProgram {
        automata,
        bindings,
        input_bindings,
        schema: BindingTableSchema { columns },
        conditions: predicates,
    })
}
