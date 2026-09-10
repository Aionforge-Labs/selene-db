#![allow(missing_docs)]
//! Criterion benches for plan-corpus analysis throughput.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod common;
mod semantic_measurement;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use selene_gql::{EmptyProcedureRegistry, analyze, parse};
use selene_testing::PlanCorpus;

fn bench_analyze_corpus(c: &mut Criterion) {
    let entries = common::corpus_entries();
    let statements = entries
        .iter()
        .map(|entry| std::sync::Arc::new(parse(entry.source).expect("source parses")))
        .collect::<Vec<_>>();
    let empty = EmptyProcedureRegistry;
    let mock = PlanCorpus::standard_mock_registry();
    let mut group = c.benchmark_group("gql_analyze_corpus");
    group.throughput(Throughput::Elements(entries.len() as u64));
    group.bench_function("m5c", |b| {
        b.iter(|| {
            for (entry, statement) in entries.iter().zip(statements.iter()) {
                let registry = common::registry_for(entry, &empty, &mock);
                let analyzed = analyze(statement.clone(), registry, None).expect("source analyzes");
                std::hint::black_box(analyzed.expr_types.len());
            }
        });
    });
    group.finish();
}

criterion_group! {
    name = analyze_group;
    config = common::criterion_config();
    targets = bench_analyze_corpus, semantic_measurement::bench
}
criterion_main!(analyze_group);
