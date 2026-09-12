//! Active-hint provider fanout benchmark rows.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use criterion::{BatchSize, BenchmarkId, Throughput};
use selene_core::{Change, DbString, EdgeId, NodeId, PropertyMap, db_string};
use selene_graph::{IndexProvider, ProviderError, ProviderTag, SharedGraph};
use selene_testing::BenchFixture;

const ACTIVE_HINT_BATCH: usize = 40;

pub(super) fn bench_active_hint_edges(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    fixture: &BenchFixture,
) {
    let recent = active_hint_recent_label();
    let dependency = active_hint_dependency_label();
    for mode in [ActiveHintMode::Recent, ActiveHintMode::Dependency] {
        bench_active_hint_edge_create(group, fixture, mode, &recent, &dependency);
        bench_active_hint_edge_delete(group, fixture, mode, &recent, &dependency);
    }
}

fn bench_active_hint_edge_create(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    fixture: &BenchFixture,
    mode: ActiveHintMode,
    recent: &DbString,
    dependency: &DbString,
) {
    group.throughput(Throughput::Elements(ACTIVE_HINT_BATCH as u64));
    group.bench_function(
        BenchmarkId::from_parameter(mode.name("active_hint", "edge_create_k40")),
        |b| {
            b.iter_batched(
                || active_hint_shared(fixture, recent, dependency),
                |(shared, provider)| {
                    let changes =
                        commit_active_hint_edges(&shared, fixture, mode, recent, dependency).0;
                    std::hint::black_box((shared, changes, provider.total_links()))
                },
                BatchSize::SmallInput,
            );
        },
    );
}

fn bench_active_hint_edge_delete(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    fixture: &BenchFixture,
    mode: ActiveHintMode,
    recent: &DbString,
    dependency: &DbString,
) {
    group.throughput(Throughput::Elements(ACTIVE_HINT_BATCH as u64));
    group.bench_function(
        BenchmarkId::from_parameter(mode.name("active_hint", "edge_delete_k40")),
        |b| {
            b.iter_batched(
                || {
                    let (shared, provider) = active_hint_shared(fixture, recent, dependency);
                    let seeded =
                        commit_active_hint_edges(&shared, fixture, mode, recent, dependency).1;
                    (shared, provider, seeded)
                },
                |(shared, provider, seeded)| {
                    let changes = delete_edges(&shared, seeded);
                    std::hint::black_box((shared, changes, provider.total_links()))
                },
                BatchSize::SmallInput,
            );
        },
    );
}

fn active_hint_shared(
    fixture: &BenchFixture,
    recent: &DbString,
    dependency: &DbString,
) -> (SharedGraph, Arc<ActiveHintProvider>) {
    let provider = Arc::new(ActiveHintProvider::new(recent.clone(), dependency.clone()));
    let shared = SharedGraph::from_graph_with_providers(
        fixture.graph().clone(),
        vec![provider.clone() as Arc<dyn IndexProvider>],
    )
    .expect("active-hint provider fixture builds");
    (shared, provider)
}

fn commit_active_hint_edges(
    shared: &SharedGraph,
    fixture: &BenchFixture,
    mode: ActiveHintMode,
    recent: &DbString,
    dependency: &DbString,
) -> (usize, Vec<EdgeId>) {
    let mut txn = shared.begin_write();
    let mut edges = Vec::with_capacity(ACTIVE_HINT_BATCH);
    {
        let mut mutator = txn.mutator();
        for idx in 0..ACTIVE_HINT_BATCH {
            let (label, source, target) = mode.edge(recent, dependency, fixture, idx);
            edges.push(
                mutator
                    .create_edge(label, source, target, PropertyMap::new())
                    .expect("active-hint edge create succeeds"),
            );
        }
    }
    let changes = txn
        .commit()
        .expect("active-hint commit succeeds")
        .changes
        .len();
    (changes, edges)
}

fn delete_edges(shared: &SharedGraph, edges: Vec<EdgeId>) -> usize {
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        for edge_id in edges {
            mutator
                .delete_edge(edge_id)
                .expect("active-hint edge delete succeeds");
        }
    }
    txn.commit()
        .expect("edge delete commit succeeds")
        .changes
        .len()
}

