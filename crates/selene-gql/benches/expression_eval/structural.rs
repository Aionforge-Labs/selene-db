//! Owned descriptor conversion and borrowed semantic lookup, without interning.

use std::{hint::black_box, mem::size_of};

use criterion::{BenchmarkId, Criterion};
use selene_core::{DbString, StructuralType, db_string};
use selene_gql::{
    EmptyProcedureRegistry, GqlType, RecordType, analyze, lower_value_type, normalize_value_type,
    parse,
};

#[allow(clippy::print_stderr)]
pub(super) fn bench_structural_types(c: &mut Criterion) {
    let mut group = c.benchmark_group("gql_structural_types");
    for count in [1, 32] {
        let fields: Vec<_> = (0..count)
            .map(|i| {
                (
                    db_string(&format!("field_{i:02}")).unwrap(),
                    GqlType::List(Box::new(GqlType::NotNull(Box::new(GqlType::Integer)))),
                )
            })
            .collect();
        let source = GqlType::Record(RecordType::Closed(fields));
        let structural = normalize_value_type(&source).unwrap();
        assert_eq!(
            normalize_value_type(&lower_value_type(&structural)).unwrap(),
            structural
        );
        // Root + named-field slice + boxed list elements. Excludes names' heap
        // storage, Arc headers, allocator metadata and source/planner carriers.
        let retained = size_of::<StructuralType>()
            + count * (size_of::<(DbString, StructuralType)>() + size_of::<StructuralType>());
        eprintln!(
            "[gql_structural_types] fields={count} retained_descriptor_payload_lower_bound={retained}B; normalize/lower include output allocation and drop; no global pool"
        );
        group.bench_function(BenchmarkId::new("normalize", count), |b| {
            b.iter(|| {
                black_box(normalize_value_type(black_box(&source)).unwrap());
            })
        });
        group.bench_function(BenchmarkId::new("lower", count), |b| {
            b.iter(|| {
                black_box(lower_value_type(black_box(&structural)));
            })
        });
    }
    let analyzed = analyze(
        parse("RETURN {a: [1, 2], b: 'text'} AS result").unwrap(),
        &EmptyProcedureRegistry,
        None,
    )
    .unwrap();
    let id = analyzed.expr_types.iter().last().unwrap().0;
    let expected = analyzed.expr_types.structural_type(id).clone();
    assert_eq!(analyzed.expr_types.structural_type(id), &expected);
    group.bench_function("borrowed_lookup", |b| {
        b.iter(|| {
            black_box(analyzed.expr_types.structural_type(black_box(id)));
        })
    });
    group.finish();
}
