use super::*;

#[test]
fn lowers_match_return_scan() {
    let plan = plan_one("MATCH (n) RETURN n");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    assert_eq!(pattern.bindings.len(), 1);
    assert_eq!(pattern.bindings[0].name.as_str(), "n");
    assert_eq!(pattern.bindings[0].element, BindingElement::Node);
    let JoinTree::Scan(scan) = &pattern.join_tree else {
        panic!("expected scan");
    };
    assert_eq!(scan.kind, ScanKind::Node);
    assert!(scan.binding.is_some());
    assert_eq!(variant_names(&plan), ["Project"]);
    assert_eq!(
        plan.output_schema.columns[0].name.clone().unwrap().as_str(),
        "n"
    );
}

#[test]
fn anonymous_node_scan_has_no_binding() {
    let plan = plan_one("MATCH (:Person) RETURN 1");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    let JoinTree::Scan(scan) = &pattern.join_tree else {
        panic!("expected scan");
    };
    assert_eq!(scan.binding, None);
    assert!(matches!(
        &scan.label_predicate,
        Some(LabelExpr::Single(label)) if label.as_str() == "Person"
    ));
}

#[test]
fn anonymous_edge_endpoints_are_preserved_as_none() {
    let plan = plan_one("MATCH ()-[e]->() RETURN e");
    let (edge, direction) = expand(&plan);
    assert_eq!(direction, EdgeDirection::Right);
    assert!(edge.binding.is_some());
    assert_eq!(edge.left_binding, None);
    assert_eq!(edge.right_binding, None);
}

#[test]
fn direction_matrix_preserves_pattern_direction() {
    let right = plan_one("MATCH (a)-[:K]->(b) RETURN a, b");
    assert_eq!(expand(&right).1, EdgeDirection::Right);

    let left = plan_one("MATCH (a)<-[:K]-(b) RETURN a, b");
    assert_eq!(expand(&left).1, EdgeDirection::Left);

    let undirected = plan_one("MATCH (a)-[:K]-(b) RETURN a, b");
    assert_eq!(expand(&undirected).1, EdgeDirection::Any);
}

#[test]
fn bounded_quantified_edge_lowers_to_repeat_with_group_list_binding() {
    let plan = plan_one("MATCH (a)-[r:K*1..3]->(b) RETURN r");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    let (edge, min, max) = repeat(&plan);
    assert!(edge.group_binding.is_some());
    assert_eq!(min, 1);
    assert_eq!(max, Some(3));

    let group = pattern
        .bindings
        .iter()
        .find(|binding| binding.name.as_str() == "r")
        .expect("group binding exposed");
    assert_eq!(group.element, BindingElement::Edge);
    assert_eq!(
        group.ty,
        AnalyzedType::Resolved(GqlType::List(Box::new(GqlType::EdgeRef)))
    );
    assert_eq!(
        plan.output_schema.columns[0].name.clone().unwrap().as_str(),
        "r"
    );
    assert_eq!(
        plan.output_schema.columns[0].ty,
        AnalyzedType::Resolved(GqlType::List(Box::new(GqlType::EdgeRef)))
    );
}

#[test]
fn optional_match_lowers_to_outer_join() {
    let plan = plan_one("MATCH (a) OPTIONAL MATCH (a)-[:K]->(b) RETURN a, b");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    assert!(matches!(pattern.join_tree, JoinTree::Outer { .. }));
}

#[test]
fn path_binding_creates_path_plan_placeholder() {
    let plan = plan_one("MATCH p = (a)-[:K]->(b) RETURN p");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    assert_eq!(pattern.paths.len(), 1);
}

#[test]
fn property_maps_and_inline_where_are_preserved() {
    let plan = plan_one("MATCH (n:Person {age: 42} WHERE n.active) RETURN n");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    let JoinTree::Scan(scan) = &pattern.join_tree else {
        panic!("expected scan");
    };
    assert_eq!(scan.property_predicates.len(), 1);
    assert!(matches!(
        &scan.property_predicates[0].kind,
        FilterPredicateKind::PropertyEquals { key, .. } if key.as_str() == "age"
    ));
    assert_eq!(pattern.filters.len(), 1);
}
