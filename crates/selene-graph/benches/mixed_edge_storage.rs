//! Controlled directed/mixed storage comparison. Invoke through run-benches.sh.

use std::{hint::black_box, io::Write, process::Command};

use criterion::{BatchSize, BenchmarkId, Throughput};
use selene_core::{EdgeDirectionality, GraphId, LabelSet, NodeId, PropertyMap, db_string};
use selene_graph::{SeleneGraph, SharedGraph};

mod common;

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn nodes_only() -> (SharedGraph, Vec<NodeId>) {
    let graph = SharedGraph::new(GraphId::new(1));
    let mut tx = graph.begin_write();
    let nodes = (0..1024)
        .map(|_| {
            tx.mutator()
                .create_node(LabelSet::new(), PropertyMap::new())
                .unwrap()
        })
        .collect();
    tx.commit().unwrap();
    (graph, nodes)
}

fn insert(graph: &SharedGraph, nodes: &[NodeId], edges: usize, mixed: bool) {
    let mut tx = graph.begin_write();
    let mut m = tx.mutator();
    let label = db_string("E").unwrap();
    for index in 0..edges {
        let kind = if mixed && index % 2 == 0 {
            EdgeDirectionality::Undirected
        } else {
            EdgeDirectionality::Directed
        };
        m.create_mixed_edge(
            label.clone(),
            nodes[index % nodes.len()],
            nodes[(index + 1) % nodes.len()],
            kind,
            PropertyMap::new(),
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

fn incidence(graph: &SeleneGraph, nodes: &[NodeId]) -> usize {
    nodes
        .iter()
        .map(|&node| {
            [
                graph.outgoing_edges(node),
                graph.incoming_edges(node),
                graph.undirected_edges(node),
            ]
            .into_iter()
            .flatten()
            .flat_map(|entry| entry.iter())
            .map(|edge| {
                black_box(edge.edge_id);
                1
            })
            .sum::<usize>()
        })
        .sum()
}

// Native process RSS, deliberately not an allocator-exact ownership claim.
// Each measurement runs in a fresh process to avoid prior retained arenas.
fn rss_bytes() -> i64 {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    assert!(output.status.success(), "native ps RSS measurement failed");
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse::<i64>()
        .unwrap()
        * 1024
}

fn memory_child(spec: &str) {
    let (kind, count) = spec.split_once(':').unwrap();
    let edges: usize = count.parse().unwrap();
    let (graph, nodes) = nodes_only();
    let before = rss_bytes();
    insert(&graph, &nodes, edges, kind == "mixed");
    assert_eq!(incidence(&graph.read(), &nodes), edges * 2);
    let delta = rss_bytes() - before;
    writeln!(
        std::io::stdout().lock(),
        "memory kind={kind} edges={edges} rss_delta_bytes={delta} rss_bytes_per_edge={:.2}",
        delta as f64 / edges as f64
    )
    .unwrap();
    black_box(graph);
}

fn main() {
    if let Ok(spec) = std::env::var("SELENE_MIXED_EDGE_MEMORY_CHILD") {
        memory_child(&spec);
        return;
    }
    let mut criterion = common::criterion_config().configure_from_args();
    let mut group = criterion.benchmark_group("mixed_edge_storage");
    for edges in [1_000, 10_000] {
        for (kind, mixed) in [("directed", false), ("mixed", true)] {
            let output = Command::new(std::env::current_exe().unwrap())
                .env("SELENE_MIXED_EDGE_MEMORY_CHILD", format!("{kind}:{edges}"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "memory child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::io::stdout().write_all(&output.stdout).unwrap();
            let (graph, nodes) = nodes_only();
            insert(&graph, &nodes, edges, mixed);
            assert_eq!(graph.read().edge_count(), edges);
            assert_eq!(incidence(&graph.read(), &nodes), 2 * edges);
            let read = graph.read();
            group.throughput(Throughput::Elements(edges as u64));
            group.bench_function(BenchmarkId::new(format!("{kind}_incidence"), edges), |b| {
                b.iter(|| black_box(incidence(&read, &nodes)))
            });
            group.bench_function(BenchmarkId::new(format!("{kind}_enumerate"), edges), |b| {
                b.iter(|| {
                    let candidates = read.live_edge_candidates().unwrap();
                    black_box(
                        candidates
                            .iter()
                            .map(|id| read.edge_directionality(id).unwrap())
                            .filter(|kind| *kind == EdgeDirectionality::Undirected)
                            .count(),
                    )
                })
            });
            group.bench_function(
                BenchmarkId::new(format!("{kind}_create_commit"), edges),
                |b| {
                    b.iter_batched(
                        nodes_only,
                        |(graph, nodes)| {
                            insert(&graph, &nodes, edges, mixed);
                            black_box(graph)
                        },
                        BatchSize::SmallInput,
                    )
                },
            );
        }
    }
    group.finish();
    criterion.final_summary();
}
