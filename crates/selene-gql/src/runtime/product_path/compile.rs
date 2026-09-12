//! Validate and bind the landed flat automata contract, without parsing again.

use super::super::ExecutorError;
use crate::{
    AnalyzedStatement, BindingExposure, BindingId, BindingTableColumn, BindingTableSchema,
    EdgeQuantifierKind, MatchMode, PathAutomaton, PathSemanticElement, PathTransitionId,
    TransitionKind,
};

/// Validated bounded automata from exactly one MATCH clause.
///
/// Borrows immutable lowering output; named locals unify by analyzer identity,
/// temporaries by (automaton, slot). A program may be reused across snapshots;
/// no graph candidates or execution history are cached here.
pub struct BoundedPathProgram<'a> {
    pub(super) paths: Vec<CompiledPath<'a>>,
    pub(super) bindings: Vec<BindingId>,
    pub(super) schema: BindingTableSchema,
    pub(super) different_edges: bool,
}

pub(super) struct CompiledPath<'a> {
    pub(super) automaton: &'a PathAutomaton,
    pub(super) transitions: Vec<PathTransitionId>,
    pub(super) upper: u32,
}

impl<'a> BoundedPathProgram<'a> {
    /// Validate one clause's automata and resolve its named output columns.
    ///
    /// # Errors
    /// Unsatisfiable bounds fail with an implementation-defined diagnostic.
    /// Open bounds, selectors, predicates and path values are explicitly
    /// unsupported at this seam. Malformed or mixed-clause IR never degrades.
    pub fn compile(
        automata: &'a [PathAutomaton],
        analyzed: &AnalyzedStatement,
    ) -> Result<Self, ExecutorError> {
        let first = automata
            .first()
            .ok_or_else(|| invalid("empty bounded path program"))?;
        let mode = first.match_mode.mode.unwrap_or(default_match_mode()?);
        let mut program = Self {
            paths: Vec::new(),
            bindings: Vec::new(),
            schema: BindingTableSchema {
                columns: Vec::new(),
            },
            different_edges: mode == MatchMode::DifferentEdges,
        };
        for (index, automaton) in automata.iter().enumerate() {
            if automaton.clause_index != first.clause_index
                || automaton.pattern_index != index
                || automaton.match_mode != first.match_mode
            {
                return Err(invalid(
                    "bounded path program requires one complete MATCH clause",
                ));
            }
            if automaton.selector.selector.is_some() {
                return Err(unsupported("product path selectors", automaton.origin));
            }
            if automaton.semantic.path_binding.is_some() {
                return Err(unsupported(
                    "product path value construction",
                    automaton.origin,
                ));
            }
            let path = validate(automaton)?;
            for binding in automaton.semantic.named_bindings() {
                if program.bindings.contains(&binding) {
                    continue;
                }
                let decl = analyzed
                    .scopes
                    .declaration(binding)
                    .ok_or_else(|| invalid("product path binding declaration missing"))?;
                program.bindings.push(binding);
                program.schema.columns.push(BindingTableColumn {
                    name: Some(decl.name()),
                    hidden: None,
                    ty: decl.ty().clone(),
                });
            }
            program.paths.push(path);
        }
        Ok(program)
    }
}

