//! OPT-5 cost-gate tests: the cost model changes the *chosen plan* (index vs
//! Linear, and which composite index) while never changing results.
//!
//! These exercise the optimizer directly with a `MockIndexCatalog` carrying
//! injected synthetic statistics, so the gate decisions are deterministic.

use selene_core::{DbString, Value};
use selene_gql::{
    EmptyProcedureRegistry, IndexKind, JoinTree, NodeOrEdgeScan, ScanAccess, analyze, optimize,
    parse, plan,
};
use selene_testing::MockIndexCatalog;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn optimized(source: &str, catalog: &MockIndexCatalog) -> selene_gql::ExecutionPlan {
    let statement = parse(source).expect("parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("plans");
    let ctx = selene_gql::OptimizeContext::default().with_index_catalog(catalog);
    optimize(plan, &ctx)
}

fn first_scan(tree: &JoinTree) -> Option<&NodeOrEdgeScan> {
    match tree {
        JoinTree::Scan(scan) => Some(scan),
        JoinTree::Expand { child, .. } => first_scan(child),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            first_scan(left).or_else(|| first_scan(right))
        }
        _ => None,
    }
}

fn scan_access(plan: &selene_gql::ExecutionPlan) -> ScanAccess {
    first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree)
        .unwrap()
        .access
        .clone()
}

// ---------------------------------------------------------------------------
// Label-scan index-vs-Linear gate.
// ---------------------------------------------------------------------------

#[test]
fn label_scan_high_selectivity_promotes_to_label_index() {
    // 42 of 1000 rows carry `Person` → the label index is far cheaper.
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(db_string("Person"))
        .with_total_rows(selene_gql::IndexTarget::Node, 1000)
        .with_label_cardinality(db_string("Person"), 42);
    let plan = optimized("MATCH (n:Person) RETURN n", &catalog);
    assert!(
        matches!(scan_access(&plan), ScanAccess::LabelIndex { .. }),
        "selective label must promote, got {:?}",
        scan_access(&plan)
    );
}

#[test]
fn label_scan_low_selectivity_stays_linear() {
    // Every row carries `Person` (1000 of 1000) → label bitmap == full scan, so
    // the index is NOT cheaper and the scan stays Linear.
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(db_string("Person"))
        .with_total_rows(selene_gql::IndexTarget::Node, 1000)
        .with_label_cardinality(db_string("Person"), 1000);
    let plan = optimized("MATCH (n:Person) RETURN n", &catalog);
    assert!(
        matches!(scan_access(&plan), ScanAccess::Linear),
        "non-selective label must stay Linear, got {:?}",
        scan_access(&plan)
    );
}

#[test]
fn label_scan_without_stats_promotes_as_before() {
    // No cardinality stats injected → gate is a no-op → structural promote.
    let catalog = MockIndexCatalog::new().with_node_label_index(db_string("Person"));
    let plan = optimized("MATCH (n:Person) RETURN n", &catalog);
    assert!(
        matches!(scan_access(&plan), ScanAccess::LabelIndex { .. }),
        "no-stats label must promote (pre-OPT-5 behavior), got {:?}",
        scan_access(&plan)
    );
}

// ---------------------------------------------------------------------------
// Typed equality index-vs-Linear gate.
// ---------------------------------------------------------------------------

#[test]
fn equality_unique_value_uses_typed_index() {
    // age = 30 matches 3 rows out of a 500-row Person population → use the index.
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(db_string("Person"))
        .with_node_typed_index(db_string("Person"), db_string("age"), IndexKind::Integer)
        .with_label_cardinality(db_string("Person"), 500)
        .with_equality_cardinality(db_string("Person"), db_string("age"), Value::Int(30), 3);
    let plan = optimized("MATCH (n:Person) FILTER n.age = 30 RETURN n", &catalog);
    assert!(
        matches!(scan_access(&plan), ScanAccess::TypedIndexRange { .. }),
        "selective equality must use the typed index, got {:?}",
        scan_access(&plan)
    );
}

#[test]
fn equality_near_constant_declines_typed_index() {
    // age = 30 matches every Person row (500 of 500) → the typed index is no
    // cheaper than the label-scoped residual baseline, so the range rule
    // declines. The bare label scan then promotes to a LabelIndex (still
    // row-equivalent); the key assertion is that the *typed property* index was
    // NOT selected.
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(db_string("Person"))
        .with_node_typed_index(db_string("Person"), db_string("age"), IndexKind::Integer)
        .with_label_cardinality(db_string("Person"), 500)
        .with_equality_cardinality(db_string("Person"), db_string("age"), Value::Int(30), 500);
    let plan = optimized("MATCH (n:Person) FILTER n.age = 30 RETURN n", &catalog);
    assert!(
        !matches!(scan_access(&plan), ScanAccess::TypedIndexRange { .. }),
        "near-constant equality must NOT use the typed index, got {:?}",
        scan_access(&plan)
    );
}

