//! BRIEF-29 IN-list optimizer tests.

use selene_core::DbString;
use selene_gql::plan::optimize::rules::InListOptimization;
use selene_gql::{
    EmptyProcedureRegistry, ExecutionPlan, FilterPredicate, GqlType, IndexKey, IndexKind, JoinTree,
    NodeOrEdgeScan, PipelineOp, Rule, ScanAccess, analyze, optimize, parse, plan,
};
use selene_testing::MockIndexCatalog;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn optimized_one(source: &str, catalog: &MockIndexCatalog) -> selene_gql::ExecutionPlan {
    let plan = planned_one(source);
    let ctx = selene_gql::OptimizeContext::default().with_index_catalog(catalog);
    optimize(plan, &ctx)
}

fn planned_one(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
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

fn first_scan_mut(tree: &mut JoinTree) -> Option<&mut NodeOrEdgeScan> {
    match tree {
        JoinTree::Scan(scan) => Some(scan),
        JoinTree::Expand { child, .. } => first_scan_mut(child),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            first_scan_mut(left).or_else(|| first_scan_mut(right))
        }
        JoinTree::WorstCaseOptimal { .. } | JoinTree::Subplan(_) => None,
        _ => None,
    }
}

fn take_first_filter(plan: &mut ExecutionPlan) -> FilterPredicate {
    if let Some(index) = plan
        .pipeline
        .iter()
        .position(|op| matches!(op, PipelineOp::Filter(_)))
    {
        let PipelineOp::Filter(predicate) = plan.pipeline.remove(index) else {
            unreachable!("position matched filter op");
        };
        return predicate;
    }
    plan.pattern_plan
        .as_mut()
        .and_then(|pattern| (!pattern.filters.is_empty()).then(|| pattern.filters.remove(0)))
        .expect("plan carries a filter predicate")
}

