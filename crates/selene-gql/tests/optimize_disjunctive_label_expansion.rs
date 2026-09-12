//! BRIEF-155 Commit 3 — registry-driven `DisjunctiveLabelExpansion` rule
//! tests.

use selene_core::DbString;
use selene_gql::{
    EmptyProcedureRegistry, ExecutionPlan, IndexKind, JoinTree, LabelExpr, NodeOrEdgeScan,
    ScanAccess, Vec2OrMore, analyze, optimize, parse, plan,
};
use selene_testing::MockIndexCatalog;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn optimized(source: &str, catalog: &MockIndexCatalog) -> ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans");
    let ctx = selene_gql::OptimizeContext::default().with_index_catalog(catalog);
    optimize(plan, &ctx)
}

fn leaf(tree: &JoinTree) -> &JoinTree {
    match tree {
        JoinTree::Expand { child, .. } => leaf(child),
        JoinTree::HashJoin { left, .. } | JoinTree::Outer { left, .. } => leaf(left),
        _ => tree,
    }
}

fn branches_of(tree: &JoinTree) -> Option<&[NodeOrEdgeScan]> {
    match leaf(tree) {
        JoinTree::DisjunctiveScan { branches, .. } => Some(branches.as_slice()),
        _ => None,
    }
}

fn first_scan(tree: &JoinTree) -> Option<&NodeOrEdgeScan> {
    match leaf(tree) {
        JoinTree::Scan(scan) => Some(scan),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Catalogs
// ---------------------------------------------------------------------------

/// Per-label `email` STRING index on three of the four candidate labels.
fn email_indexed_catalog() -> MockIndexCatalog {
    MockIndexCatalog::new()
        .with_node_typed_index(db_string("Module"), db_string("email"), IndexKind::String)
        .with_node_typed_index(
            db_string("Namespace"),
            db_string("email"),
            IndexKind::String,
        )
        .with_node_typed_index(db_string("Class"), db_string("email"), IndexKind::String)
}

/// Per-label `age` INTEGER index — covers range and equality.
fn age_indexed_catalog() -> MockIndexCatalog {
    MockIndexCatalog::new()
        .with_node_typed_index(db_string("Person"), db_string("age"), IndexKind::Integer)
        .with_node_typed_index(db_string("Robot"), db_string("age"), IndexKind::Integer)
}

/// Composite index on (tenant, kind) for each label in the disjunction.
fn composite_catalog() -> MockIndexCatalog {
    MockIndexCatalog::new()
        .with_node_composite_index(
            db_string("A"),
            vec![
                (db_string("tenant"), IndexKind::String),
                (db_string("kind"), IndexKind::String),
            ],
        )
        .with_node_composite_index(
            db_string("B"),
            vec![
                (db_string("tenant"), IndexKind::String),
                (db_string("kind"), IndexKind::String),
            ],
        )
}

fn no_index_catalog() -> MockIndexCatalog {
    MockIndexCatalog::new()
}

// ---------------------------------------------------------------------------
// Positive: rule fires for the four index shapes
// ---------------------------------------------------------------------------

#[test]
fn flat_disjunction_equality_fires() {
    let plan = optimized(
        "MATCH (n:Module|Namespace|Class) WHERE n.email = 'foo' RETURN n",
        &email_indexed_catalog(),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    let branches = branches_of(&pattern.join_tree)
        .expect("flat disjunction with indexed branch expanded to DisjunctiveScan");
    assert_eq!(branches.len(), 3);
    // Every branch carries a single-label predicate.
    for branch in branches {
        assert!(matches!(branch.label_predicate, Some(LabelExpr::Single(_))));
    }
    // At least one branch picked TypedIndexRange via the slot-8 rule.
    let typed_branches = branches
        .iter()
        .filter(|branch| matches!(branch.access, ScanAccess::TypedIndexRange { .. }))
        .count();
    assert!(
        typed_branches >= 1,
        "expected at least one TypedIndexRange branch, got {typed_branches}"
    );
}

#[test]
fn flat_disjunction_range_fires() {
    let plan = optimized(
        "MATCH (n:Person|Robot) WHERE n.age > 30 RETURN n",
        &age_indexed_catalog(),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    let branches = branches_of(&pattern.join_tree).expect("expansion fires for range");
    assert_eq!(branches.len(), 2);
    for branch in branches {
        assert!(matches!(branch.access, ScanAccess::TypedIndexRange { .. }));
    }
}

#[test]
fn flat_disjunction_composite_fires() {
    let plan = optimized(
        "MATCH (n:A|B) WHERE n.tenant = 't1' AND n.kind = 'person' RETURN n",
        &composite_catalog(),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    let branches = branches_of(&pattern.join_tree).expect("expansion fires for composite");
    assert_eq!(branches.len(), 2);
    for branch in branches {
        assert!(matches!(branch.access, ScanAccess::CompositeLookup { .. }));
    }
}

#[test]
fn flat_disjunction_in_list_fires() {
    let plan = optimized(
        "MATCH (n:Person|Robot) WHERE n.age IN [1, 2, 3] RETURN n",
        &age_indexed_catalog(),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    let branches = branches_of(&pattern.join_tree).expect("expansion fires for IN list");
    assert_eq!(branches.len(), 2);
    for branch in branches {
        assert!(
            matches!(branch.access, ScanAccess::BitmapUnion { .. }),
            "expected BitmapUnion, got {:?}",
            branch.access
        );
    }
}

// ---------------------------------------------------------------------------
// Negative: rule MUST stay Linear
// ---------------------------------------------------------------------------

#[test]
fn no_applicable_index_disjunction_stays_linear() {
    let plan = optimized(
        "MATCH (n:Module|Namespace|Class) WHERE n.email = 'foo' RETURN n",
        &no_index_catalog(),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    assert!(
        branches_of(&pattern.join_tree).is_none(),
        "no-applicable-index disjunction must not expand to DisjunctiveScan"
    );
    let scan = first_scan(&pattern.join_tree).expect("plain Scan present");
    assert!(matches!(scan.access, ScanAccess::Linear));
    assert!(matches!(
        scan.label_predicate,
        Some(LabelExpr::Disjunction(_))
    ));
}

#[test]
fn mixed_inner_branch_conjunction_stays_linear() {
    // `A|(B&C)` — middle branch is a Conjunction, not a Single. Helper
    // returns None; rule must not fire.
    let plan = optimized(
        "MATCH (n:A|B&C) WHERE n.email = 'foo' RETURN n",
        &MockIndexCatalog::new().with_node_typed_index(
            db_string("A"),
            db_string("email"),
            IndexKind::String,
        ),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    assert!(
        branches_of(&pattern.join_tree).is_none(),
        "mixed Conjunction inner must not expand"
    );
}

#[test]
fn negation_inner_branch_stays_linear() {
    // `A|!B` — second branch is a Negation.
    let plan = optimized(
        "MATCH (n:A|!B) WHERE n.email = 'foo' RETURN n",
        &MockIndexCatalog::new().with_node_typed_index(
            db_string("A"),
            db_string("email"),
            IndexKind::String,
        ),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    assert!(
        branches_of(&pattern.join_tree).is_none(),
        "Negation inner must not expand"
    );
}

#[test]
fn single_label_preserved_no_false_expansion() {
    // The F3 idempotency contract: `Single(L)` returns None from the helper
    // so the rule never wraps it into a DisjunctiveScan with a single branch.
    let plan = optimized(
        "MATCH (n:Person) WHERE n.age = 30 RETURN n",
        &age_indexed_catalog(),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    assert!(
        branches_of(&pattern.join_tree).is_none(),
        "single-label MATCH must not produce DisjunctiveScan"
    );
    let scan = first_scan(&pattern.join_tree).expect("plain Scan present");
    // The slot-8 range rule still fires per branch — confirms cleanup
    // ordering hasn't been disrupted.
    assert!(matches!(scan.access, ScanAccess::TypedIndexRange { .. }));
}

#[test]
fn fixed_point_convergence_no_re_fire_on_expanded_branches() {
    // The expansion produces N branches each with `Single(L_i)`. On the
    // next optimizer pass `flat_disjunction_singles` returns None for
    // each branch (F3 idempotency), so the rule does NOT re-fire.
    //
    // The optimizer drives rules to a fixed point. If the rule mis-handled
    // idempotency, we'd see nested `DisjunctiveScan` (or a hang in the
    // iteration cap). We assert directly that no branch is itself a
    // `DisjunctiveScan`.
    let plan = optimized(
        "MATCH (n:Module|Namespace|Class) WHERE n.email = 'foo' RETURN n",
        &email_indexed_catalog(),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    let branches = branches_of(&pattern.join_tree).expect("expansion fires");
    assert_eq!(branches.len(), 3);
    for branch in branches {
        // A branch is a `NodeOrEdgeScan` directly — proves no second-level
        // wrap occurred.
        assert!(matches!(branch.label_predicate, Some(LabelExpr::Single(_))));
    }
}

#[test]
fn edge_label_disjunction_stays_linear() {
    // F6 — no edge-index rule fires at HEAD; rule gates on
    // `ScanKind::Node`. Edge disjunctive labels must NOT expand.
    let plan = optimized(
        "MATCH ()-[r:R1|R2|R3]->() RETURN r",
        &MockIndexCatalog::new(),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    assert!(
        branches_of(&pattern.join_tree).is_none(),
        "edge disjunction must not expand"
    );
}

// ---------------------------------------------------------------------------
// Cross-cutting: registry slot
// ---------------------------------------------------------------------------

#[test]
fn rule_registered_at_slot_5() {
    // The brief pins position 5 (between expand_filter_pushdown at slot 4
    // and composite_index_lookup at slot 6). Cross-check via the public
    // `RULE_NAMES` list so a misordering surfaces here, not via subtle
    // plan drift.
    let expected = [
        "constant_folding",
        "and_splitting",
        "filter_pushdown",
        "node_filter_extraction",
        "expand_filter_pushdown",
        "disjunctive_label_expansion",
        "composite_index_lookup",
        "in_list_optimization",
        "range_index_scan",
    ];
    for (slot, name) in expected.iter().enumerate() {
        assert_eq!(
            selene_gql::plan::optimize::RULE_NAMES[slot],
            *name,
            "slot {slot} expected {name} got {}",
            selene_gql::plan::optimize::RULE_NAMES[slot]
        );
    }
}

// ---------------------------------------------------------------------------
// Helper unit tests — moved out of `src/` per dos_guard.rs.
// ---------------------------------------------------------------------------

// Probe `flat_disjunction_singles` indirectly by checking the rule's
// observable behaviour on a hand-constructed label shape. Because the
// helper is `pub(super)` in `index_helpers.rs`, we exercise it via the
// rule's end-to-end behaviour — these tests give finer-grained shape
// coverage than the broader fires/stays-linear suite above.

fn disjunction(parts: Vec<LabelExpr>) -> LabelExpr {
    LabelExpr::Disjunction(Vec2OrMore::try_from_vec(parts).expect("≥ 2 labels"))
}

#[test]
fn helper_handles_three_singles_via_full_planner() {
    // Three-label disjunction with email index on all three should fire
    // and produce exactly three branches in the IR — confirming
    // flat_disjunction_singles extracts every label, not just the first.
    let catalog = MockIndexCatalog::new()
        .with_node_typed_index(db_string("X"), db_string("email"), IndexKind::String)
        .with_node_typed_index(db_string("Y"), db_string("email"), IndexKind::String)
        .with_node_typed_index(db_string("Z"), db_string("email"), IndexKind::String);
    let plan = optimized("MATCH (n:X|Y|Z) WHERE n.email = 'foo' RETURN n", &catalog);
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    let branches = branches_of(&pattern.join_tree).expect("expansion fires");
    assert_eq!(branches.len(), 3);
}

#[test]
fn helper_rejects_disjunction_with_intermediate_conjunction() {
    // `A|(B&C)|D` — middle branch is not a single, helper returns None.
    let plan = optimized(
        "MATCH (n:A|B&C|D) WHERE n.email = 'foo' RETURN n",
        &MockIndexCatalog::new().with_node_typed_index(
            db_string("A"),
            db_string("email"),
            IndexKind::String,
        ),
    );
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan present");
    assert!(branches_of(&pattern.join_tree).is_none());
}

#[test]
fn vec2ormore_round_trip_via_disjunction() {
    // Sanity guard: confirm Vec2OrMore::try_from_vec accepts the construction
    // shape used by the rule's clone-and-stamp path. If this regresses, the
    // rule's `branches: labels.iter().map(...).collect()` would also break.
    let parts = vec![
        LabelExpr::Single(db_string("A")),
        LabelExpr::Single(db_string("B")),
    ];
    let expr = disjunction(parts);
    assert!(matches!(expr, LabelExpr::Disjunction(_)));
}
