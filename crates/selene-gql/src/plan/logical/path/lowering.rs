//! Lowering from frozen semantics to path automata.
//!
//! Source syntax supplies only statement ordering and source spans; every
//! binding identity, type, and predicate identity comes from the frozen
//! semantic tree (`BindingId` declarations and `ExprId` cells). There is no
//! second parser and no second type resolver here: an unknown name, a missing
//! expression cell, or a bypassed analyzer gate surfaces as a lowering error.
//!
//! Finite-result enforcement follows ISO §16.4: an unbounded quantifier under
//! `WALK` without a selective prefix or `DIFFERENT EDGES` is rejected with
//! [`crate::plan::PlannerError::UnboundedPathRequiresGate`]. The lowerer never
//! substitutes an arbitrary runtime hop cap. Resource limits from
//! [`super::limits::PathLoweringLimits`] are checked before allocating
//! state/transition storage.

use crate::{
    GraphPattern, MatchClause, PatternElement, Quantifier,
    analyze::{AnalyzedStatement, BindingDeclKind},
    plan::{
        PlannerError,
        logical::{
            LogicalMultiplicity,
            path::{
                automaton::{
                    MatchModeScope, PathAutomaton, PathModeScope, PathState, PathStateId,
                    PathTransition, PathTransitionId, SelectorScope, TransitionKind,
                    is_selective_selector,
                },
                inventory::{PathFeatureInventory, supported_path_inventory},
                limits::PathLoweringLimits,
                semantic::{EdgeQuantifierKind, PathSemanticElement, PathSemanticPattern},
            },
        },
    },
};

use super::{
    builder::PatternBuilder,
    gates::{check_alternating, check_label_arity, check_unbounded_gate, collect_match_clauses},
};

/// Contract version consumed by F03-PR04 (compiler cutover) and F05-PR02
/// (path execution). Bump only with a deliberate contract change.
pub const PATH_AUTOMATA_CONTRACT_VERSION: u32 = 1;

/// One statement's lowered path automata plus the inventory they were built under.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LoweredPathSet {
    /// Automata in lowering order (clause order, then pattern order).
    pub automata: Vec<PathAutomaton>,
    /// Inventory the lowerer accepted.
    pub inventory: PathFeatureInventory,
    /// Contract version (see [`PATH_AUTOMATA_CONTRACT_VERSION`]).
    pub contract_version: u32,
}

impl LoweredPathSet {
    /// Return true when every automaton and transition preserves duplicates.
    #[must_use]
    pub fn preserves_duplicates(&self) -> bool {
        self.automata
            .iter()
            .all(PathAutomaton::preserves_duplicates)
    }
}

/// Lower every top-level `MATCH` pattern of one analyzed statement.
///
/// Walks `MATCH` clauses in pipeline order across query, composite, chained,
/// and mutation pipelines. `EXISTS` / `CALL`-subquery graph patterns stay
/// with F03-PR04 full family coverage and are not lowered here.
///
/// # Errors
///
/// Returns [`PlannerError`] for non-alternating shapes, missing semantic
/// cells, ungated unbounded quantifiers, or exceeded lowering limits.
pub fn lower_path_automata(
    analyzed: &AnalyzedStatement,
    limits: &PathLoweringLimits,
) -> Result<LoweredPathSet, PlannerError> {
    let clauses = collect_match_clauses(analyzed);
    let mut automata = Vec::with_capacity(clauses.len());
    for (clause_index, clause) in clauses {
        for (pattern_index, pattern) in clause.patterns.iter().enumerate() {
            automata.push(lower_one_pattern(
                analyzed,
                clause,
                pattern,
                clause_index,
                pattern_index,
                limits,
            )?);
        }
    }
    Ok(LoweredPathSet {
        automata,
        inventory: supported_path_inventory(),
        contract_version: PATH_AUTOMATA_CONTRACT_VERSION,
    })
}

/// Lower with [`PathLoweringLimits::DEFAULT`].
///
/// # Errors
///
/// See [`lower_path_automata`].
pub fn lower_path_automata_with_defaults(
    analyzed: &AnalyzedStatement,
) -> Result<LoweredPathSet, PlannerError> {
    lower_path_automata(analyzed, &PathLoweringLimits::DEFAULT)
}

