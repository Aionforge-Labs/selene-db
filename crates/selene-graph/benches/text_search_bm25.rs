#![allow(missing_docs)]
//! Criterion benches for exact and indexed BM25 text search.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod common;
mod text_json_boundary;
mod text_search_bm25_hybrid;

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use selene_core::{
    CancellationChecker, DbString, GraphId, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap,
    Value, db_string,
};
use selene_graph::{SeleneGraph, SharedGraph, TextIndex};
use selene_testing::BenchProfile;
use text_search_bm25_hybrid::bench_hybrid_bm25_vector;

const TOPICS: [&str; 8] = [
    "gql",
    "vector",
    "memory",
    "planner",
    "storage",
    "agent",
    "schema",
    "retrieval",
];
const STATES: [&str; 4] = ["current", "stale", "draft", "verified"];
const READS_PER_CYCLE: usize = 60;
const WRITES_PER_CYCLE: usize = 40;
const OPS_PER_CYCLE: usize = READS_PER_CYCLE + WRITES_PER_CYCLE;
const TEXT_TOP_K: usize = 10;

fn bench_exact_bm25(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_text_bm25_exact");
    for &scale in BenchProfile::from_env().scales() {
        let fixture = TextFixture::build(scale);
        group.throughput(Throughput::Elements(scale as u64));
        group.bench_with_input(
            BenchmarkId::new("topic_query", format!("n{scale}_k10")),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let hits = fixture
                        .graph
                        .exact_text_search_nodes(
                            &fixture.label,
                            &fixture.property,
                            &fixture.query,
                            10,
                        )
                        .expect("BM25 search succeeds");
                    std::hint::black_box(hits.len());
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("topic_query_checked_with_deadline", format!("n{scale}_k10")),
            &fixture,
            |b, fixture| {
                let checker = deadline_checker();
                b.iter(|| {
                    let hits = fixture
                        .graph
                        .exact_text_search_nodes_checked(
                            &fixture.label,
                            &fixture.property,
                            &fixture.query,
                            10,
                            checker,
                        )
                        .expect("BM25 search succeeds");
                    std::hint::black_box(hits.len());
                });
            },
        );
    }
    group.finish();
}

fn bench_indexed_bm25(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_text_bm25_indexed");
    for &scale in BenchProfile::from_env().scales() {
        let fixture = TextFixture::build(scale);
        group.throughput(Throughput::Elements(scale as u64));
        group.bench_with_input(
            BenchmarkId::new("prebuilt_topic_query", format!("n{scale}_k10")),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let hits = fixture.index.search(&fixture.query, 10);
                    std::hint::black_box(hits.len());
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new(
                "prebuilt_topic_query_candidates_sorted",
                format!("n{scale}_k10"),
            ),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let hits = fixture.index.search_candidates(
                        &fixture.query,
                        fixture.sorted_candidates(),
                        10,
                    );
                    std::hint::black_box(hits.len());
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new(
                "prebuilt_topic_query_candidates_reverse",
                format!("n{scale}_k10"),
            ),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let hits = fixture.index.search_candidates(
                        &fixture.query,
                        fixture.reverse_candidates(),
                        10,
                    );
                    std::hint::black_box(hits.len());
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new(
                "prebuilt_topic_query_candidates_partial_sorted",
                format!("n{scale}_k10"),
            ),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let hits = fixture.index.search_candidates(
                        &fixture.query,
                        fixture.partial_sorted_candidates(),
                        10,
                    );
                    std::hint::black_box(hits.len());
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new(
                "prebuilt_topic_query_candidates_partial_reverse",
                format!("n{scale}_k10"),
            ),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let hits = fixture.index.search_candidates(
                        &fixture.query,
                        fixture.partial_reverse_candidates(),
                        10,
                    );
                    std::hint::black_box(hits.len());
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("registered_topic_query", format!("n{scale}_k10")),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let hits = fixture.registered_index.search(&fixture.query, 10);
                    std::hint::black_box(hits.len());
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("transient_build_query", format!("n{scale}_k10")),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let hits = fixture
                        .graph
                        .indexed_text_search_nodes(
                            &fixture.label,
                            &fixture.property,
                            &fixture.query,
                            10,
                        )
                        .expect("indexed BM25 search succeeds");
                    std::hint::black_box(hits.len());
                });
            },
        );
    }
    group.finish();
}

