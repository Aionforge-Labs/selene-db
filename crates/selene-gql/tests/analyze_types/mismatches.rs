use super::*;

#[test]
fn add_string_and_integer_errors() {
    let err = analyze_one("RETURN 'a' + 1 AS x").unwrap_err();
    assert!(matches!(
        err,
        AnalysisError::TypeMismatch {
            context: TypeMismatchContext::BinaryArithmetic { .. },
            ..
        }
    ));
    assert_eq!(err.gqlstatus(), GqlStatus::DATATYPE_MISMATCH);
}

#[test]
fn duration_add_integer_errors_on_integer_operand() {
    let err = analyze_one("RETURN DURATION 'PT1H' + 1 AS x").unwrap_err();
    assert!(matches!(
        err,
        AnalysisError::TypeMismatch {
            context: TypeMismatchContext::BinaryArithmetic {
                op: BinaryOp::Add,
                side: Side::Rhs,
            },
            expected: ExpectedType::Specific(GqlType::Duration),
            found: GqlType::Integer,
            ..
        }
    ));
    assert_eq!(err.gqlstatus(), GqlStatus::DATATYPE_MISMATCH);
}

#[test]
fn duration_scaling_rejects_duration_coefficient() {
    let err = analyze_one("RETURN DURATION 'PT1H' * DURATION 'PT2H' AS x").unwrap_err();
    assert!(matches!(
        err,
        AnalysisError::TypeMismatch {
            context: TypeMismatchContext::BinaryArithmetic {
                op: BinaryOp::Mul,
                side: Side::Rhs,
            },
            expected: ExpectedType::Numeric,
            found: GqlType::Duration,
            ..
        }
    ));
    assert_eq!(err.gqlstatus(), GqlStatus::DATATYPE_MISMATCH);
}

#[test]
fn not_on_integer_errors() {
    let err = analyze_one("RETURN NOT 1 AS x").unwrap_err();
    assert!(matches!(
        err,
        AnalysisError::TypeMismatch {
            context: TypeMismatchContext::UnaryNot,
            ..
        }
    ));
}

#[test]
fn compare_boolean_to_integer_errors() {
    let err = analyze_one("RETURN true < 1 AS x").unwrap_err();
    assert!(matches!(
        err,
        AnalysisError::TypeMismatch {
            context: TypeMismatchContext::BinaryComparison { .. },
            ..
        }
    ));
}

#[test]
fn boolean_operator_rejects_static_non_boolean_operand() {
    let (context, _) = type_mismatch("RETURN true AND 1 AS x");
    assert!(matches!(
        context,
        TypeMismatchContext::BinaryBoolean {
            op: _,
            side: Side::Rhs
        }
    ));
}

#[test]
fn concat_rejects_static_non_list_string_bytes_or_path_operand() {
    let (context, _) = type_mismatch("RETURN 'a' || 1 AS x");
    assert!(matches!(
        context,
        TypeMismatchContext::BinaryConcat { side: Side::Rhs }
    ));
}

#[test]
fn string_predicate_rejects_static_non_string_operand() {
    let (context, _) = type_mismatch("RETURN 1 CONTAINS 'a' AS x");
    assert!(matches!(
        context,
        TypeMismatchContext::BinaryStringPredicate {
            op: _,
            side: Side::Lhs
        }
    ));
}

#[test]
fn is_normalized_uses_dedicated_mismatch_context() {
    let (context, _) = type_mismatch("RETURN 1 IS NORMALIZED AS x");
    assert!(matches!(context, TypeMismatchContext::IsNormalized));
}

#[test]
fn truth_value_predicate_rejects_static_non_boolean_operand() {
    for source in [
        "RETURN 1 IS TRUE AS ok",
        "RETURN 'x' IS FALSE AS ok",
        "RETURN [true] IS UNKNOWN AS ok",
    ] {
        let (context, _) = type_mismatch(source);
        assert!(matches!(context, TypeMismatchContext::IsTruthValue));
    }
}