/// Measure lowering cost without asserting a service-level objective.
///
/// Runs the requested lowering once and returns wall-clock microseconds plus
/// per-automaton state/transition counts. Callers report the numbers; they
/// never gate correctness on them.
#[must_use]
pub fn measure_path_lowering(
    analyzed: &AnalyzedStatement,
    limits: &PathLoweringLimits,
) -> (u128, Vec<super::automaton::AutomatonStats>) {
    let start = std::time::Instant::now();
    let set = lower_path_automata(analyzed, limits);
    let elapsed = start.elapsed().as_micros();
    let stats = set.map_or_else(
        |_| Vec::new(),
        |set| {
            set.automata
                .iter()
                .map(super::automaton::AutomatonStats::of)
                .collect()
        },
    );
    (elapsed, stats)
}

fn lower_one_pattern(
    analyzed: &AnalyzedStatement,
    clause: &MatchClause,
    pattern: &GraphPattern,
    clause_index: usize,
    pattern_index: usize,
    limits: &PathLoweringLimits,
) -> Result<PathAutomaton, PlannerError> {
    if pattern.elements.is_empty() {
        return Err(PlannerError::NotImplemented {
            feature: "empty graph pattern",
            span: pattern.span,
        });
    }
    if pattern.elements.len() as u32 > limits.max_elements_per_pattern {
        return Err(PlannerError::ProgramLimitExceeded {
            limit_name: "max_path_elements",
            limit: limits.max_elements_per_pattern,
            actual: pattern.elements.len() as u32,
            span: pattern.span,
        });
    }
    check_alternating(pattern)?;
    check_unbounded_gate(clause, pattern)?;
    check_label_arity(pattern, limits)?;

    let questioned = pattern
        .elements
        .iter()
        .filter(|element| {
            matches!(
                element,
                PatternElement::Edge(edge) if edge.quantifier == Some(Quantifier::Questioned)
            )
        })
        .count() as u32;
    let states_needed = pattern.elements.len() as u32 + 1;
    let transitions_needed = pattern.elements.len() as u32 + questioned;
    if states_needed > limits.max_states_per_automaton {
        return Err(PlannerError::ProgramLimitExceeded {
            limit_name: "max_path_automaton_states",
            limit: limits.max_states_per_automaton,
            actual: states_needed,
            span: pattern.span,
        });
    }
    if transitions_needed > limits.max_transitions_per_automaton {
        return Err(PlannerError::ProgramLimitExceeded {
            limit_name: "max_path_automaton_transitions",
            limit: limits.max_transitions_per_automaton,
            actual: transitions_needed,
            span: pattern.span,
        });
    }

    let mut builder = PatternBuilder {
        analyzed,
        scope_fallback: analyzed.root_scope(),
        next_temp: 0,
        node_tests: 0,
        edge_tests: 0,
    };
    let path_binding = pattern
        .path_binding
        .clone()
        .map(|name| builder.resolve_decl(name, pattern.span, BindingDeclKind::PathBinding))
        .transpose()?;

    let mut elements = Vec::with_capacity(pattern.elements.len());
    for (index, element) in pattern.elements.iter().enumerate() {
        match element {
            PatternElement::Node(node) => {
                elements.push(PathSemanticElement::Node(builder.node_test(node, index)?));
            }
            PatternElement::Edge(edge) => {
                elements.push(PathSemanticElement::Edge(builder.edge_test(edge, index)?));
            }
        }
    }
    let semantic = PathSemanticPattern {
        path_binding,
        elements,
        scope: analyzed.root_scope(),
        origin: pattern.span,
    };

    build_automaton(clause, semantic, clause_index, pattern_index)
}

