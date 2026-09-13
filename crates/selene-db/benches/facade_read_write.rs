#![allow(missing_docs)]
//! Complete warm-session query and mutation guards, including returned values.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use selene_db::{CreatePolicy, Database, ExecutionOutcome, ObjectPath, SchemaPath, Value};
use std::{hint::black_box, time::Duration};

fn bench(c: &mut Criterion) {
    let scales: &[usize] = match std::env::var("SELENE_BENCH_PROFILE").ok().as_deref() {
        Some("full" | "stress") => &[1_000, 10_000],
        _ => &[1_000],
    };
    for &scale in scales {
        let db = Database::builder().build();
        let path = ObjectPath::regular("selene", "guard", "g").unwrap();
        db.catalog()
            .create_schema(
                &SchemaPath::regular("selene", "guard").unwrap(),
                CreatePolicy::Strict,
            )
            .unwrap();
        db.catalog()
            .create_graph(&path, None, CreatePolicy::Strict)
            .unwrap();
        let session = db.session(&path).unwrap();
        for start in (0..scale).step_by(100) {
            let rows = (start..(start + 100).min(scale))
                .map(|id| format!("(:Guard {{id: {id}, value: 0}})"))
                .collect::<Vec<_>>()
                .join(", ");
            session.execute(&format!("INSERT {rows}")).unwrap();
        }
        session
            .execute("CALL selene.create_index('Guard', 'id', 'i64')")
            .unwrap();
        let query = "MATCH (n:Guard) WHERE n.id = 1 RETURN n.value";
        let ExecutionOutcome::Rows { result, .. } = session.execute(query).unwrap() else {
            panic!("expected rows")
        };
        assert_eq!(result.rows()[0].values(), &[Value::Int(0)]);
        let mut group = c.benchmark_group("facade_read_write");
        group.bench_function(BenchmarkId::new("indexed_read", scale), |b| {
            b.iter(|| {
                let ExecutionOutcome::Rows { result, .. } =
                    session.execute(black_box(query)).unwrap()
                else {
                    panic!("expected rows")
                };
                black_box(result.rows()[0].values());
            });
        });
        group.bench_function(BenchmarkId::new("indexed_update", scale), |b| {
            b.iter(|| {
                black_box(
                    session
                        .execute(black_box(
                            "MATCH (n:Guard) WHERE n.id = 1 SET n.value = 1 - n.value",
                        ))
                        .unwrap(),
                )
            });
        });
        group.finish();
    }
}

fn config() -> Criterion {
    Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(1_500))
}

criterion_group! { name = benches; config = config(); targets = bench }
criterion_main!(benches);
