#![allow(missing_docs)]
//! Balanced, consumed lookup/validation/write guards; no alternative engine mode.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod common;
#[path = "read_write_guard/fixture.rs"]
mod fixture;
#[path = "read_write_guard/memory.rs"]
mod memory;

use criterion::{BatchSize, BenchmarkId, Criterion};
use selene_core::{LabelDiff, LabelSet, PropertyDiff, PropertyMap, Value, db_string};
use selene_graph::{SeleneGraph, SharedGraph};
use selene_testing::{BenchFixture, BenchProfile};
use std::hint::black_box;

fn reads(c: &mut Criterion, graph: &SeleneGraph, name: &str, scale: usize, sparse: bool) {
    let ids: Vec<_> = graph.live_node_candidates().unwrap().iter().collect();
    let label = if sparse {
        fixture::label(1)
    } else {
        db_string("Person").unwrap()
    };
    let key = db_string(if sparse { "value" } else { "age" }).unwrap();
    let value = Value::Int(if sparse { 1 } else { 20 });
    assert!(
        graph
            .node_property_eq_cardinality(&label, &key, &value)
            .unwrap()
            > 0
    );
    let mut group = c.benchmark_group(format!("read_write_guard/{name}"));
    group.bench_function(BenchmarkId::new("node_fetch", scale), |b| {
        let mut cursor = 0;
        b.iter(|| {
            cursor = (cursor + 7919) % ids.len();
            black_box(
                graph
                    .node_properties(black_box(ids[cursor]))
                    .map(PropertyMap::len),
            )
        });
    });
    group.bench_function(BenchmarkId::new("label_lookup", scale), |b| {
        b.iter(|| black_box(graph.node_label_cardinality(black_box(&label))));
    });
    group.bench_function(BenchmarkId::new("edge_label_lookup", scale), |b| {
        let edge_label = if sparse {
            label.clone()
        } else {
            db_string("KNOWS").unwrap()
        };
        b.iter(|| black_box(graph.edge_label_cardinality(black_box(&edge_label))));
    });
    group.bench_function(BenchmarkId::new("typed_index_point", scale), |b| {
        b.iter(|| black_box(graph.node_property_eq_cardinality(black_box(&label), &key, &value)));
    });
    let candidates = graph
        .bind_node_candidates(ids.iter().copied().take(1024))
        .unwrap();
    let empty = graph.bind_node_candidates([]).unwrap();
    // Difference with empty still checks every forward/reverse/liveness pair.
    // Each repetition invokes the public checked operation, not raw row access.
    for repetitions in [1, 8] {
        group.bench_function(
            BenchmarkId::new(format!("checked_candidates_x{repetitions}"), scale),
            |b| {
                b.iter(|| {
                    for _ in 0..repetitions {
                        drop(black_box(
                            graph
                                .difference_candidates(black_box(&candidates), &empty)
                                .unwrap(),
                        ));
                    }
                });
            },
        );
    }
    group.bench_function(BenchmarkId::new("clone_drop", scale), |b| {
        b.iter(|| black_box(graph.clone()));
    });
    group.finish();
}

fn writes(c: &mut Criterion, graph: &SeleneGraph, name: &str, scale: usize) {
    // Reset outside timing so all samples see identical IDs and row pressure.
    let id = graph.live_node_candidates().unwrap().iter().next().unwrap();
    let key = db_string("guard_update").unwrap();
    let label = LabelSet::single(db_string("GuardCreated").unwrap());
    let mut group = c.benchmark_group(format!("read_write_guard/{name}"));
    group.bench_function(BenchmarkId::new("mixed_r60w40", scale), |b| {
        b.iter_batched(
            || SharedGraph::from_graph(graph.clone()),
            |shared| {
                // 60 consumed reads + 20 updates + 10 creates + 10 deletes, one
                // committed batch. Fixture reconstruction and final drop excluded.
                for _ in 0..60 {
                    black_box(shared.read().node_properties(id).map(PropertyMap::len));
                }
                let mut tx = shared.begin_write();
                {
                    let mut m = tx.mutator();
                    for value in 1..=20 {
                        m.update_node(
                            id,
                            LabelDiff::new([], []).unwrap(),
                            PropertyDiff::new([(key.clone(), Value::Int(value))], []).unwrap(),
                        )
                        .unwrap();
                    }
                    for _ in 0..10 {
                        let created = m.create_node(label.clone(), PropertyMap::new()).unwrap();
                        m.delete_node(created).unwrap();
                    }
                }
                black_box(tx.commit().unwrap());
                black_box(shared)
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn main() {
    if let Ok(scale) = std::env::var("SELENE_LOOKUP_MEMORY_CHILD") {
        memory::child(scale.parse().unwrap());
        return;
    }
    let mut c = common::criterion_config().configure_from_args();
    for &scale in BenchProfile::from_env().scales() {
        let canonical = BenchFixture::build(scale);
        reads(&mut c, canonical.graph(), "canonical", scale, false);
        // Canonical fixture remains unchanged, including its three-edge shape.
        writes(&mut c, canonical.graph(), "canonical", scale);
        let sparse = fixture::sparse(scale.max(fixture::LABELS * 2));
        reads(&mut c, &sparse, "sparse", scale, true);
        writes(&mut c, &sparse, "sparse", scale);
        memory::measure(scale);
    }
    c.final_summary();
}
