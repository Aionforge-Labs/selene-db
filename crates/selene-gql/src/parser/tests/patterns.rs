use super::*;

#[test]
fn parse_match_return_pipeline() {
    let query = query("MATCH (n:Person {age: 42}) WHERE n.active RETURN n.name AS name");
    assert_eq!(query.statements.len(), 2);
    let PipelineStatement::Match(match_clause) = &query.statements[0] else {
        panic!("expected MATCH");
    };
    assert_eq!(match_clause.selector, None);
    assert!(match_clause.where_clause.is_some());
    let node = match &match_clause.patterns[0].elements[0] {
        crate::ast::PatternElement::Node(node) => node,
        _ => panic!("expected node pattern"),
    };
    assert_eq!(optional_name(node.binding.clone()).as_deref(), Some("n"));
    let Some(LabelExpr::Single(label)) = node.label_expr.as_ref() else {
        panic!("expected Person label");
    };
    assert_eq!(label.as_str(), "Person");
    assert_eq!(node.properties.len(), 1);

    let PipelineStatement::Return(return_clause) = &query.statements[1] else {
        panic!("expected RETURN");
    };
    assert_eq!(
        optional_name(return_clause.items[0].alias.clone()).as_deref(),
        Some("name")
    );
    assert!(matches!(
        return_clause.items[0].expr,
        ValueExpr::PropertyAccess { .. }
    ));
}

#[test]
fn parse_edge_quantifier_and_undirected_direction() {
    let query = query("MATCH (a)-[:KNOWS*1..3]-(b) RETURN b");
    let PipelineStatement::Match(match_clause) = &query.statements[0] else {
        panic!("expected MATCH");
    };
    let edge = match &match_clause.patterns[0].elements[1] {
        crate::ast::PatternElement::Edge(edge) => edge,
        _ => panic!("expected edge pattern"),
    };
    assert_eq!(edge.direction, EdgeDirection::Any);
    assert_eq!(
        edge.quantifier,
        Some(crate::ast::Quantifier::GraphPattern {
            min: 1,
            max: Some(3)
        })
    );
}

#[test]
fn parse_label_conjunction_with_colon_separator() {
    let query = query("MATCH (n:Person:Engineer) RETURN n");
    let PipelineStatement::Match(match_clause) = &query.statements[0] else {
        panic!("expected MATCH");
    };
    let node = match &match_clause.patterns[0].elements[0] {
        crate::ast::PatternElement::Node(node) => node,
        _ => panic!("expected node pattern"),
    };
    assert!(matches!(
        node.label_expr,
        Some(LabelExpr::Conjunction(ref parts)) if parts.len() == 2
    ));
}

#[test]
fn questioned_quantifier_is_preserved_distinctly() {
    let question_source = "MATCH (a)-[r?]->(b) RETURN r";
    let bounded_source = "MATCH (a)-[r{0,1}]->(b) RETURN r";
    let question_stmt = parse(question_source).expect("parse succeeds");
    let bounded_stmt = parse(bounded_source).expect("parse succeeds");
    assert_eq!(
        crate::ast::format_read_statement(&question_stmt).expect("format succeeds"),
        "MATCH (a)-[r?]->(b)\nRETURN r"
    );
    assert_eq!(
        crate::ast::format_read_statement(&bounded_stmt).expect("format succeeds"),
        "MATCH (a)-[r{0,1}]->(b)\nRETURN r"
    );

    let Statement::Query(question) = question_stmt else {
        panic!("expected query statement");
    };
    let Statement::Query(bounded) = bounded_stmt else {
        panic!("expected query statement");
    };
    let PipelineStatement::Match(question_match) = &question.statements[0] else {
        panic!("expected MATCH");
    };
    let PipelineStatement::Match(bounded_match) = &bounded.statements[0] else {
        panic!("expected MATCH");
    };
    let question_edge = match &question_match.patterns[0].elements[1] {
        crate::ast::PatternElement::Edge(edge) => edge,
        _ => panic!("expected edge pattern"),
    };
    let bounded_edge = match &bounded_match.patterns[0].elements[1] {
        crate::ast::PatternElement::Edge(edge) => edge,
        _ => panic!("expected edge pattern"),
    };
    assert_eq!(
        question_edge.quantifier,
        Some(crate::ast::Quantifier::Questioned)
    );
    assert_eq!(
        bounded_edge.quantifier,
        Some(crate::ast::Quantifier::GraphPattern {
            min: 0,
            max: Some(1)
        })
    );
}

#[test]
fn is_labeled_with_quoted_keyword_does_not_misroute() {
    // Quoted identifiers that contain IS-suffix keywords (IN, NOT,
    // LIKE, BETWEEN, NORMALIZED, ...) must be classified by grammar
    // rules, not by substring scans of the source text. Otherwise
    // `IS LABELED :"IN"` would be misrouted to the IN predicate and
    // fail with "missing list", and `IS LABELED :"NOT"` would
    // silently flip negation.
    let labeled_in = only_item("RETURN n IS LABELED :\"IN\"").expr;
    let ValueExpr::IsCheck { kind, negated, .. } = &labeled_in else {
        panic!("expected IS LABELED to parse as IsCheck");
    };
    assert!(!negated, "no NOT token, but negation flagged");
    assert!(matches!(
        kind,
        crate::ast::IsCheckKind::Labeled(LabelExpr::Single(_))
    ));

    let labeled_not = only_item("RETURN n IS LABELED :\"NOT\"").expr;
    let ValueExpr::IsCheck { negated, .. } = &labeled_not else {
        panic!("expected IS LABELED to parse as IsCheck");
    };
    assert!(!negated, "quoted NOT in label name must not flip negation");
}

#[test]
fn is_not_labeled_uses_token_negation() {
    // The NOT keyword in IS NOT LABELED really does negate the predicate.
    let item = only_item("RETURN n IS NOT LABELED :Person").expr;
    let ValueExpr::IsCheck { negated, .. } = &item else {
        panic!("expected IS NOT LABELED to parse as IsCheck");
    };
    assert!(negated, "IS NOT LABELED must produce negated=true");
}
