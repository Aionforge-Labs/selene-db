//! OPT-3 single-label-scan optimizer rule tests (`label_scan`).

use selene_core::DbString;
use selene_gql::{
    EmptyProcedureRegistry, IndexKind, JoinTree, NodeOrEdgeScan, ScanAccess, analyze, optimize,
    parse, plan,
};
use selene_testing::MockIndexCatalog;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn optimized_one(source: &str, catalog: &MockIndexCatalog) -> selene_gql::ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans");
    let ctx = selene_gql::OptimizeContext::default().with_index_catalog(catalog);
    optimize(plan, &ctx)
}

fn optimized_no_catalog(source: &str) -> selene_gql::ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans");
    optimize(plan, &selene_gql::OptimizeContext::default())
}

fn first_scan(tree: &JoinTree) -> Option<&NodeOrEdgeScan> {
    match tree {
        JoinTree::Scan(scan) => Some(scan),
        JoinTree::Expand { child, .. } => first_scan(child),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            first_scan(left).or_else(|| first_scan(right))
        }
        JoinTree::WorstCaseOptimal { .. } | JoinTree::Subplan(_) => None,
        _ => None,
    }
}

fn collect_scans<'a>(tree: &'a JoinTree, out: &mut Vec<&'a NodeOrEdgeScan>) {
    match tree {
        JoinTree::Scan(scan) => out.push(scan),
        JoinTree::Expand { child, .. } => collect_scans(child, out),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            collect_scans(left, out);
            collect_scans(right, out);
        }
        JoinTree::DisjunctiveScan { branches, .. } => out.extend(branches.iter()),
        JoinTree::WorstCaseOptimal { .. } | JoinTree::Subplan(_) => {}
        _ => {}
    }
}

fn person_label_catalog() -> MockIndexCatalog {
    MockIndexCatalog::new().with_node_label_index(db_string("Person"))
}

// ---------------------------------------------------------------------------
// Positive cases.
// ---------------------------------------------------------------------------

#[test]
fn single_label_node_scan_picks_label_index() {
    let plan = optimized_one("MATCH (n:Person) RETURN n", &person_label_catalog());
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(
        matches!(scan.access, ScanAccess::LabelIndex { .. }),
        "expected LabelIndex, got {:?}",
        scan.access
    );
}

