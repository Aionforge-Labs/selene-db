//! Analyzer type-inference tests.

use selene_core::db_string;
use selene_gql::{
    AnalysisError, AnalyzedStatement, AnalyzedType, BinaryOp, ConditionClause,
    EmptyProcedureRegistry, ExpectedType, GqlStatus, GqlType, IsCheckKind, Literal,
    PipelineStatement, QueryPipeline, ReturnClause, ReturnItem, Side, SourceSpan, Statement,
    TypeMismatchContext, ValueExpr, analyze, parse,
};

fn analyze_one(source: &str) -> Result<AnalyzedStatement, AnalysisError> {
    let statement = parse(source).expect("test input parses");
    analyze(statement, &EmptyProcedureRegistry, None)
}

fn type_mismatch(source: &str) -> (TypeMismatchContext, SourceSpan) {
    let err = analyze_one(source).unwrap_err();
    match err {
        AnalysisError::TypeMismatch { context, span, .. } => (context, span),
        other => panic!("expected TypeMismatch for {source}, got {other:?}"),
    }
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
            item.alias.clone().is_some_and(|alias| alias.as_str() == name)
                || matches!(&item.expr, ValueExpr::Variable { name: value, .. } if value.as_str() == name)
        })
        .unwrap_or_else(|| panic!("projection {name} exists"));
    let id = analyzed
        .expr_ids
        .get(&item.expr)
        .unwrap_or_else(|| panic!("projection {name} has an ExprId"));
    analyzed.expr_types.get(id).clone()
}

fn return_items(analyzed: &AnalyzedStatement) -> &[ReturnItem] {
    let Statement::Query(query) = analyzed.source() else {
        panic!("expected query statement");
    };
    query
        .statements
        .iter()
        .find_map(|statement| match statement {
            PipelineStatement::Return(clause) => Some(clause.items.as_slice()),
            _ => None,
        })
        .expect("RETURN clause exists")
}

#[path = "analyze_types/expr_ids.rs"]
mod expr_ids;
#[path = "analyze_types/inference.rs"]
mod inference;
#[path = "analyze_types/mismatches.rs"]
mod mismatches;
#[path = "analyze_types/null_operands.rs"]
mod null_operands;