fn active_hint_recent_label() -> DbString {
    db_string("ACTIVE_HINT_RECENT_IN").expect("bench recent edge label fits DB string cap")
}

fn active_hint_dependency_label() -> DbString {
    db_string("ACTIVE_HINT_DEPENDS_ON").expect("bench dependency edge label fits DB string cap")
}

#[derive(Clone, Copy)]
enum ActiveHintMode {
    Recent,
    Dependency,
}

impl ActiveHintMode {
    fn name(self, prefix: &'static str, suffix: &'static str) -> String {
        match self {
            Self::Recent => format!("{prefix}_recent_{suffix}"),
            Self::Dependency => format!("{prefix}_dependency_{suffix}"),
        }
    }

    fn edge(
        self,
        recent: &DbString,
        dependency: &DbString,
        fixture: &BenchFixture,
        idx: usize,
    ) -> (DbString, NodeId, NodeId) {
        match self {
            Self::Recent => (
                recent.clone(),
                NodeId::new((idx + 1) as u64),
                NodeId::new((fixture.scale() - (idx % 4)) as u64),
            ),
            Self::Dependency => (
                dependency.clone(),
                NodeId::new(1),
                NodeId::new((fixture.scale() - idx) as u64),
            ),
        }
    }
}

struct ActiveHintProvider {
    recent_label: DbString,
    dependency_label: DbString,
    state: Mutex<ActiveHintState>,
}

#[derive(Default)]
struct ActiveHintState {
    recent_by_window: HashMap<NodeId, HashSet<NodeId>>,
    dependency_by_anchor: HashMap<NodeId, HashSet<NodeId>>,
    edges: HashMap<EdgeId, ActiveHintEdge>,
}

enum ActiveHintEdge {
    Recent { source: NodeId, window: NodeId },
    Dependency { source: NodeId, target: NodeId },
}

impl ActiveHintProvider {
    fn new(recent_label: DbString, dependency_label: DbString) -> Self {
        Self {
            recent_label,
            dependency_label,
            state: Mutex::new(ActiveHintState::default()),
        }
    }

    fn total_links(&self) -> usize {
        let state = self.state.lock().expect("active-hint mutex poisoned");
        state
            .recent_by_window
            .values()
            .map(HashSet::len)
            .sum::<usize>()
            + state
                .dependency_by_anchor
                .values()
                .map(HashSet::len)
                .sum::<usize>()
    }
}

impl IndexProvider for ActiveHintProvider {
    fn provider_tag(&self) -> ProviderTag {
        ProviderTag(*b"AHNT")
    }

    fn on_change(&self, change: &Change) -> Result<(), ProviderError> {
        let mut state = self.state.lock().expect("active-hint mutex poisoned");
        match change {
            Change::EdgeCreated {
                id,
                label,
                source,
                target,
                ..
            } if label == &self.recent_label => {
                state
                    .recent_by_window
                    .entry(*target)
                    .or_default()
                    .insert(*source);
                state.edges.insert(
                    *id,
                    ActiveHintEdge::Recent {
                        source: *source,
                        window: *target,
                    },
                );
            }
            Change::EdgeCreated {
                id,
                label,
                source,
                target,
                ..
            } if label == &self.dependency_label => {
                state
                    .dependency_by_anchor
                    .entry(*source)
                    .or_default()
                    .insert(*target);
                state.edges.insert(
                    *id,
                    ActiveHintEdge::Dependency {
                        source: *source,
                        target: *target,
                    },
                );
            }
            Change::EdgeDeleted { id } => match state.edges.remove(id) {
                Some(ActiveHintEdge::Recent { source, window }) => {
                    if let Some(members) = state.recent_by_window.get_mut(&window) {
                        members.remove(&source);
                        if members.is_empty() {
                            state.recent_by_window.remove(&window);
                        }
                    }
                }
                Some(ActiveHintEdge::Dependency { source, target }) => {
                    if let Some(targets) = state.dependency_by_anchor.get_mut(&source) {
                        targets.remove(&target);
                        if targets.is_empty() {
                            state.dependency_by_anchor.remove(&source);
                        }
                    }
                }
                None => {}
            },
            _ => {}
        }
        Ok(())
    }
}
