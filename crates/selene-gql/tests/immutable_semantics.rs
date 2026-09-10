//! Source/semantic separation regressions; parsing and analysis need no database.

use selene_gql::{EmptyProcedureRegistry, Statement, analyze, parse};

#[test]
fn inherited_parameter_types_do_not_rewrite_source_syntax() {
    let source = parse("RETURN $x :: INT AS \"typed\", $x AS bare").unwrap();
    let Statement::Query(expected) = &source else {
        panic!("query expected");
    };
    let source = std::sync::Arc::new(source.clone());
    let analyzed = analyze(
        std::sync::Arc::clone(&source),
        &EmptyProcedureRegistry,
        None,
    )
    .unwrap();
    assert!(std::ptr::eq(analyzed.source(), source.as_ref()));
    let Statement::Query(actual) = analyzed.source() else {
        panic!("query expected");
    };
    assert_eq!(
        actual, expected,
        "inherited types belong to semantics, not syntax"
    );
    assert!(
        analyzed
            .parameters
            .iter()
            .all(|parameter| parameter.declared_type.is_some())
    );
}

#[test]
fn semantic_namespaces_and_child_ids_are_deterministic() {
    use selene_gql::analyze::semantic::ExpressionKind;
    let source = std::sync::Arc::new(parse("LET x = 2 RETURN x + $x :: INT AS total").unwrap());
    let analyzed = analyze(source.clone(), &EmptyProcedureRegistry, None).unwrap();
    assert!(
        analyzed
            .expressions
            .iter()
            .any(|node| matches!(node.kind, ExpressionKind::Binding(_)))
    );
    assert!(
        analyzed
            .expressions
            .iter()
            .any(|node| matches!(node.kind, ExpressionKind::Parameter(_)))
    );
    for node in &analyzed.expressions {
        assert!(
            node.children
                .iter()
                .all(|child| child.get() < node.id.get())
        );
    }
    let snapshot = format!("{:#?}", analyzed.semantics());
    for _ in 0..8 {
        let again = analyze(source.clone(), &EmptyProcedureRegistry, None).unwrap();
        assert_eq!(format!("{:#?}", again.semantics()), snapshot);
    }
}

#[test]
fn source_origins_survive_select_desugaring_and_all_existing_statement_families() {
    for text in [
        "SELECT n.x AS x FROM MATCH (n:Item) WHERE n.x > $minimum",
        "RETURN 1 AS x UNION ALL RETURN 2 AS x",
        "RETURN 1 AS x NEXT RETURN x + 1 AS y",
        "INSERT (n:Item {x: $x :: INT}) RETURN n",
        "MATCH (n:Item) SET n.x = $x :: INT RETURN n",
        "CREATE NODE TYPE :Item (x :: INT DEFAULT 1)",
        "EXPLAIN MATCH (n) RETURN n",
        "START TRANSACTION",
        "COMMIT",
        "ROLLBACK",
        "SESSION SET VALUE $x = 1",
        "SESSION SET GRAPH CURRENT_GRAPH",
        "SESSION RESET",
        "SESSION CLOSE",
    ] {
        let source = std::sync::Arc::new(parse(text).unwrap());
        let before = format!("{source:?}");
        let analyzed = analyze(source.clone(), &EmptyProcedureRegistry, None).unwrap();
        assert!(std::ptr::eq(source.as_ref(), analyzed.source()), "{text}");
        assert_eq!(format!("{source:?}"), before, "{text}");
        assert_eq!(analyzed.span, source.span());
        for node in &analyzed.expressions {
            assert!(node.origin.end() as usize <= text.len(), "{text}");
        }
        if text.starts_with("SELECT") {
            let Statement::Query(query) = analyzed.source() else {
                unreachable!()
            };
            assert_eq!(query.select_origin, Some(source.span()));
        }
    }
}
