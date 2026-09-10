use super::*;

#[test]
fn integer_arithmetic_promotes_to_integer() {
    let analyzed = analyze_one("RETURN 1 + 2 AS \"sum\"").unwrap();
    assert_eq!(
        projection_type(&analyzed, "sum"),
        AnalyzedType::Resolved(GqlType::Integer)
    );
}

#[test]
fn float_plus_integer_promotes_to_float64() {
    let analyzed = analyze_one("RETURN 1 + 2.0D AS \"sum\"").unwrap();
    assert_eq!(
        projection_type(&analyzed, "sum"),
        AnalyzedType::Resolved(GqlType::Float64)
    );
}

#[test]
fn decimal_plus_integer_promotes_to_decimal() {
    let analyzed = analyze_one("RETURN 1 + 2.0 AS \"sum\"").unwrap();
    assert_eq!(
        projection_type(&analyzed, "sum"),
        AnalyzedType::Resolved(GqlType::Decimal)
    );
}

#[test]
fn duration_addition_and_subtraction_analyze_as_duration() {
    let analyzed = analyze_one("RETURN DURATION 'PT1H' + DURATION 'PT2H' AS span").unwrap();
    assert_eq!(
        projection_type(&analyzed, "span"),
        AnalyzedType::Resolved(GqlType::Duration)
    );

    let analyzed = analyze_one("RETURN DURATION 'P4M' - DURATION 'P1M' AS span").unwrap();
    assert_eq!(
        projection_type(&analyzed, "span"),
        AnalyzedType::Resolved(GqlType::Duration)
    );
}

#[test]
fn duration_scaling_analyzes_as_duration() {
    for source in [
        "RETURN DURATION 'PT1H' * 2 AS span",
        "RETURN 2 * DURATION 'PT1H' AS span",
        "RETURN DURATION 'PT1H' / 2 AS span",
        "RETURN DURATION 'P1Y' * 0.5 AS span",
    ] {
        let analyzed = analyze_one(source).unwrap();
        assert_eq!(
            projection_type(&analyzed, "span"),
            AnalyzedType::Resolved(GqlType::Duration),
            "{source}"
        );
    }
}

#[test]
fn duration_unary_negation_analyzes_as_duration() {
    let analyzed = analyze_one("RETURN -DURATION 'PT1H' AS span").unwrap();
    assert_eq!(
        projection_type(&analyzed, "span"),
        AnalyzedType::Resolved(GqlType::Duration)
    );
}

#[test]
fn pattern_node_is_node_ref() {
    let analyzed = analyze_one("MATCH (n) RETURN n").unwrap();
    assert_eq!(
        projection_type(&analyzed, "n"),
        AnalyzedType::Resolved(GqlType::NodeRef)
    );
}

#[test]
fn quantified_edge_binding_is_edge_ref_list() {
    let analyzed = analyze_one("MATCH (a)-[r:K*1..2]->(b) RETURN r").unwrap();
    assert_eq!(
        projection_type(&analyzed, "r"),
        AnalyzedType::Resolved(GqlType::List(Box::new(GqlType::EdgeRef)))
    );
}

#[test]
fn group_variable_property_access_is_dynamic() {
    let analyzed = analyze_one("MATCH (a)-[r:K*1..2]->(b) RETURN r.weight AS weights").unwrap();
    assert_eq!(projection_type(&analyzed, "weights"), AnalyzedType::Dynamic);
}

#[test]
fn static_case_branches_unify_to_string() {
    let analyzed =
        analyze_one("RETURN CASE WHEN true THEN 'adult' ELSE 'minor' END AS label").unwrap();
    assert_eq!(
        projection_type(&analyzed, "label"),
        AnalyzedType::Resolved(GqlType::String)
    );
}

#[test]
fn dynamic_case_branch_defers_result_type() {
    let analyzed =
        analyze_one("MATCH (n) RETURN CASE WHEN n.age > 18 THEN n.name ELSE 'minor' END AS label")
            .unwrap();
    assert_eq!(projection_type(&analyzed, "label"), AnalyzedType::Dynamic);
}

#[test]
fn parameter_stays_dynamic() {
    let analyzed = analyze_one("RETURN $name AS who").unwrap();
    assert_eq!(projection_type(&analyzed, "who"), AnalyzedType::Dynamic);
}

#[test]
fn function_call_expression_stays_dynamic_until_scalar_dispatch() {
    let analyzed = analyze_one("RETURN size([1,2,3]) AS n").unwrap();
    assert_eq!(projection_type(&analyzed, "n"), AnalyzedType::Dynamic);
}

#[test]
fn element_id_expression_is_string() {
    let analyzed = analyze_one("MATCH (n) RETURN element_id(n) AS id").unwrap();
    assert_eq!(
        projection_type(&analyzed, "id"),
        AnalyzedType::Resolved(GqlType::String)
    );
}

