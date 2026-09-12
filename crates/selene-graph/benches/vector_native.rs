#![allow(missing_docs)]
//! F04-PR07 CPU exact/ANN quality and lifecycle cost on the same corpus.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod common;

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use selene_core::{
    CancellationChecker, DbString, GraphId, LabelSet, NodeId, PropertyMap, Value, VectorMetric,
    VectorValue, db_string,
};
use selene_graph::{
    ApproximateVectorSearchOptions, CandidateSet, Node, SeleneGraph, SharedGraph, VectorIndexKind,
    VectorNodeSearchHit,
};
use selene_testing::BenchProfile;

const K: usize = 10;
const WIDTH: usize = 64;
const QUERIES: usize = 8;

struct Fixture {
    graph: SeleneGraph,
    label: DbString,
    property: DbString,
    ids: Vec<NodeId>,
    queries: Vec<VectorValue>,
}

fn vector(seed: usize, dimension: usize) -> VectorValue {
    // Deterministic finite dense vectors, independent of any model service.
    let mut state = (seed as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15);
    VectorValue::new(
        (0..dimension)
            .map(|_| {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                let bits = state.wrapping_mul(0x2545f4914f6cdd1d) >> 40;
                bits as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect::<Vec<_>>(),
    )
    .unwrap()
}

impl Fixture {
    fn new(n: usize, dimension: usize) -> Self {
        let shared = SharedGraph::new(GraphId::new(407));
        let label = db_string("Memory").unwrap();
        let property = db_string("embedding").unwrap();
        let mut txn = shared.begin_write();
        let ids = (0..n)
            .map(|seed| {
                txn.mutator()
                    .create_node(
                        LabelSet::single(label.clone()),
                        PropertyMap::from_pairs([(
                            property.clone(),
                            Value::Vector(vector(seed, dimension)),
                        )])
                        .unwrap(),
                    )
                    .unwrap()
            })
            .collect();
        txn.commit().unwrap();
        Self {
            graph: shared.read().as_ref().clone(),
            label,
            property,
            ids,
            queries: (0..QUERIES).map(|q| vector(n + q, dimension)).collect(),
        }
    }

    fn indexed(&self, kind: VectorIndexKind, dimension: usize) -> SeleneGraph {
        let shared = SharedGraph::from_graph(self.graph.clone());
        shared
            .create_vector_index(
                self.label.clone(),
                self.property.clone(),
                kind,
                dimension as u32,
            )
            .unwrap();
        shared.read().as_ref().clone()
    }

    fn exact(&self, ids: &[NodeId], all: bool) -> Vec<Vec<VectorNodeSearchHit>> {
        self.queries
            .iter()
            .map(|q| {
                if all {
                    self.graph
                        .exact_vector_search_nodes(
                            &self.label,
                            &self.property,
                            q,
                            VectorMetric::Cosine,
                            K,
                        )
                        .unwrap()
                } else {
                    self.graph
                        .score_vector_nodes(&self.property, q, ids, VectorMetric::Cosine, K)
                        .unwrap()
                }
            })
            .collect()
    }

    fn ann(
        &self,
        graph: &SeleneGraph,
        candidates: &CandidateSet<Node>,
        all: bool,
    ) -> Vec<Vec<VectorNodeSearchHit>> {
        self.queries
            .iter()
            .map(|q| {
                let options = ApproximateVectorSearchOptions::new(VectorMetric::Cosine, K, WIDTH);
                if all {
                    graph
                        .approximate_vector_search_nodes_checked(
                            &self.label,
                            &self.property,
                            q,
                            options,
                            CancellationChecker::disabled(),
                        )
                        .unwrap()
                } else {
                    graph
                        .approximate_vector_search_nodes_in_candidates_checked(
                            &self.label,
                            &self.property,
                            q,
                            candidates,
                            options,
                            CancellationChecker::disabled(),
                        )
                        .unwrap()
                }
            })
            .collect()
    }
}

fn recall_basis_points(
    exact: &[Vec<VectorNodeSearchHit>],
    ann: &[Vec<VectorNodeSearchHit>],
) -> usize {
    let mut matched = 0;
    let mut total = 0;
    for (expected, hits) in exact.iter().zip(ann) {
        total += expected.len();
        matched += hits
            .iter()
            .filter(|hit| expected.iter().any(|e| e.node_id == hit.node_id))
            .count();
    }
    (10_000 * matched).checked_div(total).unwrap_or(10_000)
}

fn bench(c: &mut Criterion) {
    let scales = std::env::var("SELENE_VECTOR_BENCH_SCALES")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|v| v.parse::<usize>().unwrap())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| BenchProfile::from_env().scales().to_vec());
    let mut group = c.benchmark_group("native_vector_cpu");
    for n in scales {
        for dimension in [128, 768] {
            let fixture = Fixture::new(n, dimension);
            for stride in [1, 10, 100] {
                let ids = fixture
                    .ids
                    .iter()
                    .step_by(stride)
                    .copied()
                    .collect::<Vec<_>>();
                let all = stride == 1;
                let name = format!("n{n}_d{dimension}_eligible{}_q{QUERIES}_k{K}", ids.len());
                let exact = fixture.exact(&ids, all);
                group.bench_function(BenchmarkId::new("exact", &name), |b| {
                    b.iter(|| black_box(fixture.exact(&ids, all)))
                });
                for (kind_name, kind) in [
                    ("hnsw", VectorIndexKind::HnswCosine),
                    ("ivf", VectorIndexKind::IvfCosine),
                    ("turbo", VectorIndexKind::TurboQuantCosine),
                ] {
                    let graph = fixture.indexed(kind, dimension);
                    let candidates = graph.bind_node_candidates(ids.iter().copied()).unwrap();
                    let hits = fixture.ann(&graph, &candidates, all);
                    assert!(hits.iter().flatten().all(|h| ids.contains(&h.node_id)));
                    // Shared metric kernel: recall is candidate coverage, not an
                    // independent numerical oracle (see native_vectors tests).
                    let recall = recall_basis_points(&exact, &hits);
                    let returned: usize = hits.iter().map(Vec::len).sum();
                    let memory = graph
                        .vector_index_for(&fixture.label, &fixture.property)
                        .unwrap()
                        .memory_usage();
                    let id = format!(
                        "{name}_ef{WIDTH}_recallbp{recall}_hits{returned}_indexbytes{}_reachablebytes{}",
                        memory.estimated_index_bytes, memory.estimated_reachable_bytes
                    );
                    group.bench_function(BenchmarkId::new(kind_name, id), |b| {
                        b.iter(|| black_box(fixture.ann(&graph, &candidates, all)))
                    });
                    if !all {
                        continue;
                    }
                    for rebuild in [false, true] {
                        let operation = if rebuild { "rebuild" } else { "build" };
                        group.bench_function(
                            BenchmarkId::new(format!("{kind_name}_{operation}"), &name),
                            |b| {
                                b.iter_custom(|iterations| {
                                    let mut elapsed = Duration::ZERO;
                                    for _ in 0..iterations {
                                        let source = if rebuild { &graph } else { &fixture.graph };
                                        let shared = SharedGraph::from_graph(source.clone());
                                        let start = Instant::now();
                                        if rebuild {
                                            black_box(shared.rebuild_vector_indexes().unwrap());
                                        } else {
                                            shared
                                                .create_vector_index(
                                                    fixture.label.clone(),
                                                    fixture.property.clone(),
                                                    kind,
                                                    dimension as u32,
                                                )
                                                .unwrap();
                                        }
                                        elapsed += start.elapsed();
                                    }
                                    elapsed
                                })
                            },
                        );
                    }
                }
            }
        }
    }
    group.finish();
}

criterion_group! { name = benches; config = common::criterion_config(); targets = bench }
criterion_main!(benches);
