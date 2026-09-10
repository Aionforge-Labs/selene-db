//! BRIEF-118 analyzer recursion bound coverage.

use selene_gql::{
    AnalysisError, BinaryOp, EmptyProcedureRegistry, Literal, PipelineStatement, QueryPipeline,
    ReturnClause, ReturnItem, SourceSpan, Statement, ValueExpr, analyze,
};

fn addition_statement(terms: usize) -> Statement {
    let span = SourceSpan::new(0, 1);
    let mut expr = ValueExpr::Literal(Literal::Integer(1, span));
    for _ in 1..terms {
        expr = ValueExpr::BinaryOp {
            op: BinaryOp::Add,
            lhs: Box::new(expr),
            rhs: Box::new(ValueExpr::Literal(Literal::Integer(1, span))),
            span,
        };
    }
    Statement::Query(QueryPipeline {
        working_scopes: Vec::new(),
        select_origin: None,
        statements: vec![PipelineStatement::Return(ReturnClause {
            distinct: false,
            star: false,
            items: vec![ReturnItem {
                expr,
                alias: None,
                span,
            }],
            group_by: None,
            having: None,
            span,
        })],
        span,
    })
}

#[test]
fn analyzer_rejects_deep_expression_without_stack_overflow() {
    let statement = addition_statement(1000);
    let error = match analyze(statement, &EmptyProcedureRegistry, None) {
        Ok(_) => panic!("deep expression should exceed analyzer depth"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        AnalysisError::RecursionLimitExceeded { depth: 257 }
    ));
    assert_eq!(error.gqlstatus().as_str(), "5GQL1");
}

#[test]
fn analyzer_accepts_reasonable_expression_depth() {
    let statement = addition_statement(100);

    analyze(statement, &EmptyProcedureRegistry, None).expect("100-term expression analyzes");
}