fn build_automaton(
    clause: &MatchClause,
    semantic: PathSemanticPattern,
    clause_index: usize,
    pattern_index: usize,
) -> Result<PathAutomaton, PlannerError> {
    let origin = semantic.origin;
    let mut states = Vec::with_capacity(semantic.elements.len() + 1);
    for (index, element) in semantic.elements.iter().enumerate() {
        states.push(PathState {
            id: PathStateId(index as u32),
            scope: semantic.scope,
            origin: element.origin(),
            accept: false,
        });
    }
    let last_origin = semantic
        .elements
        .last()
        .map_or(origin, PathSemanticElement::origin);
    states.push(PathState {
        id: PathStateId(semantic.elements.len() as u32),
        scope: semantic.scope,
        origin: last_origin,
        accept: true,
    });

    let mut transitions = Vec::with_capacity(semantic.elements.len() + 1);
    let mut node_cursor = 0u32;
    let mut edge_cursor = 0u32;
    for (index, element) in semantic.elements.iter().enumerate() {
        let from = PathStateId(index as u32);
        let to = PathStateId(index as u32 + 1);
        let scope = match element {
            PathSemanticElement::Node(test) => test.scope,
            PathSemanticElement::Edge(test) => test.scope,
        };
        let element_origin = element.origin();
        match element {
            PathSemanticElement::Node(_) => {
                transitions.push(PathTransition {
                    id: PathTransitionId(transitions.len() as u32),
                    from,
                    to,
                    kind: TransitionKind::NodeTest { test: node_cursor },
                    scope,
                    multiplicity: LogicalMultiplicity::PreservesDuplicates,
                    origin: element_origin,
                });
                node_cursor += 1;
            }
            PathSemanticElement::Edge(test) => {
                if test.quantifier.is_questioned() {
                    transitions.push(PathTransition {
                        id: PathTransitionId(transitions.len() as u32),
                        from,
                        to,
                        kind: TransitionKind::Epsilon {
                            note: "questioned_skip",
                        },
                        scope,
                        multiplicity: LogicalMultiplicity::PreservesDuplicates,
                        origin: element_origin,
                    });
                    transitions.push(PathTransition {
                        id: PathTransitionId(transitions.len() as u32),
                        from,
                        to,
                        kind: TransitionKind::EdgeTraverse { test: edge_cursor },
                        scope,
                        multiplicity: LogicalMultiplicity::PreservesDuplicates,
                        origin: element_origin,
                    });
                } else {
                    match test.quantifier {
                        EdgeQuantifierKind::Single => {
                            transitions.push(PathTransition {
                                id: PathTransitionId(transitions.len() as u32),
                                from,
                                to,
                                kind: TransitionKind::EdgeTraverse { test: edge_cursor },
                                scope,
                                multiplicity: LogicalMultiplicity::PreservesDuplicates,
                                origin: element_origin,
                            });
                        }
                        EdgeQuantifierKind::Bounded { min, max } => {
                            transitions.push(PathTransition {
                                id: PathTransitionId(transitions.len() as u32),
                                from,
                                to,
                                kind: TransitionKind::QuantifiedEdge {
                                    test: edge_cursor,
                                    min,
                                    max: Some(max),
                                },
                                scope,
                                multiplicity: LogicalMultiplicity::PreservesDuplicates,
                                origin: element_origin,
                            });
                        }
                        EdgeQuantifierKind::Unbounded { min } => {
                            transitions.push(PathTransition {
                                id: PathTransitionId(transitions.len() as u32),
                                from,
                                to,
                                kind: TransitionKind::QuantifiedEdge {
                                    test: edge_cursor,
                                    min,
                                    max: None,
                                },
                                scope,
                                multiplicity: LogicalMultiplicity::PreservesDuplicates,
                                origin: element_origin,
                            });
                        }
                        EdgeQuantifierKind::Questioned => {
                            return Err(PlannerError::NotImplemented {
                                feature: "questioned quantifier without conditional-singleton exposure",
                                span: element_origin,
                            });
                        }
                    }
                }
                edge_cursor += 1;
            }
        }
    }

    let entry = PathStateId(0);
    let accept = PathStateId(semantic.elements.len() as u32);
    Ok(PathAutomaton {
        clause_index,
        pattern_index,
        semantic,
        states,
        transitions,
        entry,
        accept,
        mode: PathModeScope {
            mode: clause.path_mode,
            explicit: clause.path_mode_explicit,
            origin: clause.span,
        },
        match_mode: MatchModeScope {
            mode: clause.match_mode,
            origin: clause.span,
        },
        selector: SelectorScope {
            selector: clause.selector,
            selective: is_selective_selector(clause.selector),
            origin: clause.span,
        },
        multiplicity: LogicalMultiplicity::PreservesDuplicates,
        origin,
    })
}
