#![allow(missing_docs)]
//! Criterion benches for write-path GQL and direct durable mutation flows.
//!
//! All arms run on a no-WAL in-memory `SharedGraph`
//! (`gql_write_state_in_memory`) so they isolate parse, plan, execute, and
//! in-memory commit CPU — the deltas GQLRT-05 / CORE-06 move. Durable commit
//! baselines belong to the `selene-db` facade benches (`durable_commit`,
//! `durable_checkpoint`), never to these CPU arms.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod common;

use std::{num::NonZeroUsize, sync::Arc};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use selene_core::{DbString, JsonValue, LabelDiff, PropertyDiff, Value, db_string};
use selene_gql::{EmptyProcedureRegistry, Session, SharedPlanCache, StatementOutput};
use selene_graph::{SharedGraph, TypedIndexKind};
use selene_testing::{BenchProfile, WriteCorpus};

const READS_PER_MIXED_CYCLE: usize = 60;
const WRITES_PER_MIXED_CYCLE: usize = 40;
const OPS_PER_MIXED_CYCLE: usize = READS_PER_MIXED_CYCLE + WRITES_PER_MIXED_CYCLE;
const MIXED_READ_SOURCE: &str = "MATCH (n:Person) FILTER n.bench_id = $id RETURN n.score AS score";
const MIXED_WRITE_SOURCE: &str = "MATCH (n:Person) FILTER n.bench_id = $id SET n.score = $score";
const JSON_MIXED_READ_SOURCE: &str = "MATCH (n:Person) FILTER n.bench_id = $id RETURN json_get_path_text(n.payload, 'memory', 'facts', 1, 'title') AS title, json_has_path(n.payload, 'memory', 'source') AS has_source";
const JSON_MIXED_WRITE_SOURCE: &str =
    "MATCH (n:Person) FILTER n.bench_id = $id SET n.payload = json_patch(n.payload, $patch)";

fn bench_write_e2e(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_e2e");
    for &scale in BenchProfile::from_env().scales() {
        bench_gql_per_iter_plan(&mut group, scale);
        bench_gql_fresh_preplanned(
            &mut group,
            "gql_insert_single_node_preplanned",
            scale,
            WriteCorpus::insert_single_node(),
        );
        bench_gql_insert_single_node_cached(&mut group, scale);
        bench_gql_insert_single_node_shared_cache(&mut group, scale);
        bench_gql_insert_single_node_cached_with_schema_churn(&mut group, scale);
        bench_gql_fresh_preplanned(
            &mut group,
            "gql_insert_node_with_edge_preplanned",
            scale,
            WriteCorpus::insert_node_with_edge(),
        );
        bench_gql_fresh_preplanned(
            &mut group,
            "gql_match_set_preplanned",
            scale,
            WriteCorpus::match_set(),
        );
        bench_gql_fresh_preplanned(
            &mut group,
            "gql_match_delete_preplanned",
            scale,
            WriteCorpus::match_delete(),
        );
        bench_gql_cached_mixed_read_write(&mut group, scale);
        bench_gql_cached_json_mixed_read_write(&mut group, scale);
        bench_gql_multi_statement(&mut group, scale);
        bench_explicit_txn_3_inserts_rust_api(&mut group, scale);
        bench_explicit_txn_3_inserts_rollback(&mut group, scale);
    }
    group.finish();
}

fn bench_gql_insert_single_node_cached(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    scale: usize,
) {
    group.throughput(Throughput::Elements(1));
    let state = common::gql_write_state_in_memory(scale);
    let registry = EmptyProcedureRegistry;
    let source = WriteCorpus::insert_single_node();
    let mut session =
        Session::new(&state.graph).with_plan_cache(NonZeroUsize::new(64).expect("nonzero"));
    execute_cached_source(&mut session, source, &registry);
    group.bench_function(
        BenchmarkId::new("gql_insert_single_node_cached", scale),
        |b| {
            b.iter(|| {
                let rows = execute_cached_source(&mut session, source, &registry);
                std::hint::black_box(rows)
            });
        },
    );
    std::hint::black_box(&state);
}

fn bench_gql_insert_single_node_shared_cache(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    scale: usize,
) {
    group.throughput(Throughput::Elements(1));
    let state = common::gql_write_state_in_memory(scale);
    let registry = EmptyProcedureRegistry;
    let source = WriteCorpus::insert_single_node();
    let cache = Arc::new(SharedPlanCache::new(
        NonZeroUsize::new(64).expect("nonzero"),
    ));
    let mut warmup = Session::new(&state.graph).with_shared_plan_cache(Arc::clone(&cache));
    execute_cached_source(&mut warmup, source, &registry);
    group.bench_function(
        BenchmarkId::new("gql_insert_single_node_shared_cache", scale),
        |b| {
            b.iter(|| {
                let mut session =
                    Session::new(&state.graph).with_shared_plan_cache(Arc::clone(&cache));
                let rows = execute_cached_source(&mut session, source, &registry);
                std::hint::black_box(rows)
            });
        },
    );
    std::hint::black_box(&state);
}

