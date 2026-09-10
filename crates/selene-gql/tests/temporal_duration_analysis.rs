//! Analyzer coverage for temporal instant plus duration expressions.

use selene_gql::{
    AnalysisError, AnalyzedStatement, AnalyzedType, EmptyProcedureRegistry, ExpectedType,
    GqlStatus, GqlType, PipelineStatement, Side, Statement, TypeMismatchContext, analyze, parse,
};

fn analyze_one(source: &str) -> Result<AnalyzedStatement, AnalysisError> {
    let statement = parse(source).expect("test input parses");
    analyze(statement, &EmptyProcedureRegistry, None)
}

fn projection_type(analyzed: &AnalyzedStatement, name: &str) -> AnalyzedType {
    let Statement::Query(query) = analyzed.source() else {
        panic!("expected query statement");
    };
    let item = query
        .statements
        .iter()
        .filter_map(|statement| match statement {
            PipelineStatement::Return(clause) => Some(&clause.items),
            PipelineStatement::With(clause) => Some(&clause.items),
            _ => None,
        })
        .flatten()
        .find(|item| {
            item.alias
                .clone()
                .is_some_and(|alias| alias.as_str() == name)
        })
        .unwrap_or_else(|| panic!("projection {name} exists"));
    let id = analyzed
        .expr_ids
        .get(&item.expr)
        .unwrap_or_else(|| panic!("projection {name} has an ExprId"));
    analyzed.expr_types.get(id).clone()
}

#[test]
fn temporal_duration_add_sub_analyzes_as_temporal_operand_type() {
    for (source, expected) in [
        (
            "RETURN DATE '2026-01-01' + DURATION 'P1D' AS value",
            GqlType::Date,
        ),
        (
            "RETURN DURATION 'P1D' + LOCAL DATETIME '2026-01-01T00:00:00' AS value",
            GqlType::LocalDateTime,
        ),
        (
            "RETURN ZONED DATETIME '2026-01-01T00:00:00Z' - DURATION 'PT1H' AS value",
            GqlType::ZonedDateTime,
        ),
        (
            "RETURN LOCAL TIME '12:00:00' + DURATION 'PT1H' AS value",
            GqlType::LocalTime,
        ),
        (
            "RETURN TIME '12:00:00Z' + DURATION 'PT1H' AS value",
            GqlType::ZonedTime,
        ),
    ] {
        let analyzed = analyze_one(source).unwrap();
        assert_eq!(
            projection_type(&analyzed, "value"),
            AnalyzedType::Resolved(expected),
            "{source}"
        );
    }
}

#[test]
fn temporal_duration_null_operands_keep_temporal_static_type() {
    for (source, expected) in [
        ("RETURN DATE '2026-01-01' + NULL AS value", GqlType::Date),
        ("RETURN NULL + DATE '2026-01-01' AS value", GqlType::Date),
        (
            "RETURN LOCAL TIME '12:00:00' - NULL AS value",
            GqlType::LocalTime,
        ),
    ] {
        let analyzed = analyze_one(source).unwrap();
        assert_eq!(
            projection_type(&analyzed, "value"),
            AnalyzedType::Resolved(expected),
            "{source}"
        );
    }
}

#[test]
fn temporal_duration_analyzer_rejects_non_duration_side() {
    let err = analyze_one("RETURN DATE '2026-01-01' + 1 AS value")
        .expect_err("non-duration operand is rejected");
    assert_eq!(err.gqlstatus(), GqlStatus::DATATYPE_MISMATCH);
    match &err {
        AnalysisError::TypeMismatch {
            context:
                TypeMismatchContext::BinaryArithmetic {
                    side: Side::Rhs, ..
                },
            expected: ExpectedType::Specific(GqlType::Duration),
            found: GqlType::Integer,
            ..
        } => {}
        other => panic!("unexpected analysis error: {other:?}"),
    }
}

#[test]
fn temporal_duration_analyzer_rejects_duration_minus_temporal() {
    let err = analyze_one("RETURN DURATION 'P1D' - DATE '2026-01-01' AS value")
        .expect_err("duration minus temporal is rejected");
    match &err {
        AnalysisError::TypeMismatch {
            context:
                TypeMismatchContext::BinaryArithmetic {
                    side: Side::Rhs, ..
                },
            expected: ExpectedType::Specific(GqlType::Duration),
            found: GqlType::Date,
            ..
        } => {}
        other => panic!("unexpected analysis error: {other:?}"),
    }
}