// ---------------------------------------------------------------------------
// Composite cost-ranking: two indexes match, cheaper one wins.
// ---------------------------------------------------------------------------

#[test]
fn composite_ranking_picks_cheaper_index() {
    // Two composite indexes match the (a, b, c) equality set: (a, b) and (b, c).
    // (b, c) is cheaper by injected composite cardinality, so it must be chosen
    // even though (a, b) is not smaller as a subset (both size 2).
    let label = db_string("Doc");
    let a = db_string("a");
    let b = db_string("b");
    let c = db_string("c");
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(label.clone())
        .with_node_composite_index(label.clone(), vec![(a, IndexKind::Integer), (b.clone(), IndexKind::Integer)])
        .with_node_composite_index(label.clone(), vec![(b, IndexKind::Integer), (c, IndexKind::Integer)])
        .with_label_cardinality(label.clone(), 1000)
        // (a, b) canonical-sorted keys [a<b] → [1, 2]; expensive.
        .with_composite_cardinality(label.clone(), vec![Value::Int(1), Value::Int(2)], 200)
        // (b, c) canonical-sorted keys [b<c] → [2, 3]; cheap.
        .with_composite_cardinality(label, vec![Value::Int(2), Value::Int(3)], 5);
    let plan = optimized(
        "MATCH (n:Doc) FILTER n.a = 1 AND n.b = 2 AND n.c = 3 RETURN n",
        &catalog,
    );
    let access = scan_access(&plan);
    let ScanAccess::CompositeLookup { properties, .. } = &access else {
        panic!("expected CompositeLookup, got {access:?}");
    };
    let keys: Vec<&str> = properties.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(keys, vec!["b", "c"], "cheaper composite index must win");
}

#[test]
fn composite_gate_declines_when_not_cheaper_than_label() {
    // The only composite index matches every row (cardinality == label
    // population) → not cheaper than the residual baseline → stays Linear.
    let label = db_string("Doc2");
    let a = db_string("a");
    let b = db_string("b");
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(label.clone())
        .with_node_composite_index(
            label.clone(),
            vec![(a, IndexKind::Integer), (b, IndexKind::Integer)],
        )
        .with_label_cardinality(label.clone(), 100)
        .with_composite_cardinality(label, vec![Value::Int(1), Value::Int(2)], 100);
    let plan = optimized(
        "MATCH (n:Doc2) FILTER n.a = 1 AND n.b = 2 RETURN n",
        &catalog,
    );
    assert!(
        !matches!(scan_access(&plan), ScanAccess::CompositeLookup { .. }),
        "non-selective composite must NOT use CompositeLookup, got {:?}",
        scan_access(&plan)
    );
}

// ---------------------------------------------------------------------------
// IN-list gate.
// ---------------------------------------------------------------------------

#[test]
fn in_list_selective_uses_bitmap_union() {
    let label = db_string("PersonIn");
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(label.clone())
        .with_node_typed_index(label.clone(), db_string("age"), IndexKind::Integer)
        .with_label_cardinality(label.clone(), 1000)
        .with_equality_cardinality(label.clone(), db_string("age"), Value::Int(1), 3)
        .with_equality_cardinality(label.clone(), db_string("age"), Value::Int(2), 4);
    let plan = optimized(
        "MATCH (n:PersonIn) FILTER n.age IN [1, 2] RETURN n",
        &catalog,
    );
    assert!(
        matches!(scan_access(&plan), ScanAccess::BitmapUnion { .. }),
        "selective IN-list must use bitmap union, got {:?}",
        scan_access(&plan)
    );
}

#[test]
fn in_list_non_selective_declines_bitmap_union() {
    let label = db_string("PersonIn2");
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(label.clone())
        .with_node_typed_index(label.clone(), db_string("age"), IndexKind::Integer)
        .with_label_cardinality(label.clone(), 10)
        // Each value matches ~all rows: 8+8 ≥ 10 baseline → not cheaper.
        .with_equality_cardinality(label.clone(), db_string("age"), Value::Int(1), 8)
        .with_equality_cardinality(label.clone(), db_string("age"), Value::Int(2), 8);
    let plan = optimized(
        "MATCH (n:PersonIn2) FILTER n.age IN [1, 2] RETURN n",
        &catalog,
    );
    assert!(
        !matches!(scan_access(&plan), ScanAccess::BitmapUnion { .. }),
        "non-selective IN-list must NOT use bitmap union, got {:?}",
        scan_access(&plan)
    );
}
