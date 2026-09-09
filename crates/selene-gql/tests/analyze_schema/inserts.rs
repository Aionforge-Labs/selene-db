use super::*;

#[test]
fn schema_absent_skips_closed_graph_checks() {
    analyze_source("INSERT (n { extra: 1 })", None).expect("open graph accepts insert");
}

#[test]
fn validates_insert_node_and_edge_against_schema() {
    let graph_type = person_company_graph_type();
    analyze_with_schema("INSERT (n:Person { name: 'Alice' })", &graph_type).expect("valid node");
    let source = concat!(
        "INSERT ",
        "(a:Person { name: 'Alice' })",
        "-[:WORKS_AT { since: 2026 }]->",
        "(b:Company { name: 'Acme' })"
    );
    analyze_with_schema(source, &graph_type).expect("valid edge");
}

#[test]
fn rejects_unknown_node_type() {
    let graph_type = person_company_graph_type();
    let error = schema_error("INSERT (n:UnknownProject { name: 'X' })", &graph_type);
    assert_eq!(error.gqlstatus().as_str(), "G2000");
    assert!(matches!(error, AnalysisError::SchemaUnknownNodeType { .. }));
}

#[test]
fn rejects_unknown_edge_type() {
    let graph_type = person_company_graph_type();
    let error = schema_error(
        "INSERT (a:Person { name: 'A' })-[:LIKES]->(b:Company { name: 'B' })",
        &graph_type,
    );
    assert!(matches!(error, AnalysisError::SchemaUnknownEdgeType { .. }));
}

#[test]
fn rejects_unknown_label_on_undirected_insert_edge() {
    let graph_type = person_company_graph_type();
    let pattern = GraphPattern {
        path_binding: None,
        elements: vec![
            node("a", "Person", "A", SourceSpan::new(0, 1)),
            edge("LIKES", EdgeDirection::Undirected, SourceSpan::new(1, 1)),
            node("b", "Company", "B", SourceSpan::new(2, 1)),
        ],
        span: SourceSpan::new(0, 3),
    };
    let statement = Statement::Mutate(MutationPipeline {
        statements: NonEmpty::try_from_vec(vec![MutationStatement::Insert(InsertStatement {
            patterns: vec![pattern],
            span: SourceSpan::new(0, 3),
        })])
        .expect("non-empty"),
        terminator: None,
        span: SourceSpan::new(0, 3),
    });
    let error = analyze(statement, &EmptyProcedureRegistry, Some(&graph_type))
        .expect_err("unknown edge label rejects before unordered endpoint validation");
    assert!(matches!(error, AnalysisError::SchemaUnknownEdgeType { .. }));
}

#[test]
fn rejects_unknown_label_when_endpoints_are_dynamic() {
    let graph_type = person_company_graph_type();
    let error = schema_error("MATCH (a), (b) INSERT (a)-[:LIKES]->(b)", &graph_type);
    assert!(matches!(error, AnalysisError::SchemaUnknownEdgeType { .. }));
}

#[test]
fn rejects_static_edge_endpoint_mismatch() {
    let graph_type = person_company_graph_type();
    let error = schema_error(
        "INSERT (a:Company { name: 'A' })-[:WORKS_AT]->(b:Person { name: 'B' })",
        &graph_type,
    );
    assert!(matches!(
        error,
        AnalysisError::SchemaEdgeEndpointMismatch { .. }
    ));
}

#[test]
fn reused_insert_node_uses_exact_endpoint_lookup() {
    let graph_type = person_company_graph_type();
    let error = schema_error(
        concat!(
            "INSERT (n:Person { name: 'A' }) ",
            "INSERT (n)-[:WORKS_AT]->(b:Person { name: 'B' })"
        ),
        &graph_type,
    );
    assert!(matches!(
        error,
        AnalysisError::SchemaEdgeEndpointMismatch { .. }
    ));
}