#[test]
fn rewrites_small_literal_in_list_to_bitmap_union() {
    let catalog = MockIndexCatalog::new().with_node_typed_index(
        db_string("Person"),
        db_string("email"),
        IndexKind::String,
    );
    let plan = optimized_one(
        "MATCH (p:Person) WHERE p.email IN ['alice@example.com', 'bob@example.com'] RETURN p",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    assert!(matches!(scan.access, ScanAccess::BitmapUnion { ref keys, .. } if keys.len() == 2));
    assert!(scan.property_predicates.is_empty());
}

#[test]
fn rewrites_scan_under_path_search_selector() {
    let catalog = MockIndexCatalog::new().with_node_typed_index(
        db_string("Person"),
        db_string("email"),
        IndexKind::String,
    );
    let mut plan = planned_one(
        "MATCH ANY (p:Person) WHERE p.email IN ['alice@example.com', 'bob@example.com'] RETURN p",
    );
    let predicate = take_first_filter(&mut plan);
    let pattern = plan.pattern_plan.as_mut().expect("pattern plan exists");
    first_scan_mut(&mut pattern.join_tree)
        .expect("scan exists")
        .property_predicates
        .push(predicate);

    let ctx = selene_gql::OptimizeContext::default().with_index_catalog(&catalog);
    let plan = InListOptimization.rewrite(plan, &ctx).plan;
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    assert!(matches!(scan.access, ScanAccess::BitmapUnion { ref keys, .. } if keys.len() == 2));
    assert!(scan.property_predicates.is_empty());
}

#[test]
fn leaves_large_in_list_unchanged() {
    let catalog = MockIndexCatalog::new().with_node_typed_index(
        db_string("Person"),
        db_string("age"),
        IndexKind::Integer,
    );
    let plan = optimized_one(
        "MATCH (p:Person) WHERE p.age IN [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17] RETURN p",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    assert!(matches!(scan.access, ScanAccess::Linear));
    assert_eq!(scan.property_predicates.len(), 1);
}

#[test]
fn all_parameter_in_list_fires_bitmap_union() {
    // BRIEF-154 bar 4: `WHERE n.x IN [$a, $b, $c]` plans as BitmapUnion with
    // all-parameter keys.
    let catalog = MockIndexCatalog::new().with_node_typed_index(
        db_string("Person"),
        db_string("email"),
        IndexKind::String,
    );
    let plan = optimized_one(
        "MATCH (p:Person) WHERE p.email IN [$a, $b, $c] RETURN p",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    let ScanAccess::BitmapUnion { keys, .. } = &scan.access else {
        panic!("expected bitmap union, got {:?}", scan.access);
    };
    assert_eq!(keys.len(), 3);
    for key in keys {
        assert!(
            matches!(key, IndexKey::Parameter { .. }),
            "expected parameter key, got {key:?}"
        );
    }
    assert!(scan.property_predicates.is_empty());
}

#[test]
fn typed_list_parameter_in_list_fires_bitmap_union() {
    let catalog = MockIndexCatalog::new().with_node_typed_index(
        db_string("Person"),
        db_string("email"),
        IndexKind::String,
    );
    let plan = optimized_one(
        "MATCH (p:Person) WHERE p.email IN $emails :: LIST<STRING> RETURN p",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    let ScanAccess::BitmapUnion { keys, .. } = &scan.access else {
        panic!("expected bitmap union, got {:?}", scan.access);
    };
    assert_eq!(keys.len(), 1);
    let IndexKey::ParameterList {
        name,
        declared_type,
        ..
    } = &keys[0]
    else {
        panic!("expected parameter-list key, got {:?}", keys[0]);
    };
    assert_eq!(name.as_str(), "emails");
    assert_eq!(declared_type, &GqlType::List(Box::new(GqlType::String)));
    assert!(scan.property_predicates.is_empty());
}

#[test]
fn untyped_list_parameter_in_list_stays_linear() {
    let catalog = MockIndexCatalog::new().with_node_typed_index(
        db_string("Person"),
        db_string("email"),
        IndexKind::String,
    );
    let plan = optimized_one(
        "MATCH (p:Person) WHERE p.email IN $emails RETURN p",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(matches!(scan.access, ScanAccess::Linear));
    assert_eq!(scan.property_predicates.len(), 1);
}

#[test]
fn mixed_literal_and_parameter_in_list_falls_back_to_linear() {
    // BRIEF-154 Q3: mixed-shape InLists fall back to Linear in v1.1; the
    // homogeneous bar keeps runtime dispatch trivial.
    let catalog = MockIndexCatalog::new().with_node_typed_index(
        db_string("Person"),
        db_string("email"),
        IndexKind::String,
    );
    let plan = optimized_one(
        "MATCH (p:Person) WHERE p.email IN [$a, 'bob@example.com'] RETURN p",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    assert!(
        matches!(scan.access, ScanAccess::Linear),
        "expected linear fallback for mixed InList, got {:?}",
        scan.access
    );
    assert_eq!(scan.property_predicates.len(), 1);
}

#[test]
fn in_list_typed_param_incompatibility_falls_back_to_linear() {
    let catalog = MockIndexCatalog::new().with_node_typed_index(
        db_string("Person"),
        db_string("email"),
        IndexKind::String,
    );
    let plan = optimized_one(
        "MATCH (p:Person) WHERE p.email IN [$a :: INTEGER, $b :: INTEGER] RETURN p",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    assert!(
        matches!(scan.access, ScanAccess::Linear),
        "expected linear fallback for typed-incompatible InList params, got {:?}",
        scan.access
    );
    assert_eq!(scan.property_predicates.len(), 1);
}

#[test]
fn sentinel_in_list_snapshot() {
    let catalog = MockIndexCatalog::new().with_node_typed_index(
        db_string("Person"),
        db_string("email"),
        IndexKind::String,
    );
    let plan = optimized_one(
        "MATCH (p:Person) WHERE p.email IN ['alice@example.com', 'bob@example.com'] RETURN p",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    let summary = match scan.access {
        ScanAccess::BitmapUnion { ref keys, .. } => format!(
            "access=bitmap_union\nkeys={}\nresidual_filters={}",
            keys.len(),
            scan.property_predicates.len()
        ),
        ref other => format!("unexpected={other:?}"),
    };

    insta::assert_snapshot!(summary, @r###"
access=bitmap_union
keys=2
residual_filters=0
"###);
}
