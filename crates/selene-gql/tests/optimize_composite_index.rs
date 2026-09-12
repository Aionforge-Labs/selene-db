//! BRIEF-29 composite-index optimizer tests.

use selene_core::DbString;
use selene_gql::{
    BinaryOp, EmptyProcedureRegistry, FilterPredicate, FilterPredicateKind, IndexKey, JoinTree,
    Literal, NodeOrEdgeScan, ScanAccess, ValueExpr, analyze, optimize, parse, plan,
};
use selene_testing::MockIndexCatalog;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn integer_property_names(count: usize) -> Vec<(DbString, selene_gql::IndexKind)> {
    (0..count)
        .map(|index| {
            (
                db_string(&format!("p{index}")),
                selene_gql::IndexKind::Integer,
            )
        })
        .collect()
}

fn equality_predicate_source(count: usize) -> String {
    (0..count)
        .map(|index| format!("n.p{index} = {index}"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn match_with_equality_count(count: usize) -> String {
    format!(
        "MATCH (n:Doc) WHERE {} RETURN n",
        equality_predicate_source(count)
    )
}

fn optimized_one(source: &str, catalog: &MockIndexCatalog) -> selene_gql::ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans");
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
        JoinTree::WorstCaseOptimal { .. } | JoinTree::Subplan(_) => None,
        _ => None,
    }
}

fn integer_literal(expr: &ValueExpr) -> Option<i64> {
    match expr {
        ValueExpr::Literal(Literal::Integer(value, _)) => Some(*value),
        _ => None,
    }
}

fn property_access_key(expr: &ValueExpr) -> Option<DbString> {
    let ValueExpr::PropertyAccess { key, .. } = expr else {
        return None;
    };
    Some(key.clone())
}

fn expression_property_integer(predicate: &FilterPredicate, expected_key: DbString) -> Option<i64> {
    let ValueExpr::BinaryOp {
        op: BinaryOp::Eq,
        lhs,
        rhs,
        ..
    } = &predicate.expr
    else {
        return None;
    };
    if property_access_key(lhs.as_ref()) == Some(expected_key.clone()) {
        return integer_literal(rhs.as_ref());
    }
    if property_access_key(rhs.as_ref()) == Some(expected_key) {
        return integer_literal(lhs.as_ref());
    }
    None
}

fn property_integer(predicate: &FilterPredicate, expected_key: DbString) -> Option<i64> {
    match &predicate.kind {
        FilterPredicateKind::PropertyEquals { key, .. } if *key == expected_key => {
            integer_literal(&predicate.expr)
        }
        FilterPredicateKind::Expression => expression_property_integer(predicate, expected_key),
        _ => None,
    }
}

fn residual_integers(scan: &NodeOrEdgeScan, key: DbString) -> Vec<i64> {
    scan.property_predicates
        .iter()
        .filter_map(|predicate| property_integer(predicate, key.clone()))
        .collect()
}

fn composite_key_integer(keys: &[(DbString, IndexKey)], key: DbString) -> Option<i64> {
    keys.iter().find_map(|(candidate_key, index_key)| {
        let IndexKey::Literal(literal) = index_key else {
            return None;
        };
        match literal {
            Literal::Integer(value, _) if *candidate_key == key => Some(*value),
            _ => None,
        }
    })
}

#[test]
fn composite_lookup_uses_declaration_order() {
    let catalog = MockIndexCatalog::new().with_node_composite_index(
        db_string("Doc"),
        vec![
            (db_string("tenant"), selene_gql::IndexKind::String),
            (db_string("kind"), selene_gql::IndexKind::String),
        ],
    );
    let plan = optimized_one(
        "MATCH (n:Doc) WHERE n.kind = 'k' AND n.tenant = 't1' RETURN n",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    let ScanAccess::CompositeLookup {
        ref properties,
        ref keys,
        ..
    } = scan.access
    else {
        panic!("expected composite lookup, got {:?}", scan.access);
    };
    assert_eq!(
        properties,
        &vec![
            (db_string("tenant"), selene_gql::IndexKind::String),
            (db_string("kind"), selene_gql::IndexKind::String),
        ]
    );
    assert_eq!(keys[0].0, db_string("tenant"));
    assert_eq!(keys[1].0, db_string("kind"));
    assert!(scan.property_predicates.is_empty());
}

#[test]
fn composite_index_lookup_dedupes_duplicate_property_keys() {
    let catalog = MockIndexCatalog::new().with_node_composite_index(
        db_string("Doc"),
        vec![
            (db_string("tenant"), selene_gql::IndexKind::Integer),
            (db_string("year"), selene_gql::IndexKind::Integer),
        ],
    );
    let plan = optimized_one(
        "MATCH (n:Doc) WHERE n.tenant = 1 AND n.tenant = 1 AND n.year = 2024 RETURN n",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    let ScanAccess::CompositeLookup {
        ref properties,
        ref keys,
        ..
    } = scan.access
    else {
        panic!("expected composite lookup, got {:?}", scan.access);
    };
    assert_eq!(
        properties,
        &vec![
            (db_string("tenant"), selene_gql::IndexKind::Integer),
            (db_string("year"), selene_gql::IndexKind::Integer),
        ]
    );
    assert_eq!(composite_key_integer(keys, db_string("tenant")), Some(1));
    assert_eq!(composite_key_integer(keys, db_string("year")), Some(2024));

    let tenant_residuals = residual_integers(scan, db_string("tenant"));
    assert!(
        tenant_residuals.is_empty() || tenant_residuals == vec![1],
        "exact duplicate residual should be absent or a redundant tenant=1 predicate, got {tenant_residuals:?}"
    );
}

#[test]
fn composite_index_lookup_rewrites_scan_under_path_search_selector() {
    let catalog = MockIndexCatalog::new().with_node_composite_index(
        db_string("Doc"),
        vec![
            (db_string("tenant"), selene_gql::IndexKind::String),
            (db_string("kind"), selene_gql::IndexKind::String),
        ],
    );
    let plan = optimized_one(
        "MATCH ANY (n:Doc {tenant: 't1', kind: 'k'}) RETURN n",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    assert!(matches!(scan.access, ScanAccess::CompositeLookup { .. }));
    assert!(scan.property_predicates.is_empty());
}

#[test]
fn composite_index_lookup_keeps_conflicting_duplicates_in_residual() {
    let catalog = MockIndexCatalog::new().with_node_composite_index(
        db_string("Doc"),
        vec![
            (db_string("tenant"), selene_gql::IndexKind::Integer),
            (db_string("year"), selene_gql::IndexKind::Integer),
        ],
    );
    let plan = optimized_one(
        "MATCH (n:Doc) WHERE n.tenant = 1 AND n.tenant = 2 AND n.year = 2024 RETURN n",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    let ScanAccess::CompositeLookup {
        ref properties,
        ref keys,
        ..
    } = scan.access
    else {
        panic!("expected composite lookup, got {:?}", scan.access);
    };
    assert_eq!(
        properties,
        &vec![
            (db_string("tenant"), selene_gql::IndexKind::Integer),
            (db_string("year"), selene_gql::IndexKind::Integer),
        ]
    );
    assert_eq!(composite_key_integer(keys, db_string("tenant")), Some(1));
    assert_eq!(composite_key_integer(keys, db_string("year")), Some(2024));
    assert_eq!(residual_integers(scan, db_string("tenant")), vec![2]);
}

#[test]
fn composite_index_lookup_does_not_panic_on_oversized_candidates() {
    let catalog = MockIndexCatalog::new();
    let source = match_with_equality_count(64);

    let result = std::panic::catch_unwind(|| optimized_one(&source, &catalog));

    assert!(result.is_ok());
    let plan = result.expect("optimizer should not panic");
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    assert!(matches!(scan.access, ScanAccess::Linear));
}

#[test]
fn composite_index_lookup_rewrites_at_cap_boundary() {
    let catalog = MockIndexCatalog::new()
        .with_node_composite_index(db_string("Doc"), integer_property_names(16));
    let plan = optimized_one(&match_with_equality_count(16), &catalog);
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    assert!(matches!(scan.access, ScanAccess::CompositeLookup { .. }));
    assert!(scan.property_predicates.is_empty());
}

#[test]
fn composite_index_lookup_bails_above_cap() {
    let catalog = MockIndexCatalog::new()
        .with_node_composite_index(db_string("Doc"), integer_property_names(17));
    let plan = optimized_one(&match_with_equality_count(17), &catalog);
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    assert!(matches!(scan.access, ScanAccess::Linear));
    assert_eq!(scan.property_predicates.len(), 17);
}

#[test]
fn composite_lookup_admits_parameter_slots_per_component() {
    // BRIEF-154 bar 3: parameterized composite equality fires.
    let catalog = MockIndexCatalog::new().with_node_composite_index(
        db_string("Doc"),
        vec![
            (db_string("tenant"), selene_gql::IndexKind::String),
            (db_string("kind"), selene_gql::IndexKind::String),
        ],
    );
    let plan = optimized_one(
        "MATCH (n:Doc) WHERE n.tenant = $a AND n.kind = $b RETURN n",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    let ScanAccess::CompositeLookup { keys, .. } = &scan.access else {
        panic!("expected composite lookup, got {:?}", scan.access);
    };
    assert_eq!(keys.len(), 2);
    for (_, index_key) in keys {
        assert!(
            matches!(index_key, IndexKey::Parameter { .. }),
            "expected parameter key in composite probe, got {index_key:?}"
        );
    }
}

#[test]
fn composite_lookup_admits_mixed_literal_and_parameter() {
    let catalog = MockIndexCatalog::new().with_node_composite_index(
        db_string("Doc"),
        vec![
            (db_string("tenant"), selene_gql::IndexKind::String),
            (db_string("kind"), selene_gql::IndexKind::String),
        ],
    );
    let plan = optimized_one(
        "MATCH (n:Doc) WHERE n.tenant = 't1' AND n.kind = $kind RETURN n",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    let ScanAccess::CompositeLookup { keys, .. } = &scan.access else {
        panic!("expected composite lookup, got {:?}", scan.access);
    };
    let tenant_key = keys
        .iter()
        .find(|(property, _)| *property == db_string("tenant"))
        .map(|(_, key)| key);
    let kind_key = keys
        .iter()
        .find(|(property, _)| *property == db_string("kind"))
        .map(|(_, key)| key);
    assert!(matches!(tenant_key, Some(IndexKey::Literal(_))));
    assert!(matches!(kind_key, Some(IndexKey::Parameter { .. })));
}

#[test]
fn composite_lookup_typed_param_incompatibility_falls_back_to_linear() {
    // BRIEF-154 §B.2 F7: typed parameter declared with an incompatible type
    // for its component column aborts the rewrite. The rule produces no
    // CompositeLookup; the residual predicates remain for linear evaluation.
    let catalog = MockIndexCatalog::new().with_node_composite_index(
        db_string("Doc"),
        vec![
            (db_string("tenant"), selene_gql::IndexKind::String),
            (db_string("kind"), selene_gql::IndexKind::String),
        ],
    );
    let plan = optimized_one(
        "MATCH (n:Doc) WHERE n.tenant = $tenant :: INTEGER AND n.kind = 'k' RETURN n",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();

    assert!(
        matches!(scan.access, ScanAccess::Linear),
        "expected linear fallback for typed-incompatible composite param, got {:?}",
        scan.access
    );
    assert_eq!(scan.property_predicates.len(), 2);
}

#[test]
fn sentinel_composite_index_snapshot() {
    let catalog = MockIndexCatalog::new().with_node_composite_index(
        db_string("Doc"),
        vec![
            (db_string("tenant"), selene_gql::IndexKind::String),
            (db_string("kind"), selene_gql::IndexKind::String),
        ],
    );
    let plan = optimized_one(
        "MATCH (n:Doc) WHERE n.kind = 'k' AND n.tenant = 't1' RETURN n",
        &catalog,
    );
    let scan = first_scan(&plan.pattern_plan.as_ref().unwrap().join_tree).unwrap();
    let summary = match scan.access {
        ScanAccess::CompositeLookup {
            ref properties,
            ref keys,
            ..
        } => format!(
            "access=composite\nproperties={}\nkeys={}\nresidual_filters={}",
            properties
                .iter()
                .map(|(property, _kind)| property.as_str())
                .collect::<Vec<_>>()
                .join(","),
            keys.len(),
            scan.property_predicates.len()
        ),
        ref other => format!("unexpected={other:?}"),
    };

    insta::assert_snapshot!(summary, @r###"
access=composite
properties=tenant,kind
keys=2
residual_filters=0
"###);
}