fn bench_gql_insert_single_node_cached_with_schema_churn(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    scale: usize,
) {
    group.throughput(Throughput::Elements(1));
    let state = common::gql_write_state_in_memory(scale);
    let registry = EmptyProcedureRegistry;
    let source = WriteCorpus::insert_single_node();
    let label = db_string("Person").expect("Person label fits DB string cap");
    let property = db_string("plan_cache_churn").expect("churn property fits DB string cap");
    let mut session =
        Session::new(&state.graph).with_plan_cache(NonZeroUsize::new(64).expect("nonzero"));
    execute_cached_source(&mut session, source, &registry);
    let mut iteration = 0_u64;
    let mut index_exists = false;
    group.bench_function(
        BenchmarkId::new("gql_insert_single_node_cached_with_schema_churn", scale),
        |b| {
            b.iter(|| {
                if iteration.is_multiple_of(10) {
                    if index_exists {
                        state
                            .graph
                            .drop_property_index(label.clone(), property.clone())
                            .expect("churn index drops");
                    } else {
                        state
                            .graph
                            .create_property_index(
                                label.clone(),
                                property.clone(),
                                TypedIndexKind::I64,
                            )
                            .expect("churn index creates");
                    }
                    index_exists = !index_exists;
                }
                iteration = iteration.saturating_add(1);
                let rows = execute_cached_source(&mut session, source, &registry);
                std::hint::black_box(rows)
            });
        },
    );
    std::hint::black_box(&state);
}

fn bench_gql_cached_mixed_read_write(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    scale: usize,
) {
    group.throughput(Throughput::Elements(OPS_PER_MIXED_CYCLE as u64));
    let state = common::gql_write_state_in_memory(scale);
    let mut fixture = GqlMixedFixture::new(&state.graph, scale);
    group.bench_function(
        BenchmarkId::new("gql_cached_point_read_set_r60w40", scale),
        |b| b.iter(|| std::hint::black_box(fixture.run_cycle())),
    );
    std::hint::black_box(&state);
}

fn bench_gql_cached_json_mixed_read_write(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    scale: usize,
) {
    group.throughput(Throughput::Elements(OPS_PER_MIXED_CYCLE as u64));
    let state = common::gql_write_state_in_memory(scale);
    seed_json_payloads(&state.graph);
    let mut fixture = GqlJsonMixedFixture::new(&state.graph, scale);
    group.bench_function(
        BenchmarkId::new("gql_cached_json_read_patch_r60w40", scale),
        |b| b.iter(|| std::hint::black_box(fixture.run_cycle())),
    );
    std::hint::black_box(&state);
}

struct GqlMixedFixture<'g> {
    session: Session<'g>,
    id_key: DbString,
    score_key: DbString,
    read_ids: Vec<i64>,
    write_ids: Vec<i64>,
}

impl<'g> GqlMixedFixture<'g> {
    fn new(graph: &'g selene_graph::SharedGraph, scale: usize) -> Self {
        let id_key = db_string("id").expect("parameter name fits DB string cap");
        let score_key = db_string("score").expect("parameter name fits DB string cap");
        let mut session =
            Session::new(graph).with_plan_cache(NonZeroUsize::new(8).expect("nonzero"));
        session.bind_parameter(id_key.clone(), Value::Int(0));
        session.bind_parameter(score_key.clone(), Value::Int(0));
        execute_cached_source(&mut session, MIXED_READ_SOURCE, &EmptyProcedureRegistry);
        execute_cached_source(&mut session, MIXED_WRITE_SOURCE, &EmptyProcedureRegistry);
        Self {
            session,
            id_key,
            score_key,
            read_ids: person_bench_ids(scale, READS_PER_MIXED_CYCLE),
            write_ids: person_bench_ids(scale, WRITES_PER_MIXED_CYCLE),
        }
    }

    fn run_cycle(&mut self) -> usize {
        let mut read_idx = 0;
        let mut write_idx = 0;
        let mut observed = 0;
        for slot in 0..OPS_PER_MIXED_CYCLE {
            if slot % 5 < 3 {
                observed += self.read_once(read_idx);
                read_idx += 1;
            } else {
                observed += self.write_once(write_idx);
                write_idx += 1;
            }
        }
        observed
    }

    fn read_once(&mut self, idx: usize) -> usize {
        self.session
            .bind_parameter(self.id_key.clone(), Value::Int(self.read_ids[idx]));
        execute_cached_source(
            &mut self.session,
            MIXED_READ_SOURCE,
            &EmptyProcedureRegistry,
        )
    }