#[test]
fn edge_scan_branch_maps_edge_target() {
    // The lowering represents `()-[e:KNOWS]->()` as an Expand whose edge lives
    // in an EdgeMatch (with its own access slot), not as a standalone
    // `JoinTree::Scan` of kind Edge — so the LabelScan rule (which rewrites
    // only JoinTree::Scan) does not fire here, mirroring the Node-only scope of
    // disjunctive_label_expansion at HEAD. The rule must not crash or rewrite
    // any node scan against an edge-only catalog. If a future planner emits a
    // standalone edge Scan, the rule's `ScanKind::Edge -> IndexTarget::Edge`
    // arm promotes it (the runtime consumer already exists).
    let catalog = MockIndexCatalog::new().with_edge_label_index(db_string("KNOWS"));
    let plan = optimized_one("MATCH ()-[e:KNOWS]->() RETURN e", &catalog);
    let mut scans = Vec::new();
    collect_scans(&plan.pattern_plan.as_ref().unwrap().join_tree, &mut scans);
    // Any standalone edge Scan that did materialize must carry LabelIndex; any
    // standalone node Scan (the anonymous endpoints) stays Linear (no node
    // label index registered).
    for scan in &scans {
        match scan.kind {
            selene_gql::ScanKind::Edge => assert!(
                matches!(scan.access, ScanAccess::LabelIndex { .. }),
                "edge scan should pick LabelIndex, got {:?}",
                scan.access
            ),
            selene_gql::ScanKind::Node => assert!(
                matches!(scan.access, ScanAccess::Linear),
                "anonymous node endpoints stay Linear, got {:?}",
                scan.access
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Negative cases.
// ---------------------------------------------------------------------------

#[test]
fn no_label_index_stays_linear() {
    // Catalog has no Person label index.
    let plan = optimized_one("MATCH (n:Person) RETURN n", &MockIndexCatalog::new());
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(matches!(scan.access, ScanAccess::Linear));
}

#[test]
fn no_catalog_stays_linear() {
    let plan = optimized_no_catalog("MATCH (n:Person) RETURN n");
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(matches!(scan.access, ScanAccess::Linear));
}

#[test]
fn unlabeled_scan_stays_linear() {
    let plan = optimized_one("MATCH (n) RETURN n", &person_label_catalog());
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(matches!(scan.access, ScanAccess::Linear));
}

#[test]
fn multi_label_conjunction_stays_linear() {
    // `(n:A&B)` is a conjunction, not a single label.
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(db_string("Person"))
        .with_node_label_index(db_string("Employee"));
    let plan = optimized_one("MATCH (n:Person&Employee) RETURN n", &catalog);
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(
        matches!(scan.access, ScanAccess::Linear),
        "conjunction must stay Linear, got {:?}",
        scan.access
    );
}

#[test]
fn pure_label_disjunction_stays_linear() {
    // BRIEF-155 boundary: `(n:A|B)` with only label indexes (no property
    // predicate) is NOT expanded by disjunctive_label_expansion, so the scan
    // keeps a Disjunction predicate and label_scan leaves it Linear.
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(db_string("Person"))
        .with_node_label_index(db_string("Account"));
    let plan = optimized_one("MATCH (n:Person|Account) RETURN n", &catalog);
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(
        matches!(scan.access, ScanAccess::Linear),
        "pure-label disjunction must stay Linear, got {:?}",
        scan.access
    );
}

// ---------------------------------------------------------------------------
// Precedence: a stronger property-index access must win.
// ---------------------------------------------------------------------------

#[test]
fn typed_index_precedence_over_label_index() {
    // Both a label index AND a typed index on (Person, age) exist; the
    // range_index_scan rule fires first and label_scan must NOT clobber it.
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(db_string("Person"))
        .with_node_typed_index(db_string("Person"), db_string("age"), IndexKind::Integer);
    let plan = optimized_one("MATCH (n:Person) WHERE n.age = 30 RETURN n", &catalog);
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(
        matches!(scan.access, ScanAccess::TypedIndexRange { .. }),
        "typed index must win over label index, got {:?}",
        scan.access
    );
}

#[test]
fn composite_index_precedence_over_label_index() {
    let catalog = MockIndexCatalog::new()
        .with_node_label_index(db_string("Person"))
        .with_node_composite_index(
            db_string("Person"),
            vec![
                (db_string("tenant"), IndexKind::String),
                (db_string("kind"), IndexKind::String),
            ],
        );
    let plan = optimized_one(
        "MATCH (n:Person) WHERE n.tenant = 't1' AND n.kind = 'person' RETURN n",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(
        matches!(scan.access, ScanAccess::CompositeLookup { .. }),
        "composite index must win over label index, got {:?}",
        scan.access
    );
}

#[test]
fn label_index_fires_when_no_property_index_exists() {
    // A residual property predicate with NO typed index leaves the scan Linear
    // after the property rules, so label_scan promotes it and the predicate
    // stays residual.
    let plan = optimized_one(
        "MATCH (n:Person) WHERE n.city = 'NYC' RETURN n",
        &person_label_catalog(),
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(
        matches!(scan.access, ScanAccess::LabelIndex { .. }),
        "expected LabelIndex with residual predicate, got {:?}",
        scan.access
    );
    assert!(
        !scan.property_predicates.is_empty(),
        "un-consumed property predicate must stay residual"
    );
}

// ---------------------------------------------------------------------------
// Disjunctive composition + idempotency.
// ---------------------------------------------------------------------------

#[test]
fn idempotent_under_double_optimize() {
    let catalog = person_label_catalog();
    let statement = parse("MATCH (n:Person) RETURN n").unwrap();
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).unwrap();
    let plan = plan(&analyzed, &EmptyProcedureRegistry).unwrap();
    let ctx = selene_gql::OptimizeContext::default().with_index_catalog(&catalog);
    let once = optimize(plan, &ctx);
    let twice = optimize(once.clone(), &ctx);
    assert_eq!(format!("{once:?}"), format!("{twice:?}"));
}