#[test]
fn unary_negate_rejects_static_non_numeric_operand() {
    let (context, _) = type_mismatch("RETURN -true AS x");
    assert!(matches!(context, TypeMismatchContext::UnaryNegate));
}

#[test]
fn in_list_updates_unified_type_across_resolved_items() {
    let (context, _) = type_mismatch("RETURN 'a' IN [NULL, 'b', 1] AS x");
    assert!(matches!(context, TypeMismatchContext::InListUnification));
}

#[test]
fn case_branches_with_disjoint_types_error() {
    let err = analyze_one("MATCH (n) RETURN CASE WHEN n.age > 18 THEN 1 ELSE 'minor' END AS label")
        .unwrap_err();
    assert!(matches!(
        err,
        AnalysisError::TypeMismatch {
            context: TypeMismatchContext::CaseBranchUnification,
            ..
        }
    ));
}

#[test]
fn case_dynamic_branch_still_validates_resolved_branches() {
    let (context, span) = type_mismatch(
        "MATCH (n) RETURN CASE WHEN true THEN n.age WHEN false THEN 'a' ELSE 1 END AS label",
    );
    assert!(matches!(
        context,
        TypeMismatchContext::CaseBranchUnification
    ));
    assert!(span.byte_len > 0);
}

#[test]
fn list_literal_with_disjoint_types_errors() {
    let err = analyze_one("RETURN [1, 'a'] AS xs").unwrap_err();
    assert!(matches!(
        err,
        AnalysisError::TypeMismatch {
            context: TypeMismatchContext::ListLiteralUnification,
            ..
        }
    ));
}

#[test]
fn list_dynamic_item_still_validates_resolved_items() {
    let (context, span) = type_mismatch("MATCH (n) RETURN [n.age, 'a', 1] AS xs");
    assert!(matches!(
        context,
        TypeMismatchContext::ListLiteralUnification
    ));
    assert!(span.byte_len > 0);
}

#[test]
fn boolean_conditions_reject_static_non_boolean() {
    for (source, expected_clause) in [
        ("MATCH (n) WHERE 1 RETURN n", ConditionClause::MatchWhere),
        ("MATCH (n WHERE 1) RETURN n", ConditionClause::InlineWhere),
        ("MATCH (n) FILTER 1 RETURN n", ConditionClause::Filter),
        (
            "MATCH (n) RETURN n GROUP BY n HAVING 1",
            ConditionClause::Having,
        ),
        (
            "MATCH (n) WITH n AS kept WHERE 1 RETURN kept",
            ConditionClause::WithWhere,
        ),
        (
            "RETURN CASE WHEN 1 THEN 'yes' ELSE 'no' END AS answer",
            ConditionClause::CaseWhen,
        ),
    ] {
        let err = analyze_one(source).unwrap_err();
        assert!(
            matches!(
                err,
                AnalysisError::TypeMismatch {
                    context: TypeMismatchContext::Condition { clause },
                    ..
                } if clause == expected_clause
            ),
            "{source} produced {err:?}"
        );
    }
}

#[test]
fn is_typed_unsupported_variant_errors_for_hand_built_ast() {
    let span = SourceSpan::new(0, 1);
    let expr = ValueExpr::IsCheck {
        operand: Box::new(ValueExpr::Literal(Literal::Null(span))),
        kind: IsCheckKind::Typed(GqlType::GraphRef),
        negated: false,
        span,
    };
    let statement = Statement::Query(QueryPipeline {
        working_scopes: Vec::new(),
        select_origin: None,
        statements: vec![PipelineStatement::Return(ReturnClause {
            distinct: false,
            star: false,
            items: vec![ReturnItem {
                expr,
                alias: Some(db_string("ok").unwrap()),
                span,
            }],
            group_by: None,
            having: None,
            span,
        })],
        span,
    });

    let err = analyze(statement, &EmptyProcedureRegistry, None).unwrap_err();
    assert!(matches!(
        err,
        AnalysisError::TypeMismatch {
            context: TypeMismatchContext::IsTypedTarget,
            ..
        }
    ));
}
