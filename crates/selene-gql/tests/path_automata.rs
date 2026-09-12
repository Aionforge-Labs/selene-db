//! F05-PR01 path semantic IR and automata lowering regressions.
//!
//! Each expectation below is hand-authored from the ISO rule or the tracked
//! `docs/gql/mixed-edge-orientation.md` oracle — never copied from the
//! lowerer's own output. A stored rendering of the new lowerer would prove
//! determinism only; the field-level assertions here are the independent
//! proof that binding semantics survive lowering.

use selene_gql::{
    AnalyzedStatement, BindingExposure, EdgeDirection, EdgeQuantifierKind, EmptyProcedureRegistry,
    GqlStatus, GqlType, LabelExpr, LogicalMultiplicity, NodeTest, PATH_AUTOMATA_CONTRACT_VERSION,
    PathLoweringLimits, PathMode, PathSemanticElement, PlannerError, TransitionKind, analyze,
    explain_set, lower_path_automata_with_defaults, measure_path_lowering, parse,
    supported_path_inventory,
};

fn analyzed(source: &str) -> AnalyzedStatement {
    let statement = parse(source).expect("test input parses");
    analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes")
}

fn lowered(source: &str) -> selene_gql::LoweredPathSet {
    lower_path_automata_with_defaults(&analyzed(source)).expect("test input lowers")
}

/// Expect one node element test (independent of future enum growth).
fn expect_node(element: &PathSemanticElement) -> &NodeTest {
    let PathSemanticElement::Node(test) = element else {
        panic!("expected node element, got {element:?}");
    };
    test
}

/// Expect one edge element test (independent of future enum growth).
fn expect_edge(element: &PathSemanticElement) -> &selene_gql::EdgeTest {
    let PathSemanticElement::Edge(test) = element else {
        panic!("expected edge element, got {element:?}");
    };
    test
}

/// True for the questioned-skip epsilon (independent of future kinds).
fn is_questioned_skip(kind: TransitionKind) -> bool {
    match kind {
        TransitionKind::Epsilon { note } => note == "questioned_skip",
        TransitionKind::NodeTest { .. }
        | TransitionKind::EdgeTraverse { .. }
        | TransitionKind::QuantifiedEdge { .. } => false,
        _ => false,
    }
}