fn bench_mixed_bm25(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_text_bm25_mixed");
    for &scale in BenchProfile::from_env().scales() {
        group.throughput(Throughput::Elements(OPS_PER_CYCLE as u64));
        group.bench_with_input(
            BenchmarkId::new("registered_query_update_r60w40", format!("n{scale}_k10")),
            &scale,
            |b, &scale| {
                b.iter_batched(
                    || TextMixedFixture::build(scale),
                    |fixture| std::hint::black_box(fixture.run_cycle()),
                    BatchSize::LargeInput,
                );
            },
        );

        group.throughput(Throughput::Elements(WRITES_PER_CYCLE as u64));
        group.bench_with_input(
            BenchmarkId::new("write_registered_update_w40", format!("n{scale}")),
            &scale,
            |b, &scale| {
                b.iter_batched(
                    || TextMixedFixture::build(scale),
                    |fixture| std::hint::black_box(fixture.run_write_cycle()),
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

fn bench_text_rebuild(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_text_bm25_rebuild");
    for &scale in BenchProfile::from_env().scales() {
        let fixture = TextRebuildFixture::build(scale);
        group.throughput(Throughput::Elements(scale as u64));
        group.bench_with_input(
            BenchmarkId::new("create_registered_index", format!("n{scale}")),
            &fixture,
            |b, fixture| {
                b.iter_batched(
                    || SharedGraph::from_graph(fixture.graph.as_ref().clone()),
                    |shared| {
                        shared
                            .create_text_index(fixture.label.clone(), fixture.property.clone())
                            .expect("bench text index registration succeeds");
                        let snapshot = shared.read();
                        let index = snapshot
                            .text_index_for(&fixture.label, &fixture.property)
                            .expect("bench text index exists after create");
                        assert_eq!(index.document_count(), fixture.scale);
                        std::hint::black_box((shared, index.document_count()))
                    },
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new(
                "compact_registered_after_delete",
                format!("n{scale}_del{}", fixture.delete_count()),
            ),
            &fixture,
            |b, fixture| {
                b.iter_batched(
                    || {
                        let shared =
                            SharedGraph::from_graph(fixture.indexed_graph.as_ref().clone());
                        delete_targets(&shared, &fixture.delete_ids);
                        shared
                    },
                    |shared| {
                        let before = shared.read();
                        assert_eq!(before.node_count(), fixture.live_after_delete());
                        assert_eq!(
                            before.node_store.len() - before.node_count(),
                            fixture.delete_count()
                        );
                        drop(before);
                        let report = shared.compact().expect("bench text compaction succeeds");
                        assert_eq!(report.reclaimed_nodes as usize, fixture.delete_count());
                        let after = shared.read();
                        let index = after
                            .text_index_for(&fixture.label, &fixture.property)
                            .expect("bench text index survives compaction");
                        assert_eq!(after.node_count(), fixture.live_after_delete());
                        assert_eq!(index.document_count(), fixture.live_after_delete());
                        std::hint::black_box((shared, report, index.document_count()))
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

struct TextFixture {
    graph: Arc<SeleneGraph>,
    label: DbString,
    property: DbString,
    query: String,
    sorted_candidates: Vec<NodeId>,
    reverse_candidates: Vec<NodeId>,
    partial_sorted_candidates: Vec<NodeId>,
    partial_reverse_candidates: Vec<NodeId>,
    index: TextIndex,
    registered_index: Arc<TextIndex>,
}

impl TextFixture {
    fn build(scale: usize) -> Self {
        let shared = SharedGraph::new(GraphId::new(431_201));
        let label = db_string("BenchTextDoc").expect("bench label fits DB string cap");
        let property = db_string("body").expect("bench property fits DB string cap");
        {
            let mut txn = shared.begin_write();
            let mut mutator = txn.mutator();
            let mut sorted_candidates = Vec::with_capacity(scale);
            for row in 0..scale {
                let topic = TOPICS[row % TOPICS.len()];
                let neighbor = TOPICS[(row + 3) % TOPICS.len()];
                let state = STATES[row % STATES.len()];
                let text = format!(
                    "{topic} {state} agent memory fact supports {neighbor} retrieval evidence"
                );
                let node = mutator
                    .create_node(
                        LabelSet::single(label.clone()),
                        props(
                            &property,
                            Value::String(db_string(&text).expect("bench text fits DB string cap")),
                        ),
                    )
                    .expect("bench node inserts");
                sorted_candidates.push(node);
            }
            txn.commit().expect("bench fixture commits");
            let mut reverse_candidates = sorted_candidates.clone();
            reverse_candidates.reverse();
            let partial_sorted_candidates = sorted_candidates
                .iter()
                .step_by(4)
                .copied()
                .collect::<Vec<_>>();
            let mut partial_reverse_candidates = partial_sorted_candidates.clone();
            partial_reverse_candidates.reverse();
            Self::finish(
                shared,
                label,
                property,
                sorted_candidates,
                reverse_candidates,
                partial_sorted_candidates,
                partial_reverse_candidates,
            )
        }
    }

    fn finish(
        shared: SharedGraph,
        label: DbString,
        property: DbString,
        sorted_candidates: Vec<NodeId>,
        reverse_candidates: Vec<NodeId>,
        partial_sorted_candidates: Vec<NodeId>,
        partial_reverse_candidates: Vec<NodeId>,
    ) -> Self {
        shared
            .create_text_index(label.clone(), property.clone())
            .expect("bench text index registers");
        let graph = shared.read();
        let index = graph
            .build_text_index(&label, &property)
            .expect("bench text index builds");
        let registered_index = graph
            .text_index_for(&label, &property)
            .expect("registered bench text index exists");
        Self {
            graph,
            label,
            property,
            query: "gql current retrieval evidence".to_owned(),
            sorted_candidates,
            reverse_candidates,
            partial_sorted_candidates,
            partial_reverse_candidates,
            index,
            registered_index,
        }
    }

    fn sorted_candidates(&self) -> &[NodeId] {
        &self.sorted_candidates
    }

    fn reverse_candidates(&self) -> &[NodeId] {
        &self.reverse_candidates
    }

    fn partial_sorted_candidates(&self) -> &[NodeId] {
        &self.partial_sorted_candidates
    }

    fn partial_reverse_candidates(&self) -> &[NodeId] {
        &self.partial_reverse_candidates
    }
}

struct TextRebuildFixture {
    graph: Arc<SeleneGraph>,
    indexed_graph: Arc<SeleneGraph>,
    label: DbString,
    property: DbString,
    scale: usize,
    delete_ids: Vec<NodeId>,
}

impl TextRebuildFixture {
    fn build(scale: usize) -> Self {
        let scale = scale.max(READS_PER_CYCLE).max(WRITES_PER_CYCLE);
        let shared = SharedGraph::new(GraphId::new(431_900 + scale as u64));
        let label = db_string("BenchTextDoc").expect("bench label fits DB string cap");
        let property = db_string("body").expect("bench property fits DB string cap");
        let delete_count = (scale / 10).max(1);
        let mut delete_ids = Vec::with_capacity(delete_count);
        {
            let mut txn = shared.begin_write();
            let mut mutator = txn.mutator();
            for row in 0..scale {
                let node_id = mutator
                    .create_node(
                        LabelSet::single(label.clone()),
                        props(&property, text_value(&seed_text(row))),
                    )
                    .expect("bench node inserts");
                if delete_ids.len() < delete_count {
                    delete_ids.push(node_id);
                }
            }
            txn.commit().expect("bench fixture commits");
        }
        let graph = shared.read();
        shared
            .create_text_index(label.clone(), property.clone())
            .expect("bench text index registers");
        let indexed_graph = shared.read();
        Self {
            graph,
            indexed_graph,
            label,
            property,
            scale,
            delete_ids,
        }
    }

    fn delete_count(&self) -> usize {
        self.delete_ids.len()
    }

    fn live_after_delete(&self) -> usize {
        self.scale - self.delete_count()
    }
}

struct TextMixedFixture {
    shared: SharedGraph,
    label: DbString,
    property: DbString,
    query: String,
    update_ids: Vec<NodeId>,
    update_values: Vec<Value>,
}

impl TextMixedFixture {
    fn build(scale: usize) -> Self {
        let scale = scale.max(READS_PER_CYCLE).max(WRITES_PER_CYCLE);
        let shared = SharedGraph::new(GraphId::new(431_700 + scale as u64));
        let label = db_string("BenchTextDoc").expect("bench label fits DB string cap");
        let property = db_string("body").expect("bench property fits DB string cap");
        let mut update_ids = Vec::with_capacity(WRITES_PER_CYCLE);
        {
            let mut txn = shared.begin_write();
            let mut mutator = txn.mutator();
            for row in 0..scale {
                let node_id = mutator
                    .create_node(
                        LabelSet::single(label.clone()),
                        props(&property, text_value(&seed_text(row))),
                    )
                    .expect("bench node inserts");
                if update_ids.len() < WRITES_PER_CYCLE {
                    update_ids.push(node_id);
                }
            }
            txn.commit().expect("bench fixture commits");
        }
        shared
            .create_text_index(label.clone(), property.clone())
            .expect("bench text index registers");
        Self {
            shared,
            label,
            property,
            query: "gql current retrieval evidence".to_owned(),
            update_ids,
            update_values: (0..WRITES_PER_CYCLE)
                .map(|idx| text_value(&updated_text(scale, idx)))
                .collect(),
        }
    }

    fn run_cycle(self) -> usize {
        let mut read_idx = 0;
        let mut write_idx = 0;
        let mut observed = 0;
        for slot in 0..OPS_PER_CYCLE {
            if slot % 5 < 3 {
                observed += self.read_once(read_idx);
                read_idx += 1;
            } else {
                self.write_once(write_idx);
                write_idx += 1;
            }
        }
        observed
    }

    fn run_write_cycle(self) -> usize {
        for write_idx in 0..WRITES_PER_CYCLE {
            self.write_once(write_idx);
        }
        WRITES_PER_CYCLE
    }

    fn read_once(&self, _idx: usize) -> usize {
        self.shared
            .read()
            .text_index_for(&self.label, &self.property)
            .expect("registered bench text index exists")
            .search(&self.query, TEXT_TOP_K)
            .len()
    }

    fn write_once(&self, idx: usize) {
        let mut txn = self.shared.begin_write();
        {
            let mut mutator = txn.mutator();
            let diff = PropertyDiff::new(
                [(self.property.clone(), self.update_values[idx].clone())],
                [],
            )
            .expect("bench property diff is valid");
            mutator
                .update_node(
                    self.update_ids[idx],
                    LabelDiff::new([], []).expect("bench label diff is valid"),
                    diff,
                )
                .expect("bench text update succeeds");
        }
        txn.commit().expect("bench text update commit succeeds");
    }
}

fn delete_targets(shared: &SharedGraph, targets: &[NodeId]) {
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        for &target in targets {
            mutator
                .delete_node(target)
                .expect("bench text delete succeeds");
        }
    }
    txn.commit().expect("bench text delete commit succeeds");
}

fn deadline_checker() -> CancellationChecker<'static> {
    CancellationChecker::new(None, Some(Instant::now() + Duration::from_secs(3600)))
}

fn props(key: &DbString, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(key.clone(), value)]).expect("bench property map is valid")
}

fn seed_text(row: usize) -> String {
    let topic = TOPICS[row % TOPICS.len()];
    let neighbor = TOPICS[(row + 3) % TOPICS.len()];
    let state = STATES[row % STATES.len()];
    format!("{topic} {state} agent memory fact supports {neighbor} retrieval evidence")
}

fn updated_text(scale: usize, idx: usize) -> String {
    let topic = TOPICS[(scale + idx + 5) % TOPICS.len()];
    let neighbor = TOPICS[(scale + idx + 1) % TOPICS.len()];
    format!("{topic} verified bm25 maintenance rewrite supports {neighbor} current evidence")
}

fn text_value(text: &str) -> Value {
    Value::String(db_string(text).expect("bench text fits DB string cap"))
}

criterion_group! {
    name = text_search;
    config = common::criterion_config();
    targets = bench_exact_bm25, bench_indexed_bm25, bench_mixed_bm25, bench_text_rebuild, bench_hybrid_bm25_vector, text_json_boundary::bench
}
criterion_main!(text_search);