#[test]
fn for_list_aliases_to_element_type() {
    let analyzed = analyze_one("FOR x IN [1, 2, 3] RETURN x").unwrap();
    assert_eq!(
        projection_type(&analyzed, "x"),
        AnalyzedType::Resolved(GqlType::Integer)
    );
}

#[test]
fn for_position_aliases_are_integer() {
    for source in [
        "FOR x IN [1, 2, 3] WITH ORDINALITY ord RETURN ord",
        "FOR x IN [1, 2, 3] WITH OFFSET off RETURN off",
    ] {
        let analyzed = analyze_one(source).unwrap();
        let name = if source.contains("ORDINALITY") {
            "ord"
        } else {
            "off"
        };
        assert_eq!(
            projection_type(&analyzed, name),
            AnalyzedType::Resolved(GqlType::Integer)
        );
    }
}

#[test]
fn record_literal_retains_its_named_field_descriptor() {
    let analyzed = analyze_one("RETURN {score: 1} AS r").unwrap();
    assert_eq!(
        projection_type(&analyzed, "r"),
        AnalyzedType::Resolved(GqlType::Record(selene_gql::RecordType::Closed(vec![(
            selene_core::db_string("score").unwrap(),
            GqlType::Integer
        )])))
    );
}

#[test]
fn list_concat_returns_list_type() {
    let analyzed = analyze_one("RETURN [1] || [2] AS xs").unwrap();
    assert_eq!(
        projection_type(&analyzed, "xs"),
        AnalyzedType::Resolved(GqlType::List(Box::new(GqlType::Integer)))
    );
}

#[test]
fn byte_string_concat_returns_bytes_type() {
    let analyzed = analyze_one("RETURN X'CA' || X'FE' AS payload").unwrap();
    assert_eq!(
        projection_type(&analyzed, "payload"),
        AnalyzedType::Resolved(GqlType::Bytes)
    );
}

#[test]
fn path_concat_returns_path_type() {
    let analyzed = analyze_one("MATCH (a) RETURN PATH[a] || PATH[a] AS p").unwrap();
    assert_eq!(
        projection_type(&analyzed, "p"),
        AnalyzedType::Resolved(GqlType::Path)
    );
}

#[test]
fn concat_accepts_static_null_operand() {
    let analyzed = analyze_one("RETURN NULL || 'tail' AS value").unwrap();
    assert_eq!(
        projection_type(&analyzed, "value"),
        AnalyzedType::Resolved(GqlType::String)
    );
}

#[test]
fn compare_boolean_literals_analyzes_as_boolean() {
    let analyzed = analyze_one("RETURN false < true AS x").expect("BOOLEAN comparison analyzes");
    assert_eq!(
        projection_type(&analyzed, "x"),
        AnalyzedType::Resolved(GqlType::Boolean)
    );
}

#[test]
fn compare_uuid_literals_analyzes_as_boolean() {
    let analyzed = analyze_one(
        "RETURN UUID '00000000-0000-0000-0000-000000000001' \
         < UUID '00000000-0000-0000-0000-000000000002' AS x",
    )
    .expect("UUID comparison analyzes");
    assert_eq!(
        projection_type(&analyzed, "x"),
        AnalyzedType::Resolved(GqlType::Boolean)
    );
}

#[test]
fn compare_graph_references_analyzes_by_static_base_type() {
    let node = analyze_one("MATCH (a), (b) RETURN a < b AS x").expect("NODE comparison analyzes");
    assert_eq!(
        projection_type(&node, "x"),
        AnalyzedType::Resolved(GqlType::Boolean)
    );

    let edge = analyze_one("MATCH ()-[a]->(), ()-[b]->() RETURN a < b AS x")
        .expect("EDGE comparison analyzes");
    assert_eq!(
        projection_type(&edge, "x"),
        AnalyzedType::Resolved(GqlType::Boolean)
    );

    let err = analyze_one("MATCH (a)-[b]->() RETURN a < b AS x").unwrap_err();
    assert!(matches!(
        err,
        AnalysisError::TypeMismatch {
            context: TypeMismatchContext::BinaryComparison { .. },
            ..
        }
    ));
}

#[test]
fn truth_value_predicate_accepts_boolean_null_and_dynamic_operands() {
    let analyzed = analyze_one(
        "RETURN true IS TRUE AS bool_value, NULL IS UNKNOWN AS null_value, $p IS FALSE AS param_value",
    )
    .expect("truth-value predicates analyze");
    for alias in ["bool_value", "null_value", "param_value"] {
        assert_eq!(
            projection_type(&analyzed, alias),
            AnalyzedType::Resolved(GqlType::Boolean)
        );
    }
}