    fn write_once(&mut self, idx: usize) -> usize {
        self.session
            .bind_parameter(self.id_key.clone(), Value::Int(self.write_ids[idx]));
        self.session.bind_parameter(
            self.score_key.clone(),
            Value::Int((idx + READS_PER_MIXED_CYCLE) as i64),
        );
        execute_cached_source(
            &mut self.session,
            MIXED_WRITE_SOURCE,
            &EmptyProcedureRegistry,
        )
    }
}

struct GqlJsonMixedFixture<'g> {
    session: Session<'g>,
    id_key: DbString,
    read_ids: Vec<i64>,
    write_ids: Vec<i64>,
}

impl<'g> GqlJsonMixedFixture<'g> {
    fn new(graph: &'g SharedGraph, scale: usize) -> Self {
        let id_key = db_string("id").expect("parameter name fits DB string cap");
        let patch_key = db_string("patch").expect("parameter name fits DB string cap");
        let mut session =
            Session::new(graph).with_plan_cache(NonZeroUsize::new(8).expect("nonzero"));
        session.bind_parameter(id_key.clone(), Value::Int(0));
        session.bind_parameter(patch_key.clone(), json_patch_value());
        execute_cached_source(
            &mut session,
            JSON_MIXED_READ_SOURCE,
            &EmptyProcedureRegistry,
        );
        execute_cached_source(
            &mut session,
            JSON_MIXED_WRITE_SOURCE,
            &EmptyProcedureRegistry,
        );
        Self {
            session,
            id_key,
            read_ids: person_bench_ids(scale, READS_PER_MIXED_CYCLE),
            write_ids: person_bench_ids(scale, WRITES_PER_MIXED_CYCLE),
        }
    }

    fn run_cycle(&mut self) -> usize {
        let mut read_idx = 0;
        let mut write_idx = 0;
        let mut observed = 0;
        for slot in 0..OPS_PER_MIXED_CYCLE {
            if slot % 5 < 3 {
                observed += self.read_once(read_idx);
                read_idx += 1;
            } else {
                observed += self.write_once(write_idx);
                write_idx += 1;
            }
        }
        observed
    }

    fn read_once(&mut self, idx: usize) -> usize {
        self.session
            .bind_parameter(self.id_key.clone(), Value::Int(self.read_ids[idx]));
        execute_cached_source(
            &mut self.session,
            JSON_MIXED_READ_SOURCE,
            &EmptyProcedureRegistry,
        )
    }

    fn write_once(&mut self, idx: usize) -> usize {
        self.session
            .bind_parameter(self.id_key.clone(), Value::Int(self.write_ids[idx]));
        execute_cached_source(
            &mut self.session,
            JSON_MIXED_WRITE_SOURCE,
            &EmptyProcedureRegistry,
        )
    }
}

fn person_bench_ids(scale: usize, count: usize) -> Vec<i64> {
    let person_count = scale.div_ceil(3).max(1);
    (0..count)
        .map(|idx| (3 * (idx % person_count)) as i64)
        .collect()
}

fn seed_json_payloads(graph: &SharedGraph) {
    let person_label = db_string("Person").expect("Person label fits DB string cap");
    let payload_key = db_string("payload").expect("payload key fits DB string cap");
    let node_ids = {
        let snapshot = graph.read();
        snapshot
            .node_candidates_with_label(&person_label)
            .map(|candidates| candidates.iter().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let payload = json_payload_value();
    let mut txn = graph.begin_write();
    {
        let mut mutator = txn.mutator();
        for node_id in node_ids {
            mutator
                .update_node(
                    node_id,
                    LabelDiff::new([], []).expect("empty label diff builds"),
                    PropertyDiff::new([(payload_key.clone(), payload.clone())], [])
                        .expect("payload property diff builds"),
                )
                .expect("payload seed update succeeds");
        }
    }
    txn.commit().expect("payload seed commit succeeds");
}

fn json_payload_value() -> Value {
    json_value(
        r#"{
            "memory":{
                "kind":"episodic",
                "current":false,
                "revision":0,
                "score":7,
                "facts":[
                    {"kind":"semantic","title":"old"},
                    {"kind":"episodic","title":"current"}
                ]
            },
            "tags":["agent","graph"]
        }"#,
    )
}

fn json_patch_value() -> Value {
    json_value(
        r#"[
            {"op":"replace","path":"/memory/current","value":true},
            {"op":"add","path":"/memory/source","value":"graph"},
            {"op":"replace","path":"/memory/revision","value":1}
        ]"#,
    )
}

fn json_value(source: &str) -> Value {
    Value::Json(JsonValue::parse_str(source).expect("write bench JSON value parses"))
}

