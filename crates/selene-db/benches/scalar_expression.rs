#![allow(missing_docs)]
//! Isolated expression-index workloads; no unrelated catalog fixture allocation.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use selene_db::{CreatePolicy, Database, ExecutionOutcome, ObjectPath, SchemaPath, Value};
use std::{hint::black_box, time::Duration};

#[path = "scalar_expression/workloads.rs"]
mod workloads;

fn schema(name: &str) -> SchemaPath {
    SchemaPath::regular("selene", name).unwrap()
}

fn graph(schema: &str, name: &str) -> ObjectPath {
    ObjectPath::regular("selene", schema, name).unwrap()
}

fn config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(500))
}

criterion_group! { name = benches; config = config(); targets = workloads::bench }
criterion_main!(benches);
