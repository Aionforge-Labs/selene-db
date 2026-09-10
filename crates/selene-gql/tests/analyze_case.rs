//! Analyzer coverage for CASE expression lowering.

use selene_gql::{
    AnalyzedStatement, BinaryOp, EmptyProcedureRegistry, ExprId, PipelineStatement, ReturnItem,
    Statement, ValueExpr, analyze, parse,
};

fn analyze_one(source: &str) -> AnalyzedStatement {
    let statement = parse(source).expect("test input parses");
    analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes")
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

#[test]
fn expr_id_lookup_survives_simple_case_operand_list_base_clone() {
    let analyzed =
        analyze_one("MATCH (n) RETURN CASE n.age WHEN 1, 2 THEN 'a' ELSE 'c' END AS label");
    let items = return_items(&analyzed);
    let ValueExpr::Case { branches, .. } = &items[0].expr else {
        panic!("expected CASE expression, got {:?}", items[0].expr);
    };

    let mut ids = Vec::new();
    collect_case_base_ids(&branches[0].0, &analyzed, &mut ids);

    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1]);
}

fn collect_case_base_ids(expr: &ValueExpr, analyzed: &AnalyzedStatement, ids: &mut Vec<ExprId>) {
    match expr {
        ValueExpr::BinaryOp {
            op: BinaryOp::Eq,
            lhs,
            ..
        } => ids.push(
            analyzed
                .expr_ids
                .get(lhs.as_ref())
                .expect("cloned CASE base resolves to an ExprId"),
        ),
        ValueExpr::BinaryOp {
            op: BinaryOp::Or,
            lhs,
            rhs,
            ..
        } => {
            collect_case_base_ids(lhs, analyzed, ids);
            collect_case_base_ids(rhs, analyzed, ids);
        }
        _ => panic!("expected equality/OR CASE condition, got {expr:?}"),
    }
}