fn execute_cached_source(
    session: &mut Session<'_>,
    source: &str,
    registry: &EmptyProcedureRegistry,
) -> usize {
    match session
        .execute_source(source, registry)
        .expect("cached write statement executes")
    {
        StatementOutput::Rows(table) => table.row_count(),
        StatementOutput::Written(outcome) => {
            let rows = outcome.rows.as_ref().map_or(0, |table| table.row_count());
            rows + outcome.changes.len()
        }
        StatementOutput::Empty => 0,
        _ => panic!("unexpected statement output"),
    }
}

fn bench_gql_per_iter_plan(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    scale: usize,
) {
    group.throughput(Throughput::Elements(1));
    group.bench_function(
        BenchmarkId::new("gql_insert_single_node_per_iter_plan", scale),
        |b| {
            b.iter_batched(
                || common::gql_write_state_in_memory(scale),
                |state| {
                    let plan = common::plan_write(WriteCorpus::insert_single_node());
                    let mut session = Session::new(&state.graph);
                    let rows = common::execute_preplanned(&plan, &mut session);
                    drop(session);
                    std::hint::black_box((state, rows))
                },
                BatchSize::LargeInput,
            );
        },
    );
}

fn bench_gql_fresh_preplanned(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    name: &'static str,
    scale: usize,
    source: &str,
) {
    group.throughput(Throughput::Elements(1));
    let plan = common::plan_write(source);
    group.bench_function(BenchmarkId::new(name, scale), |b| {
        b.iter_batched(
            || common::gql_write_state_in_memory(scale),
            |state| {
                let mut session = Session::new(&state.graph);
                let rows = common::execute_preplanned(&plan, &mut session);
                drop(session);
                std::hint::black_box((state, rows))
            },
            BatchSize::LargeInput,
        );
    });
}

fn bench_gql_multi_statement(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    scale: usize,
) {
    group.throughput(Throughput::Elements(3));
    let plans = WriteCorpus::multi_statement_txn()
        .iter()
        .map(|source| common::plan_write(source))
        .collect::<Vec<_>>();
    group.bench_function(
        BenchmarkId::new("gql_multi_statement_txn_preplanned", scale),
        |b| {
            b.iter_batched(
                || common::gql_write_state_in_memory(scale),
                |state| {
                    let mut session = Session::new(&state.graph);
                    let rows = plans
                        .iter()
                        .map(|plan| common::execute_preplanned(plan, &mut session))
                        .sum::<usize>();
                    drop(session);
                    std::hint::black_box((state, rows))
                },
                BatchSize::LargeInput,
            );
        },
    );
}

fn bench_explicit_txn_3_inserts_rust_api(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    scale: usize,
) {
    group.throughput(Throughput::Elements(3));
    let plans = WriteCorpus::multi_statement_txn()[1..4]
        .iter()
        .map(|source| common::plan_write(source))
        .collect::<Vec<_>>();
    group.bench_function(
        BenchmarkId::new("explicit_txn_3_inserts_rust_api", scale),
        |b| {
            b.iter_batched(
                || common::gql_write_state_in_memory(scale),
                |state| {
                    let mut session = Session::new(&state.graph);
                    session.start_transaction().expect("start succeeds");
                    let rows = plans
                        .iter()
                        .map(|plan| common::execute_preplanned(plan, &mut session))
                        .sum::<usize>();
                    let outcome = session.commit_transaction().expect("commit succeeds");
                    drop(session);
                    std::hint::black_box((state, rows + outcome.changes.len()))
                },
                BatchSize::LargeInput,
            );
        },
    );
}

fn bench_explicit_txn_3_inserts_rollback(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    scale: usize,
) {
    group.throughput(Throughput::Elements(3));
    let plans = WriteCorpus::multi_statement_txn()[1..4]
        .iter()
        .map(|source| common::plan_write(source))
        .collect::<Vec<_>>();
    group.bench_function(
        BenchmarkId::new("explicit_txn_3_inserts_rollback", scale),
        |b| {
            b.iter_batched(
                || common::gql_write_state_in_memory(scale),
                |state| {
                    let mut session = Session::new(&state.graph);
                    session.start_transaction().expect("start succeeds");
                    let rows = plans
                        .iter()
                        .map(|plan| common::execute_preplanned(plan, &mut session))
                        .sum::<usize>();
                    let outcome = session.rollback_transaction().expect("rollback succeeds");
                    drop(session);
                    std::hint::black_box((state, rows + outcome.discarded_changes))
                },
                BatchSize::LargeInput,
            );
        },
    );
}

criterion_group! {
    name = write_e2e;
    config = common::criterion_config();
    targets = bench_write_e2e
}
criterion_main!(write_e2e);
