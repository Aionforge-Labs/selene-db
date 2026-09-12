#![allow(missing_docs, clippy::print_stderr)]
//! Absolute bounded path costs, including bindings rather than reachability.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use selene_core::{GraphId, LabelSet, PropertyMap, db_string};
use selene_gql::runtime::product_path::{BoundedPathProgram, PathExecutionLimits};
use selene_gql::{
    EmptyProcedureRegistry, ImplDefinedCaps, TxContext, analyze, lower_path_automata_with_defaults,
    parse,
};
use selene_graph::SharedGraph;
use selene_testing::BenchProfile;
use std::{hint::black_box, time::Duration};

mod path_queries;
mod path_selection;

fn fixture(shape: &str, n: usize) -> (SharedGraph, usize) {
    let graph = SharedGraph::new(GraphId::new(50502));
    let mut tx = graph.begin_write();
    let mut m = tx.mutator();
    let count = if shape == "fanout" {
        1 + 2 * (n / 8)
    } else {
        n
    };
    let nodes: Vec<_> = (0..count)
        .map(|i| {
            m.create_node(
                LabelSet::single(db_string(if i == 0 { "Root" } else { "N" }).unwrap()),
                PropertyMap::new(),
            )
            .unwrap()
        })
        .collect();
    let mut edge = |a: usize, b: usize| {
        m.create_edge(
            db_string("K").unwrap(),
            nodes[a],
            nodes[b],
            PropertyMap::new(),
        )
        .unwrap();
    };
    let expected = match shape {
        "fanout" => {
            let width = n / 8;
            for a in 1..=width {
                edge(0, a);
                for b in width + 1..=width * 2 {
                    edge(a, b);
                }
            }
            1 + width + width * width
        }
        "cycle" => {
            for a in 0..n {
                edge(a, (a + 1) % n);
            }
            4 * n
        }
        "parallel" => {
            for a in 0..n - 1 {
                edge(a, a + 1);
                edge(a, a + 1);
            }
            n + 2 * (n - 1) + 4 * (n - 2) + 8 * (n - 3)
        }
        "sparse" => {
            for a in 0..n - 1 {
                edge(a, a + 1);
            }
            4 * n - 6
        }
        _ => unreachable!(),
    };
    tx.commit().unwrap();
    (graph, expected)
}

fn bounded_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("gql_bounded_paths");
    for n in [64, 256] {
        for shape in ["sparse", "cycle", "fanout", "parallel"] {
            let (graph, expected) = fixture(shape, n);
            let source = if shape == "fanout" {
                "MATCH WALK (a:Root)-[r:K{0,3}]->(b) RETURN a, r, b"
            } else {
                "MATCH WALK (a)-[r:K{0,3}]->(b) RETURN a, r, b"
            };
            let analyzed = analyze(parse(source).unwrap(), &EmptyProcedureRegistry, None).unwrap();
            let set = lower_path_automata_with_defaults(&analyzed).unwrap();
            let program = BoundedPathProgram::compile(&set.automata, &analyzed).unwrap();
            let caps = ImplDefinedCaps::default();
            let tx = TxContext::read_only(
                graph.read(),
                &caps,
                &EmptyProcedureRegistry,
                graph.index_providers(),
            );
            let check = program
                .execute(&tx, PathExecutionLimits::default())
                .unwrap();
            assert_eq!(check.table.row_count(), expected);
            eprintln!("bounded_paths/{shape}/{n}: {:?}", check.stats);
            group.throughput(Throughput::Elements(expected as u64));
            group.bench_with_input(BenchmarkId::new(shape, n), &n, |b, _| {
                b.iter(|| {
                    black_box(
                        program
                            .execute(black_box(&tx), PathExecutionLimits::default())
                            .unwrap(),
                    )
                });
            });
        }
    }
    group.finish();
}

fn config() -> Criterion {
    if let Ok(n) = std::env::var("SELENE_PATH_QUERY_MEMORY") {
        path_queries::memory_child(n.parse().expect("memory child scale"));
        std::process::exit(0);
    }
    let profile = BenchProfile::from_env();
    Criterion::default()
        .sample_size(profile.sample_size())
        .warm_up_time(Duration::from_millis(200))
        .measurement_time(Duration::from_secs(1))
}

criterion_group! { name = benches; config = config(); targets = bounded_paths, path_selection::selected_paths, path_queries::whole_path_queries }
criterion_main!(benches);