fn validate(automaton: &PathAutomaton) -> Result<CompiledPath<'_>, ExecutorError> {
    let elements = &automaton.semantic.elements;
    if elements.is_empty()
        || elements.len().is_multiple_of(2)
        || automaton.entry.get() != 0
        || automaton.accept.get() as usize != elements.len()
        || automaton.states.len() != elements.len() + 1
        || !automaton.preserves_duplicates()
    {
        return Err(invalid("invalid bounded path automaton shape"));
    }
    for (i, state) in automaton.states.iter().enumerate() {
        if state.id.get() as usize != i
            || state.accept != (i == elements.len())
            || state.scope != automaton.semantic.scope
        {
            return Err(invalid("invalid bounded path automaton state"));
        }
    }
    let mut transitions = Vec::new();
    let mut cursor = 0;
    let mut upper = 0u32;
    let mut temporary_slots = Vec::new();
    for (i, element) in elements.iter().enumerate() {
        let (scope, temporary, expected) = match element {
            PathSemanticElement::Node(node) if i % 2 == 0 => {
                if !node.property_predicates.is_empty() || node.inline_where.is_some() {
                    return Err(unsupported("product path predicates", node.origin));
                }
                if node.binding.is_some() == node.temporary.is_some() {
                    return Err(invalid(
                        "product path node exposure must identify one local",
                    ));
                }
                (
                    node.scope,
                    node.temporary,
                    TransitionKind::NodeTest {
                        test: (i / 2) as u32,
                    },
                )
            }
            PathSemanticElement::Edge(edge) if i % 2 == 1 => {
                if !edge.property_predicates.is_empty() || edge.inline_where.is_some() {
                    return Err(unsupported("product path predicates", edge.origin));
                }
                if edge.orientation
                    != crate::acceptance_for(edge.orientation.declared, edge.orientation.origin)
                {
                    return Err(invalid(
                        "product path orientation mask disagrees with token",
                    ));
                }
                let hidden = match edge.exposure {
                    BindingExposure::Singleton { hidden, .. }
                    | BindingExposure::ConditionalSingleton { hidden, .. }
                    | BindingExposure::GroupList { hidden, .. } => hidden,
                };
                if edge.exposure.named().is_some() == hidden.is_some() {
                    return Err(invalid(
                        "product path edge exposure must identify one local",
                    ));
                }
                let test = (i / 2) as u32;
                let (kind, max) = match edge.quantifier {
                    EdgeQuantifierKind::Single
                        if matches!(edge.exposure, BindingExposure::Singleton { .. }) =>
                    {
                        (TransitionKind::EdgeTraverse { test }, 1)
                    }
                    EdgeQuantifierKind::Questioned if edge.exposure.is_conditional_singleton() => {
                        check_transition(
                            automaton,
                            cursor,
                            i,
                            TransitionKind::Epsilon {
                                note: "questioned_skip",
                            },
                            edge.scope,
                        )?;
                        cursor += 1;
                        (TransitionKind::EdgeTraverse { test }, 1)
                    }
                    EdgeQuantifierKind::Bounded { min, max } if edge.exposure.is_group() => {
                        if min > max {
                            return Err(invalid(
                                "unsatisfiable bounded path quantifier: minimum exceeds maximum",
                            ));
                        }
                        (
                            TransitionKind::QuantifiedEdge {
                                test,
                                min,
                                max: Some(max),
                            },
                            max,
                        )
                    }
                    EdgeQuantifierKind::Unbounded { .. } => {
                        return Err(unsupported("product path open bounds", edge.origin));
                    }
                    _ => return Err(invalid("product path quantifier exposure mismatch")),
                };
                upper = upper
                    .checked_add(max)
                    .ok_or_else(|| invalid("bounded path length overflow"))?;
                (edge.scope, hidden, kind)
            }
            _ => {
                return Err(invalid(
                    "product path elements must alternate nodes and edges",
                ));
            }
        };
        if element.element_index() != i {
            return Err(invalid("product path element index mismatch"));
        }
        if let Some(temp) = temporary {
            if temporary_slots.contains(&temp.slot) {
                return Err(invalid("product path temporary slot reused"));
            }
            temporary_slots.push(temp.slot);
        }
        check_transition(automaton, cursor, i, expected, scope)?;
        transitions.push(automaton.transitions[cursor].id);
        cursor += 1;
    }
    if cursor != automaton.transitions.len() {
        return Err(invalid("extra bounded path automaton transition"));
    }
    Ok(CompiledPath {
        automaton,
        transitions,
        upper,
    })
}

fn check_transition(
    a: &PathAutomaton,
    cursor: usize,
    element: usize,
    kind: TransitionKind,
    scope: crate::ScopeId,
) -> Result<(), ExecutorError> {
    let t = a
        .transitions
        .get(cursor)
        .ok_or_else(|| invalid("missing bounded path automaton transition"))?;
    if t.id.get() as usize != cursor
        || t.from.get() as usize != element
        || t.to.get() as usize != element + 1
        || t.kind != kind
        || t.scope != scope
    {
        return Err(invalid("invalid bounded path automaton transition"));
    }
    Ok(())
}

fn default_match_mode() -> Result<MatchMode, ExecutorError> {
    use selene_profile::{AnnexBDecision, AnnexBValue};
    let record = selene_profile::annex_b_by_id("ID086")
        .ok_or_else(|| invalid("default graph match mode absent from profile"))?;
    match record.decision {
        AnnexBDecision::Selected {
            value: AnnexBValue::Identifier("REPEATABLE ELEMENTS"),
            ..
        } => Ok(MatchMode::RepeatableElements),
        AnnexBDecision::Selected {
            value: AnnexBValue::Identifier("DIFFERENT EDGES"),
            ..
        } => Ok(MatchMode::DifferentEdges),
        _ => Err(invalid("unsupported profile default graph match mode")),
    }
}

pub(super) fn invalid(detail: &'static str) -> ExecutorError {
    ExecutorError::ImplementationDefined { detail }
}

fn unsupported(feature: &'static str, span: crate::SourceSpan) -> ExecutorError {
    ExecutorError::FeatureNotSupportedYet { feature, span }
}
