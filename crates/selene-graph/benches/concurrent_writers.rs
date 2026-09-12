#![allow(missing_docs)]
//! Criterion benches for serialized writer queueing under contention.
//!
//! In-memory (no WAL) rows: pure committer queueing over `SharedGraph`.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod common;

use std::sync::atomic::{AtomicBool, Ordering};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use selene_core::{LabelDiff, NodeId, PropertyDiff, Value};
use selene_graph::SharedGraph;
use selene_testing::BenchFixture;

const TOTAL_COMMITS: usize = 1_000;
const UPDATES_PER_COMMIT: usize = 10;
const THREADS: &[usize] = &[1, 2, 4, 8, 16, 32];

fn bench_concurrent_writers(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_writers");
    let fixture = BenchFixture::build(10_000);

    for &threads in THREADS {
        group.throughput(Throughput::Elements(TOTAL_COMMITS as u64));

        // ── In-memory (no WAL): pure committer queueing. ──
        group.bench_function(
            BenchmarkId::from_parameter(format!("threads{threads}")),
            |b| {
                b.iter_batched(
                    || SharedGraph::from_graph(fixture.graph().clone()),
                    |shared| {
                        run_writers(&shared, threads);
                        std::hint::black_box(shared.read().node_count())
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_function(
            BenchmarkId::from_parameter(format!("threads{threads}_with_readers8")),
            |b| {
                b.iter_batched(
                    || SharedGraph::from_graph(fixture.graph().clone()),
                    |shared| {
                        run_writers_with_readers(&shared, threads);
                        std::hint::black_box(shared.read().node_count())
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

fn run_writers(shared: &SharedGraph, threads: usize) {
    std::thread::scope(|scope| {
        let handles = (0..threads)
            .map(|thread_idx| {
                scope.spawn(move || {
                    writer_loop(shared, thread_idx, TOTAL_COMMITS / threads);
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().expect("writer thread joins");
        }
    });
}

fn run_writers_with_readers(shared: &SharedGraph, threads: usize) {
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let reader_handles = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    let mut reads = 0_usize;
                    while !done.load(Ordering::Relaxed) {
                        let snapshot = shared.read();
                        std::hint::black_box(snapshot.node_properties(NodeId::new(1)));
                        reads = reads.wrapping_add(1);
                    }
                    std::hint::black_box(reads);
                })
            })
            .collect::<Vec<_>>();

        let writer_handles = (0..threads)
            .map(|thread_idx| {
                scope.spawn(move || {
                    writer_loop(shared, thread_idx, TOTAL_COMMITS / threads);
                })
            })
            .collect::<Vec<_>>();

        for handle in writer_handles {
            handle.join().expect("writer thread joins");
        }
        done.store(true, Ordering::Relaxed);
        for handle in reader_handles {
            handle.join().expect("reader thread joins");
        }
    });
}

fn writer_loop(shared: &SharedGraph, thread_idx: usize, commits: usize) {
    let score = selene_core::db_string("score").expect("score key fits DB string cap");
    for commit_idx in 0..commits {
        let mut txn = shared.begin_write();
        {
            let mut mutator = txn.mutator();
            for update_idx in 0..UPDATES_PER_COMMIT {
                let node = node_for(thread_idx, update_idx);
                let value = ((commit_idx * UPDATES_PER_COMMIT) + update_idx) as i64;
                let diff = PropertyDiff::new([(score.clone(), Value::Int(value))], [])
                    .expect("property diff");
                mutator
                    .update_node(
                        node,
                        LabelDiff::new([], []).expect("label diff is valid"),
                        diff,
                    )
                    .expect("node update succeeds");
            }
        }
        txn.commit().expect("writer commit succeeds");
    }
}

fn node_for(thread_idx: usize, update_idx: usize) -> NodeId {
    NodeId::new(((thread_idx * UPDATES_PER_COMMIT + update_idx) % 10_000) as u64 + 1)
}

criterion_group! {
    name = concurrent_writers;
    config = common::criterion_config();
    targets = bench_concurrent_writers
}
criterion_main!(concurrent_writers);
