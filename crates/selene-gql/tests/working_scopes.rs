//! Parser-only ISO scope-host tests. No database is constructed here.

use selene_gql::{GraphExpression, ParserError, Statement, WorkingScopeClause, parse};

#[test]
fn bounded_iso_hosts_parse_and_round_trip_without_catalog_access() {
    for source in [
        "USE g MATCH (n) FILTER n.x = $x RETURN n.x",
        "AT /s USE ./g RETURN 1",
        "USE /s/g { AT /s USE CURRENT_GRAPH RETURN 1 }",
        "AT /s CALL { AT /t USE /s/g RETURN 1 AS x } YIELD x RETURN x",
        "RETURN VALUE { AT /s USE g RETURN 1 }",
        "USE g RETURN 1 AS x UNION ALL USE g RETURN 2 AS x",
        "USE g RETURN 1 AS x NEXT USE g RETURN x",
    ] {
        let parsed = parse(source).unwrap();
        let formatted = selene_gql::ast::format_read_statement(&parsed).unwrap();
        let reparsed = parse(&formatted).unwrap();
        assert!(
            selene_gql::ast::structurally_eq(&parsed, &reparsed),
            "{source}\n{formatted}"
        );
    }
}

#[test]
fn reference_spelling_and_original_scope_origins_are_preserved() {
    let source = "USE g { AT /s USE ./g RETURN $x }";
    let Statement::Query(query) = parse(source).unwrap() else {
        unreachable!()
    };
    let [
        WorkingScopeClause::Use {
            expression:
                GraphExpression::Reference {
                    may_reference_binding: true,
                    ..
                },
            ..
        },
        WorkingScopeClause::Nested(origin),
        WorkingScopeClause::At { .. },
        WorkingScopeClause::Use {
            expression:
                GraphExpression::Reference {
                    reference,
                    may_reference_binding: false,
                },
            ..
        },
    ] = query.working_scopes.as_slice()
    else {
        panic!("scope origins missing");
    };
    assert_eq!(
        &source[origin.byte_offset as usize..origin.end() as usize],
        "{ AT /s USE ./g RETURN $x }"
    );
    assert_eq!(
        &source[reference.span.byte_offset as usize..reference.span.end() as usize],
        "./g"
    );
}

#[test]
fn nonstandard_wrappers_and_invalid_hosts_are_not_admitted() {
    for source in [
        "USE GRAPH g RETURN 1",
        "AT SCHEMA /s RETURN 1",
        "RETURN 1 AT /s",
        "USE g AT /s RETURN 1",
        "RETURN 1 UNION ALL USE g RETURN 2",
        "USE g RETURN 1 NEXT RETURN 2",
        "AT /s SESSION RESET",
        "USE g SESSION SET GRAPH CURRENT_GRAPH",
    ] {
        assert!(parse(source).is_err(), "{source}");
    }
}

#[test]
fn incomplete_scope_families_report_explicit_unsupported_diagnostics() {
    for source in [
        "USE g INSERT (:Item)",
        "USE $g RETURN 1",
        "USE g { RETURN 1 UNION ALL RETURN 2 }",
    ] {
        let error = parse(source).unwrap_err();
        assert!(
            matches!(error, ParserError::NotImplemented { .. }),
            "{source}: {error:?}"
        );
        assert_eq!(error.gqlstatus().as_str(), "42N01");
    }
}

#[test]
fn parse_many_rebases_scope_and_select_origins() {
    let source = "RETURN 0; USE /s/g { AT /s USE ./g RETURN 1 }; SELECT 2";
    let statements = selene_gql::parse_many(source).unwrap();
    let Statement::Query(query) = &statements[1] else {
        unreachable!()
    };
    for scope in &query.working_scopes {
        let (origin, prefix) = match scope {
            WorkingScopeClause::Use { span, .. } => (*span, "USE"),
            WorkingScopeClause::At { span, .. } => (*span, "AT"),
            WorkingScopeClause::Nested(span) => (*span, "{"),
        };
        assert!(source[origin.byte_offset as usize..origin.end() as usize].starts_with(prefix));
    }
    let Statement::Query(query) = &statements[2] else {
        unreachable!()
    };
    let origin = query.select_origin.unwrap();
    assert_eq!(
        &source[origin.byte_offset as usize..origin.end() as usize],
        "SELECT 2"
    );
}