/// True for any epsilon transition (independent of future kinds).
fn is_epsilon(kind: TransitionKind) -> bool {
    match kind {
        TransitionKind::Epsilon { .. } => true,
        TransitionKind::NodeTest { .. }
        | TransitionKind::EdgeTraverse { .. }
        | TransitionKind::QuantifiedEdge { .. } => false,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Questioned (?) vs bounded {0,1}: conditional singleton vs group.
// ---------------------------------------------------------------------------

#[test]
fn questioned_and_bounded_01_keep_distinct_binding_metadata() {
    let questioned = lowered("MATCH (a)-[r?]->(b) RETURN r");
    let bounded = lowered("MATCH (a)-[r{0,1}]->(b) RETURN r");
    assert_eq!(questioned.automata.len(), 1);
    assert_eq!(bounded.automata.len(), 1);
    let question_automaton = &questioned.automata[0];
    let bounded_automaton = &bounded.automata[0];

    let question_edge = expect_edge(&question_automaton.semantic.elements[1]);
    let bounded_edge = expect_edge(&bounded_automaton.semantic.elements[1]);
    // Same traversal lengths, observably different variable exposure.
    assert_eq!(question_edge.quantifier, EdgeQuantifierKind::Questioned);
    assert!(question_edge.exposure.is_conditional_singleton());
    assert_eq!(
        bounded_edge.quantifier,
        EdgeQuantifierKind::Bounded { min: 0, max: 1 }
    );
    assert!(bounded_edge.exposure.is_group());

    // Automaton shapes differ: the questioned primary carries an epsilon skip
    // plus an edge take; {0,1} is one quantified group transition.
    assert_eq!(question_automaton.transitions.len(), 4);
    assert!(
        question_automaton
            .transitions
            .iter()
            .any(|transition| is_questioned_skip(transition.kind)),
        "questioned automaton must carry the skip branch"
    );
    assert_eq!(bounded_automaton.transitions.len(), 3);
    assert!(
        bounded_automaton
            .transitions
            .iter()
            .all(|transition| !is_epsilon(transition.kind)),
        "bounded {{0,1}} must not gain a questioned-style skip"
    );
    assert_eq!(
        bounded_automaton.quantifier_bounds(),
        vec![(0, Some(1))],
        "{{0,1}} keeps its group bounds on the transition"
    );
}

// ---------------------------------------------------------------------------
// Concatenation + selected alternation preserve scope and duplicates.
// ---------------------------------------------------------------------------

#[test]
fn concatenation_preserves_scope_and_multiset_semantics() {
    let set = lowered("MATCH (a:Person)-[r:KNOWS]->(b) RETURN a, r, b");
    assert_eq!(set.automata.len(), 1);
    let automaton = &set.automata[0];
    assert_eq!(automaton.semantic.elements.len(), 3);
    assert_eq!(automaton.state_count(), 4);
    assert_eq!(automaton.transition_count(), 3);
    // Every transition resolves in the statement scope and preserves
    // duplicates: binding tables may contain duplicates (§4.3.6), so nothing
    // here silently deduplicates (set vs multiset stays downstream).
    for transition in &automaton.transitions {
        assert_eq!(transition.scope, automaton.semantic.scope);
        assert_eq!(
            transition.multiplicity,
            LogicalMultiplicity::PreservesDuplicates,
            "{transition:?}"
        );
    }
    assert!(automaton.preserves_duplicates());
    assert!(set.preserves_duplicates());
    assert_eq!(automaton.ordering(), selene_gql::LogicalOrdering::Preserved);
}

#[test]
fn comma_patterns_share_repeated_declarations_without_copying_them() {
    let set = lowered("MATCH (a)-[:K]->(b), (b)-[:L]->(c) RETURN a, b, c");
    assert_eq!(set.automata.len(), 2);
    let first_b = expect_node(&set.automata[0].semantic.elements[2])
        .binding
        .expect("b is named in the first pattern");
    let second_b = expect_node(&set.automata[1].semantic.elements[0])
        .binding
        .expect("b is reused in the second pattern");
    // One declaration, two uses: the reuse resolves to the same identity.
    assert_eq!(first_b, second_b);
    // Per-pattern automata keep their own clause/pattern coordinates.
    assert_eq!(set.automata[0].pattern_index, 0);
    assert_eq!(set.automata[1].pattern_index, 1);
    assert_eq!(set.automata[0].clause_index, set.automata[1].clause_index);
}

#[test]
fn label_disjunction_is_selected_alternation_with_preserved_scope() {
    let set = lowered("MATCH (n:A|B) RETURN n");
    assert_eq!(set.automata.len(), 1);
    let automaton = &set.automata[0];
    assert_eq!(automaton.semantic.elements.len(), 1);
    let node = expect_node(&automaton.semantic.elements[0]);
    let Some(LabelExpr::Disjunction(parts)) = &node.label else {
        panic!(
            "expected the disjunctive label predicate, got {:?}",
            node.label
        );
    };
    assert_eq!(parts.len(), 2);
    // The alternation rides one test transition with scope and multiplicity
    // preserved — no exponential branch expansion in this layer.
    assert_eq!(automaton.transitions.len(), 1);
    assert_eq!(
        automaton.transitions[0].multiplicity,
        LogicalMultiplicity::PreservesDuplicates
    );
    assert_eq!(automaton.transitions[0].scope, automaton.semantic.scope);
}

// ---------------------------------------------------------------------------
// Nested quantifiers and local modes never collapse into one global rule.
// ---------------------------------------------------------------------------

#[test]
fn nested_quantifiers_keep_per_transition_bounds() {
    let set = lowered("MATCH (a)-[r:K*1..2]->(m)-[s:L*3..4]->(b) RETURN r, s");
    assert_eq!(set.automata.len(), 1);
    let automaton = &set.automata[0];
    // Each quantified edge keeps its own scope; the pair is never merged.
    assert_eq!(
        automaton.quantifier_bounds(),
        vec![(1, Some(2)), (3, Some(4))],
    );
    assert_eq!(automaton.state_count(), 6);
    assert_eq!(automaton.transition_count(), 5);
}

#[test]
fn local_path_modes_stay_per_automaton() {
    let set = lowered("MATCH TRAIL (a)-[:K]->(b) MATCH ACYCLIC (c)-[:L]->(d) RETURN a, b, c, d");
    assert_eq!(set.automata.len(), 2);
    assert_eq!(set.automata[0].mode.mode, PathMode::Trail);
    assert!(set.automata[0].mode.explicit);
    assert_eq!(set.automata[1].mode.mode, PathMode::Acyclic);
    assert!(set.automata[1].mode.explicit);
    assert_ne!(set.automata[0].mode.mode, set.automata[1].mode.mode);
}

// ---------------------------------------------------------------------------
// Mixed-edge orientation tokens lower to the expected acceptance masks.
// ---------------------------------------------------------------------------

/// Hand-written from `docs/gql/mixed-edge-orientation.md`: full spelling,
/// declared token, and the (left, undirected, right) acceptance triple.
const ORIENTATION_CASES: &[(&str, EdgeDirection, (bool, bool, bool))] = &[
    ("(a)-[e]->(b)", EdgeDirection::Right, (false, false, true)),
    ("(a)<-[e]-(b)", EdgeDirection::Left, (true, false, false)),
    (
        "(a)~[e]~(b)",
        EdgeDirection::Undirected,
        (false, true, false),
    ),
    (
        "(a)<~[e]~(b)",
        EdgeDirection::LeftOrUndirected,
        (true, true, false),
    ),
    (
        "(a)~[e]~>(b)",
        EdgeDirection::UndirectedOrRight,
        (false, true, true),
    ),
    (
        "(a)<-[e]->(b)",
        EdgeDirection::LeftOrRight,
        (true, false, true),
    ),
    ("(a)-[e]-(b)", EdgeDirection::Any, (true, true, true)),
];

#[test]
fn mixed_edge_tokens_lower_to_expected_acceptance_tests() {
    for (pattern, token, (left, undirected, right)) in ORIENTATION_CASES {
        let source = format!("MATCH {pattern} RETURN e");
        let set = lowered(&source);
        assert_eq!(set.automata.len(), 1, "{pattern}");
        let edge = expect_edge(&set.automata[0].semantic.elements[1]);
        // Intrinsic directionality stays a runtime property; the pattern
        // records the declared token plus the derived acceptance mask.
        assert_eq!(edge.orientation.declared, *token, "{pattern}");
        assert_eq!(edge.orientation.accept_left, *left, "{pattern}");
        assert_eq!(edge.orientation.accept_undirected, *undirected, "{pattern}");
        assert_eq!(edge.orientation.accept_right, *right, "{pattern}");
    }
}

#[test]
fn abbreviated_spelling_shares_the_full_form_acceptance() {
    let full = lowered("MATCH (a)-[e]->(b) RETURN e");
    let abbreviated = lowered("MATCH (a)->(b) RETURN 1");
    let full_edge = expect_edge(&full.automata[0].semantic.elements[1]);
    let abbreviated_edge = expect_edge(&abbreviated.automata[0].semantic.elements[1]);
    assert!(!full_edge.abbreviated);
    assert!(abbreviated_edge.abbreviated);
    assert_eq!(
        (
            abbreviated_edge.orientation.accept_left,
            abbreviated_edge.orientation.accept_undirected,
            abbreviated_edge.orientation.accept_right,
        ),
        (
            full_edge.orientation.accept_left,
            full_edge.orientation.accept_undirected,
            full_edge.orientation.accept_right,
        ),
        "abbreviated `->` accepts exactly what full `-[]->` accepts"
    );
}

// ---------------------------------------------------------------------------
// Invalid unbounded patterns are rejected by rule, never hop-capped.
// ---------------------------------------------------------------------------

#[test]
fn ungated_unbounded_is_rejected_without_a_hop_cap() {
    // The analyzer enforces the same ISO §16.4 gate first: a bare WALK
    // unbounded pattern never reaches lowering.
    let statement = parse("MATCH (a)-[r:K*]->(b) RETURN r").expect("parses");
    let err = analyze(statement, &EmptyProcedureRegistry, None)
        .expect_err("ungated unbounded must fail analysis");
    assert_eq!(
        err.gqlstatus(),
        GqlStatus::SYNTAX_ERROR,
        "finite-result violation is a syntax-rule error, not a limit"
    );
}

#[test]
fn gated_unbounded_forms_lower_with_open_bounds() {
    for source in [
        "MATCH TRAIL (a)-[r:K*]->(b) RETURN r",
        "MATCH ANY (a)-[r:K*]->(b) RETURN r",
        "MATCH DIFFERENT EDGES (a)-[r:K*]->(b) RETURN r",
    ] {
        let set = lowered(source);
        assert_eq!(set.automata.len(), 1, "{source}");
        let edge = expect_edge(&set.automata[0].semantic.elements[1]);
        assert_eq!(
            edge.quantifier,
            EdgeQuantifierKind::Unbounded { min: 0 },
            "{source}"
        );
        // No arbitrary runtime hop cap is substituted for the language rule.
        assert_eq!(
            set.automata[0].quantifier_bounds(),
            vec![(0, None)],
            "{source}"
        );
    }
}

#[test]
fn lowering_backstop_error_carries_the_rule_status() {
    // The lowering backstop behind the analyzer gate (pinned by the
    // crate-internal mutation test in `gates.rs`) reports the ISO §16.4
    // finite-result rule — never a hop-cap program limit.
    let err = PlannerError::UnboundedPathRequiresGate {
        mode: PathMode::Walk,
        selector: None,
        match_mode: None,
        span: selene_gql::SourceSpan::default(),
    };
    assert_eq!(err.gqlstatus(), GqlStatus::SYNTAX_ERROR);
    assert!(
        format!("{err}").contains("selective path selector"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Source spans stay useful after temporaries are introduced.
// ---------------------------------------------------------------------------

#[test]
fn temporaries_inherit_source_spans_within_the_pattern() {
    let settled = analyzed("MATCH ()-[]->() RETURN 1");
    let set = lower_path_automata_with_defaults(&settled).expect("anonymous pattern lowers");
    assert_eq!(set.automata.len(), 1);
    let automaton = &set.automata[0];
    let pattern_origin = automaton.origin;
    let mut temporaries = 0;
    for element in &automaton.semantic.elements {
        let (temporary, origin) = if let PathSemanticElement::Node(test) = element {
            (test.temporary, test.origin)
        } else if let PathSemanticElement::Edge(test) = element {
            let hidden = match test.exposure {
                BindingExposure::Singleton { hidden, .. }
                | BindingExposure::ConditionalSingleton { hidden, .. }
                | BindingExposure::GroupList { hidden, .. } => hidden,
                _ => panic!("future exposure variant"),
            };
            (hidden, test.origin)
        } else {
            panic!("future element variant");
        };
        // The element span itself is non-empty and inside the pattern span.
        assert!(origin.byte_len > 0, "{element:?}");
        assert!(
            origin.byte_offset >= pattern_origin.byte_offset
                && origin.end() <= pattern_origin.end(),
            "element span {origin:?} must sit inside the pattern {pattern_origin:?}"
        );
        if let Some(temporary) = temporary {
            temporaries += 1;
            // The temporary is anchored at its source element span, so a
            // diagnostic on the slot still points at useful source text.
            assert_eq!(temporary.origin, origin);
            assert!(temporary.origin.byte_len > 0);
        }
    }
    assert_eq!(temporaries, 3, "two nodes plus one edge are anonymous");
}

// ---------------------------------------------------------------------------
// Independently authored binding metadata.
// ---------------------------------------------------------------------------

#[test]
fn binding_metadata_matches_hand_written_expectations() {
    let settled = analyzed("MATCH (a:Person)-[r:KNOWS]->(b) RETURN a, r, b");
    let set = lower_path_automata_with_defaults(&settled).expect("lowers");
    assert_eq!(set.automata.len(), 1);
    let automaton = &set.automata[0];

    // Expected declaration order from the source text: a, r, b.
    let expected_names = ["a", "r", "b"];
    let mut expected_ids = Vec::new();
    for name in expected_names {
        let id = settled
            .scopes
            .declarations()
            .iter()
            .find(|decl| decl.name().as_str() == name)
            .unwrap_or_else(|| panic!("analyzer declares {name}"))
            .id();
        expected_ids.push(id);
    }
    assert_eq!(automaton.semantic.named_bindings(), expected_ids);

    let node_a = expect_node(&automaton.semantic.elements[0]);
    assert_eq!(node_a.binding, Some(expected_ids[0]));
    assert_eq!(
        node_a.ty,
        selene_gql::AnalyzedType::Resolved(GqlType::NodeRef)
    );
    let Some(LabelExpr::Single(label)) = &node_a.label else {
        panic!("expected the :Person label, got {:?}", node_a.label);
    };
    assert_eq!(label.as_str(), "Person");

    let edge_r = expect_edge(&automaton.semantic.elements[1]);
    assert_eq!(
        edge_r.exposure,
        BindingExposure::Singleton {
            binding: Some(expected_ids[1]),
            hidden: None,
        }
    );
    assert_eq!(edge_r.quantifier, EdgeQuantifierKind::Single);
    assert_eq!(edge_r.orientation.declared, EdgeDirection::Right);
    let Some(LabelExpr::Single(label)) = &edge_r.label else {
        panic!("expected the :KNOWS label, got {:?}", edge_r.label);
    };
    assert_eq!(label.as_str(), "KNOWS");

    let node_b = expect_node(&automaton.semantic.elements[2]);
    assert_eq!(node_b.binding, Some(expected_ids[2]));
}

// ---------------------------------------------------------------------------
// Inventory, contract version, and stable debug fixtures.
// ---------------------------------------------------------------------------

#[test]
fn inventory_contract_and_debug_fixture() {
    let inventory = supported_path_inventory();
    for feature in [
        "G002", "G010", "G016", "G019", "G036", "G037", "G060", "G061", "GH02",
    ] {
        assert!(inventory.supports_feature(feature), "{feature}");
    }
    assert!(!inventory.supports_feature("G999"));
    assert!(inventory.supports_syntax("concatenation"));
    assert!(!inventory.supports_syntax("path_pattern_pipe_alternation"));
    assert_eq!(PATH_AUTOMATA_CONTRACT_VERSION, 1);

    let set = lowered("MATCH (a)-[r?]->(b) RETURN r");
    assert_eq!(set.contract_version, PATH_AUTOMATA_CONTRACT_VERSION);
    let text = explain_set(&set);
    assert!(text.contains("path_automaton"), "{text}");
    assert!(text.contains("conditional_singleton"), "{text}");
    assert!(text.contains("origin="), "{text}");
    assert!(!text.contains("0x"), "{text}");

    let bounded = lowered("MATCH (a)-[r{0,1}]->(b) RETURN r");
    let bounded_text = explain_set(&bounded);
    assert!(bounded_text.contains("group("), "{bounded_text}");
    assert!(
        !bounded_text.contains("conditional_singleton"),
        "{bounded_text}"
    );
}

// ---------------------------------------------------------------------------
// Observed lowering cost and explicit resource limits.
// ---------------------------------------------------------------------------

#[test]
#[allow(
    clippy::print_stderr,
    reason = "measurement output for the handoff, not production logging"
)]
fn lowering_cost_and_resource_limits_are_observed() {
    let settled = analyzed("MATCH (a)-[r:K*1..5]->(m)-[s:L*2..6]->(b) RETURN r, s");
    let (micros, stats) = measure_path_lowering(&settled, &PathLoweringLimits::DEFAULT);
    eprintln!("path lowering cost: {micros}us stats={stats:?}");
    assert_eq!(stats.len(), 1);
    // Five elements lower to six states and five transitions: linear in the
    // source, with one transition per quantified edge (never expanded).
    assert_eq!(stats[0].states, 6);
    assert_eq!(stats[0].transitions, 5);

    // Pathological source expansion fails before allocation: a two-branch
    // label alternation under a one-branch cap.
    let disjunct = analyzed("MATCH (n:A|B) RETURN n");
    let tight = PathLoweringLimits::DEFAULT.with_max_label_branches(1);
    let err = selene_gql::lower_path_automata(&disjunct, &tight)
        .expect_err("disjunction over the branch cap must fail");
    let PlannerError::ProgramLimitExceeded { limit_name, .. } = err else {
        panic!("expected a resource-limit error, got {err:?}");
    };
    assert_eq!(limit_name, "max_path_label_branches");

    // A three-element pattern under a two-state cap fails before allocating
    // the state vector.
    let tiny = PathLoweringLimits::DEFAULT.with_max_states(2);
    let err = selene_gql::lower_path_automata(&settled, &tiny)
        .expect_err("pattern over the state cap must fail");
    let PlannerError::ProgramLimitExceeded { limit_name, .. } = err else {
        panic!("expected a resource-limit error, got {err:?}");
    };
    assert_eq!(limit_name, "max_path_automaton_states");
}
