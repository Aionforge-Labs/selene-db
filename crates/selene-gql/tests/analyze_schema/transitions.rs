use super::*;

#[test]
fn validates_static_label_transitions_when_all_candidates_have_a_target_type() {
    let graph_type = person_company_graph_type();
    analyze_with_schema("MATCH (n:Person) SET n :Active", &graph_type)
        .expect("Person candidates can transition to declared label set");

    let error = schema_error("MATCH (n:Company) SET n :Active", &graph_type);
    assert!(matches!(error, AnalysisError::SchemaUnknownNodeType { .. }));
}

#[test]
fn rejects_label_transition_when_any_candidate_becomes_invalid() {
    let graph_type = label_transition_graph_type();
    let error = schema_error("MATCH (n:Person) SET n :Visitor", &graph_type);
    assert!(matches!(error, AnalysisError::SchemaUnknownNodeType { .. }));
}

#[test]
fn validates_edge_set_and_remove_when_edge_label_is_unique() {
    let graph_type = person_company_graph_type();
    analyze_with_schema(
        "MATCH (a:Person)-[r:WORKS_AT]->(b:Company) SET r.since = 2026",
        &graph_type,
    )
    .expect("valid edge property set");

    let type_error = schema_error(
        "MATCH (a:Person)-[r:WORKS_AT]->(b:Company) SET r.since = 'soon'",
        &graph_type,
    );
    assert!(matches!(
        type_error,
        AnalysisError::SchemaPropertyTypeMismatch { .. }
    ));

    let undeclared = schema_error(
        "MATCH (a:Person)-[r:WORKS_AT]->(b:Company) SET r.extra = 1",
        &graph_type,
    );
    assert!(matches!(
        undeclared,
        AnalysisError::SchemaUndeclaredProperty { .. }
    ));
}

#[test]
fn rejects_edge_label_remove_before_schema_validation() {
    let graph_type = person_company_graph_type();
    let error = schema_error(
        "MATCH (a:Person)-[r:WORKS_AT]->(b:Company) REMOVE r :WORKS_AT",
        &graph_type,
    );
    assert!(matches!(error, AnalysisError::NotImplemented { .. }));
    assert_eq!(
        error.gqlstatus(),
        selene_gql::GqlStatus::FEATURE_NOT_SUPPORTED
    );
}

#[test]
fn defers_edge_set_when_label_has_multiple_edge_types() {
    let graph_type = duplicate_edge_label_graph_type();
    analyze_with_schema(
        "MATCH (a:Person)-[r:REL]->(b:Company) SET r.extra = 1",
        &graph_type,
    )
    .expect("shared edge label defers property validation");
}

#[test]
fn edge_direction_matrix_checks_right_left_and_unordered_endpoints() {
    let graph_type = person_company_graph_type();
    analyze_with_schema(
        "INSERT (a:Person { name: 'A' })-[:WORKS_AT]->(b:Company { name: 'B' })",
        &graph_type,
    )
    .expect("right edge endpoints match");
    analyze_with_schema(
        "INSERT (b:Company { name: 'B' })<-[:WORKS_AT]-(a:Person { name: 'A' })",
        &graph_type,
    )
    .expect("left edge endpoints match");

    let pattern = GraphPattern {
        path_binding: None,
        elements: vec![
            node("a", "Company", "A", SourceSpan::new(0, 1)),
            edge("WORKS_AT", EdgeDirection::Undirected, SourceSpan::new(1, 1)),
            node("b", "Person", "B", SourceSpan::new(2, 1)),
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
    analyze(statement, &EmptyProcedureRegistry, Some(&graph_type))
        .expect("undirected INSERT accepts reverse endpoint type order");
}

#[test]
fn schema_validation_smoke_preserves_write_set() {
    let graph_type = person_company_graph_type();
    let analyzed = analyze_with_schema(
        "INSERT (n:Person) SET n.name = 'Alice' RETURN n",
        &graph_type,
    )
    .expect("valid mutation analyzes");
    let write_set = analyzed.write_set.expect("mutation write-set");
    assert_eq!(write_set.entries.len(), 2);
}

#[test]
fn one_of_source_endpoint_accepts_either_declared_member() {
    // BRIEF-131e: analyzer must accept INSERT with either Document OR Comment
    // as the MENTIONS source. `matches_node_type` does the membership check on
    // OneOf at the analyzer layer too.
    let graph_type = mentions_one_of_graph_type();
    analyze_with_schema(
        "INSERT (a:Document { title: 'T' })-[:MENTIONS]->(b:Topic { name: 'N' })",
        &graph_type,
    )
    .expect("Document is in OneOf source set");
    analyze_with_schema(
        "INSERT (a:Comment { body: 'B' })-[:MENTIONS]->(b:Topic { name: 'N' })",
        &graph_type,
    )
    .expect("Comment is in OneOf source set");
}

#[test]
fn one_of_source_endpoint_rejects_non_member() {
    // F11 coverage at the analyzer layer: the rendered endpoint name for an
    // OneOf endpoint is comma-joined member node-type names (per the commit-1
    // endpoint_name OneOf arm). The error must still surface as
    // SchemaEdgeEndpointMismatch.
    let graph_type = mentions_one_of_graph_type();
    let error = schema_error(
        "INSERT (a:Topic { name: 'N1' })-[:MENTIONS]->(b:Topic { name: 'N2' })",
        &graph_type,
    );
    assert!(matches!(
        error,
        AnalysisError::SchemaEdgeEndpointMismatch { ref expected_source, .. }
            if expected_source.contains("Document") && expected_source.contains("Comment")
    ));
}
