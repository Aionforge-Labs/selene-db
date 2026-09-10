//! Matched parsing/analysis fixtures and explicitly partial retained-byte accounting.

use criterion::{BenchmarkId, Criterion};
use selene_gql::analyze::{SemanticTree, semantic::SemanticExpression};
use selene_gql::{
    AnalyzedStatement, BindingId, BindingUse, EmptyProcedureRegistry, ExprId, GqlType,
    ParameterUse, PipelineStatement, ReturnItem, SourceSpan, Statement, ValueExpr, analyze, parse,
};
use std::{hint::black_box, mem::size_of, sync::Arc};

pub(crate) fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("gql_immutable_semantics");
    for scale in [1, 16, 64] {
        let mut text = String::from("RETURN 0");
        for index in 0..scale {
            text.push_str(&format!(" + $p{index} :: INT"));
        }
        let source = Arc::new(parse(&text).expect("measurement fixture parses"));
        let analyzed = analyze(source.clone(), &EmptyProcedureRegistry, None).unwrap();
        assert_eq!(analyzed.parameters.len(), scale);
        assert_eq!(analyzed.expressions.len(), 2 * scale + 1);
        report_memory(scale, &text, &source, &analyzed);
        group.bench_with_input(BenchmarkId::new("parse", scale), &text, |b, text| {
            b.iter(|| black_box(parse(black_box(text)).unwrap()));
        });
        group.bench_with_input(BenchmarkId::new("analyze", scale), &source, |b, source| {
            b.iter(|| {
                black_box(analyze(Arc::clone(source), &EmptyProcedureRegistry, None).unwrap())
            });
        });
    }
    group.finish();
}

// Fixture-specific lower-bound structural accounting, NOT a global allocator
// intercept or RSS measurement. Fail if the fixture grows unaccounted shapes.
// Source owns syntax; semantics owns separate resolved trees. DbString storage
// is shared and omitted, as are Arc headers, allocator overhead, map-node slack,
// and capacity slack in private tables whose API exposes only their length.
#[allow(clippy::print_stderr)]
fn report_memory(scale: usize, text: &str, source: &Statement, analyzed: &AnalyzedStatement) {
    let Statement::Query(query) = source else {
        panic!("query fixture expected");
    };
    let [PipelineStatement::Return(result)] = query.statements.as_slice() else {
        panic!("RETURN fixture expected");
    };
    assert!(query.working_scopes.is_empty() && query.select_origin.is_none());
    assert!(analyzed.calls.is_empty() && analyzed.catalog.is_none());
    let source_bytes = size_of::<Statement>()
        + query.statements.capacity() * size_of::<PipelineStatement>()
        + result.items.capacity() * size_of::<ReturnItem>()
        + result
            .items
            .iter()
            .map(|item| expression_heap(&item.expr))
            .sum::<usize>();
    let scope_bytes = std::mem::size_of_val(analyzed.scopes.scopes())
        + std::mem::size_of_val(analyzed.scopes.declarations())
        + analyzed
            .scopes
            .scopes()
            .iter()
            .map(|scope| {
                (scope.locals.capacity() + scope.imports.capacity()) * size_of::<BindingId>()
            })
            .sum::<usize>();
    let semantic_bytes = size_of::<SemanticTree>() + scope_bytes
        + analyzed.expressions.capacity() * size_of::<SemanticExpression>()
        + analyzed.expressions.iter().map(|expr| expr.children.capacity() * size_of::<ExprId>()).sum::<usize>()
        + analyzed.expr_types.len() * size_of::<selene_gql::AnalyzedType>()
        + analyzed.expr_ids.len() * size_of::<(SourceSpan, u64, ExprId)>()
        + analyzed.parameters.capacity() * size_of::<ParameterUse>()
        + analyzed.references.capacity() * size_of::<BindingUse>()
        // These fixtures have one distinct inline INT declaration per name.
        + scale * size_of::<(selene_core::DbString, GqlType)>();
    // Pin accounting assumptions against accidental fixture drift.
    assert!(analyzed.scopes.declarations().len() < 2);
    eprintln!(
        "F03 memory scale={scale} source_text_bytes={} source_structural_bytes={source_bytes} semantic_structural_lower_bound_bytes={semantic_bytes} shared_handle_bytes={}",
        text.len(),
        size_of::<AnalyzedStatement>()
    );
}

fn expression_heap(expression: &ValueExpr) -> usize {
    match expression {
        ValueExpr::Literal(selene_gql::Literal::Integer(..)) => 0,
        ValueExpr::Parameter {
            declared_type: Some(GqlType::Integer),
            ..
        } => 0,
        ValueExpr::BinaryOp { lhs, rhs, .. } => {
            2 * size_of::<ValueExpr>() + expression_heap(lhs) + expression_heap(rhs)
        }
        _ => panic!("measurement fixture has an unaccounted expression"),
    }
}
