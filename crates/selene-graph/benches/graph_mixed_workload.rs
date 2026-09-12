#![allow(missing_docs)]
//! Scalar graph read/write mixed workload benchmark.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod common;

use std::sync::Arc;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use selene_core::{
    DbString, EdgeId, GraphId, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value,
    db_string,
};
use selene_graph::{
    CandidateStateSpec, IndexProvider, MaintainedCandidateStateProvider, SharedGraph,
};
use selene_testing::BenchFixture;

const READS_PER_CYCLE: usize = 60;
const WRITES_PER_CYCLE: usize = 40;
const OPS_PER_CYCLE: usize = READS_PER_CYCLE + WRITES_PER_CYCLE;

fn bench_scalar_mixed_workload(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_mixed_workload");
    for fixture in common::fixtures() {
        group.throughput(Throughput::Elements(OPS_PER_CYCLE as u64));
        group.bench_with_input(
            BenchmarkId::new("point_read_update_r60w40", fixture.scale()),
            &fixture,
            |b, fixture| {
                b.iter_batched(
                    || ScalarMixedFixture::build(fixture),
                    |fixture| std::hint::black_box(fixture.run_cycle()),
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new("point_read_indexed_update_r60w40", fixture.scale()),
            &fixture,
            |b, fixture| {
                b.iter_batched(
                    || ScalarMixedFixture::build_indexed_property(fixture),
                    |fixture| std::hint::black_box(fixture.run_cycle()),
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new("candidate_state_edge_update_r60w40", fixture.scale()),
            &fixture,
            |b, fixture| {
                let scale = fixture.scale();
                b.iter_batched(
                    || CandidateStateMixedFixture::build(scale),
                    |fixture| std::hint::black_box(fixture.run_cycle()),
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new(
                "candidate_state_metadata_edge_update_r60w40",
                fixture.scale(),
            ),
            &fixture,
            |b, fixture| {
                let scale = fixture.scale();
                b.iter_batched(
                    || CandidateStateMixedFixture::build(scale),
                    |fixture| std::hint::black_box(fixture.run_metadata_cycle()),
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

struct CandidateStateMixedFixture {
    shared: SharedGraph,
    set_name: DbString,
    edge_label: DbString,
    activate_edges: Vec<EdgeId>,
    deactivate_sources: Vec<NodeId>,
    deactivate_targets: Vec<NodeId>,
}

impl CandidateStateMixedFixture {
    fn build(scale: usize) -> Self {
        let scale = scale.max(WRITES_PER_CYCLE * 2);
        let active_count = scale / 2;
        let stale_count = scale - active_count;
        let set_name = db_string("current").expect("bench set name is valid");
        let doc_label = db_string("MemoryFact").expect("bench label is valid");
        let edge_label = db_string("SUPERSEDED_BY").expect("bench edge label is valid");
        let spec = CandidateStateSpec::new(set_name.clone())
            .require_label(doc_label.clone())
            .exclude_outgoing(edge_label.clone());
        let provider = Arc::new(
            MaintainedCandidateStateProvider::new([spec]).expect("bench provider is valid"),
        );
        let shared = SharedGraph::builder(GraphId::new(31_000 + scale as u64))
            .with_provider(provider as Arc<dyn IndexProvider>)
            .build()
            .expect("bench candidate-state graph builds");
        let mut active = Vec::with_capacity(active_count);
        let mut stale_edges = Vec::with_capacity(stale_count);
        {
            let mut txn = shared.begin_write();
            let mut mutator = txn.mutator();
            for _ in 0..active_count {
                active.push(
                    mutator
                        .create_node(LabelSet::single(doc_label.clone()), PropertyMap::new())
                        .expect("bench active node insert succeeds"),
                );
            }
            for idx in 0..stale_count {
                let stale = mutator
                    .create_node(LabelSet::single(doc_label.clone()), PropertyMap::new())
                    .expect("bench stale node insert succeeds");
                stale_edges.push(
                    mutator
                        .create_edge(
                            edge_label.clone(),
                            stale,
                            active[idx % active.len()],
                            PropertyMap::new(),
                        )
                        .expect("bench stale edge insert succeeds"),
                );
            }
            txn.commit()
                .expect("bench candidate-state fixture commit succeeds");
        }
        let toggle_count = WRITES_PER_CYCLE / 2;
        Self {
            shared,
            set_name,
            edge_label,
            activate_edges: stale_edges.into_iter().take(toggle_count).collect(),
            deactivate_sources: active.iter().copied().take(toggle_count).collect(),
            deactivate_targets: active
                .iter()
                .copied()
                .skip(toggle_count)
                .take(toggle_count)
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

    fn run_metadata_cycle(self) -> usize {
        let mut read_idx = 0;
        let mut write_idx = 0;
        let mut observed = 0;
        for slot in 0..OPS_PER_CYCLE {
            if slot % 5 < 3 {
                observed += self.read_metadata_once(read_idx);
                read_idx += 1;
            } else {
                self.write_once(write_idx);
                write_idx += 1;
            }
        }
        observed
    }

    fn read_once(&self, _idx: usize) -> usize {
        self.shared
            .vector_candidate_set(&self.set_name)
            .expect("bench candidate-state provider is current")
            .expect("bench candidate-state set exists")
            .len()
    }

    fn read_metadata_once(&self, _idx: usize) -> usize {
        self.shared
            .vector_candidate_state_infos()
            .expect("bench candidate-state provider metadata is current")
            .into_iter()
            .find(|info| info.name == self.set_name)
            .expect("bench candidate-state metadata exists")
            .candidate_count
    }

    fn write_once(&self, idx: usize) {
        let mut txn = self.shared.begin_write();
        {
            let mut mutator = txn.mutator();
            if idx < self.activate_edges.len() {
                mutator
                    .delete_edge(self.activate_edges[idx])
                    .expect("bench candidate-state edge delete succeeds");
            } else {
                let create_idx = idx - self.activate_edges.len();
                mutator
                    .create_edge(
                        self.edge_label.clone(),
                        self.deactivate_sources[create_idx],
                        self.deactivate_targets[create_idx],
                        PropertyMap::new(),
                    )
                    .expect("bench candidate-state edge create succeeds");
            }
        }
        txn.commit()
            .expect("bench candidate-state update commit succeeds");
    }
}

struct ScalarMixedFixture {
    shared: SharedGraph,
    update_key: selene_core::DbString,
    update_base: i64,
    read_ids: Vec<NodeId>,
    update_ids: Vec<NodeId>,
}

impl ScalarMixedFixture {
    fn build(fixture: &BenchFixture) -> Self {
        Self::from_shared(SharedGraph::from_graph(fixture.graph().clone()), fixture)
    }

    fn build_indexed_property(fixture: &BenchFixture) -> Self {
        let shared = SharedGraph::from_graph(fixture.graph().clone());
        let node_count = fixture.graph().node_count().max(1);
        Self {
            shared,
            update_key: fixture.age_key(),
            update_base: 10_000,
            read_ids: point_node_ids(node_count),
            update_ids: person_node_ids(node_count),
        }
    }

    fn from_shared(shared: SharedGraph, fixture: &BenchFixture) -> Self {
        let node_count = fixture.graph().node_count().max(1);
        Self {
            shared,
            update_key: fixture.score_key(),
            update_base: READS_PER_CYCLE as i64,
            read_ids: point_node_ids(node_count),
            update_ids: point_update_node_ids(node_count),
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

    fn read_once(&self, idx: usize) -> usize {
        self.shared
            .read()
            .node_properties(self.read_ids[idx])
            .is_some() as usize
    }

    fn write_once(&self, idx: usize) {
        let mut txn = self.shared.begin_write();
        {
            let mut mutator = txn.mutator();
            let diff = PropertyDiff::new(
                [(
                    self.update_key.clone(),
                    Value::Int(self.update_base + idx as i64),
                )],
                [],
            )
            .expect("bench property diff is valid");
            mutator
                .update_node(
                    self.update_ids[idx],
                    LabelDiff::new([], []).expect("bench label diff is valid"),
                    diff,
                )
                .expect("bench scalar update succeeds");
        }
        txn.commit().expect("bench scalar commit succeeds");
    }
}

fn point_node_ids(node_count: usize) -> Vec<NodeId> {
    (0..READS_PER_CYCLE)
        .map(|idx| NodeId::new((idx % node_count) as u64 + 1))
        .collect()
}

fn point_update_node_ids(node_count: usize) -> Vec<NodeId> {
    (0..WRITES_PER_CYCLE)
        .map(|idx| NodeId::new((idx % node_count) as u64 + 1))
        .collect()
}

fn person_node_ids(node_count: usize) -> Vec<NodeId> {
    let person_count = node_count.div_ceil(3).max(1);
    (0..WRITES_PER_CYCLE)
        .map(|idx| NodeId::new((3 * (idx % person_count)) as u64 + 1))
        .collect()
}

criterion_group! {
    name = graph_mixed_workload;
    config = common::criterion_config();
    targets = bench_scalar_mixed_workload
}
criterion_main!(graph_mixed_workload);
