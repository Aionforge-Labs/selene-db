//! Stable debug rendering for path automata.
//!
//! Output contains stable semantic descriptors (binding, scope, and temporary
//! identities, orientations, quantifiers, exposures, modes, selectors, and
//! source origins as `offset+len`) and never contains runtime addresses,
//! pointer values, or hash-dependent ordering. F03-PR04 and F05-PR02 consume
//! this text in debug fixtures; the field-level contract stays in
//! `automaton.rs` / `semantic.rs`.

use super::{
    automaton::{PathAutomaton, TransitionKind},
    lowering::LoweredPathSet,
    semantic::{BindingExposure, EdgeQuantifierKind},
};

/// Render one automaton as stable multi-line debug text.
#[must_use]
pub fn explain_automaton(automaton: &PathAutomaton) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "path_automaton clause={} pattern={} states={} transitions={} multiplicity={} mode={}{} match_mode={} selector={} origin={}\n",
        automaton.clause_index,
        automaton.pattern_index,
        automaton.state_count(),
        automaton.transition_count(),
        multiplicity_label(automaton.multiplicity),
        mode_label(automaton.mode.mode),
        if automaton.mode.explicit { "(explicit)" } else { "" },
        match_mode_label(automaton.match_mode.mode),
        selector_label(automaton.selector.selector),
        origin_label(automaton.origin),
    ));
    for (index, element) in automaton.semantic.elements.iter().enumerate() {
        out.push_str(&format!("element[{index}] {}\n", explain_element(element)));
    }
    for state in &automaton.states {
        out.push_str(&format!(
            "state s{} scope=s{} accept={} origin={}\n",
            state.id.get(),
            state.scope.get(),
            state.accept,
            origin_label(state.origin),
        ));
    }
    for transition in &automaton.transitions {
        out.push_str(&format!(
            "transition t{} s{}->s{} {} scope=s{} multiplicity={} origin={}\n",
            transition.id.get(),
            transition.from.get(),
            transition.to.get(),
            transition_label(transition.kind),
            transition.scope.get(),
            multiplicity_label(transition.multiplicity),
            origin_label(transition.origin),
        ));
    }
    out
}

/// Render a lowered path set as stable multi-line debug text.
#[must_use]
pub fn explain_set(set: &LoweredPathSet) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "path_set contract={} automata={}\n",
        set.contract_version,
        set.automata.len()
    ));
    for automaton in &set.automata {
        out.push_str(&explain_automaton(automaton));
    }
    out
}

fn explain_element(element: &super::semantic::PathSemanticElement) -> String {
    match element {
        super::semantic::PathSemanticElement::Node(test) => format!(
            "node binding={} temp={} labels={} props={} scope=s{} origin={}",
            binding_label(test.binding.map(|binding| binding.get())),
            temp_label(test.temporary.map(|temp| temp.slot)),
            label_shape(&test.label),
            test.property_predicates.len(),
            test.scope.get(),
            origin_label(test.origin),
        ),
        super::semantic::PathSemanticElement::Edge(test) => format!(
            "edge {} {} orientation={} abbreviated={} labels={} props={} scope=s{} origin={}",
            exposure_label(test.exposure),
            quantifier_label(test.quantifier),
            orientation_label(test.orientation),
            test.abbreviated,
            label_shape(&test.label),
            test.property_predicates.len(),
            test.scope.get(),
            origin_label(test.origin),
        ),
    }
}

fn binding_label(binding: Option<u32>) -> String {
    binding.map_or_else(|| "-".to_owned(), |id| format!("b{id}"))
}

fn temp_label(slot: Option<u32>) -> String {
    slot.map_or_else(|| "-".to_owned(), |slot| format!("t{slot}"))
}

fn exposure_label(exposure: BindingExposure) -> String {
    match exposure {
        BindingExposure::Singleton { binding, hidden } => format!(
            "singleton({},{})",
            binding_label(binding.map(|binding| binding.get())),
            temp_label(hidden.map(|temp| temp.slot)),
        ),
        BindingExposure::ConditionalSingleton { binding, hidden } => format!(
            "conditional_singleton({},{})",
            binding_label(binding.map(|binding| binding.get())),
            temp_label(hidden.map(|temp| temp.slot)),
        ),
        BindingExposure::GroupList { binding, hidden } => format!(
            "group({},{})",
            binding_label(binding.map(|binding| binding.get())),
            temp_label(hidden.map(|temp| temp.slot)),
        ),
    }
}

fn quantifier_label(quantifier: EdgeQuantifierKind) -> String {
    match quantifier {
        EdgeQuantifierKind::Single => "single".to_owned(),
        EdgeQuantifierKind::Questioned => "questioned".to_owned(),
        EdgeQuantifierKind::Bounded { min, max } => format!("bounded({min},{max})"),
        EdgeQuantifierKind::Unbounded { min } => format!("unbounded({min}..)"),
    }
}

fn orientation_label(orientation: super::semantic::OrientationAcceptance) -> String {
    let mut mask = String::new();
    if orientation.accept_left {
        mask.push('L');
    }
    if orientation.accept_undirected {
        mask.push('U');
    }
    if orientation.accept_right {
        mask.push('R');
    }
    format!("{:?}[{mask}]", orientation.declared)
}

fn label_shape(label: &Option<crate::LabelExpr>) -> &'static str {
    match label {
        None => "none",
        Some(crate::LabelExpr::Single(_)) => "single",
        Some(crate::LabelExpr::Conjunction(_)) => "conjunction",
        Some(crate::LabelExpr::Disjunction(_)) => "disjunction",
        Some(crate::LabelExpr::Negation(_)) => "negation",
        Some(crate::LabelExpr::Wildcard) => "wildcard",
    }
}

fn transition_label(kind: TransitionKind) -> String {
    match kind {
        TransitionKind::NodeTest { test } => format!("node_test(e{test})"),
        TransitionKind::EdgeTraverse { test } => format!("edge(e{test})"),
        TransitionKind::QuantifiedEdge { test, min, max } => match max {
            Some(max) => format!("quantified(e{test},{min}..{max})"),
            None => format!("quantified(e{test},{min}..)"),
        },
        TransitionKind::Epsilon { note } => format!("epsilon({note})"),
    }
}

fn multiplicity_label(multiplicity: super::super::operator::LogicalMultiplicity) -> &'static str {
    match multiplicity {
        super::super::operator::LogicalMultiplicity::PreservesDuplicates => "preserves_duplicates",
        super::super::operator::LogicalMultiplicity::Distinct => "distinct",
    }
}

fn mode_label(mode: crate::PathMode) -> &'static str {
    match mode {
        crate::PathMode::Walk => "walk",
        crate::PathMode::Trail => "trail",
        crate::PathMode::Acyclic => "acyclic",
        crate::PathMode::Simple => "simple",
    }
}

fn match_mode_label(mode: Option<crate::MatchMode>) -> &'static str {
    match mode {
        None => "none",
        Some(crate::MatchMode::DifferentEdges) => "different_edges",
        Some(crate::MatchMode::RepeatableElements) => "repeatable_elements",
    }
}

fn selector_label(selector: Option<crate::PathSelector>) -> String {
    match selector {
        None => "none".to_owned(),
        Some(selector) => format!("{selector:?}"),
    }
}

fn origin_label(span: crate::SourceSpan) -> String {
    format!("{}+{}", span.byte_offset, span.byte_len)
}
