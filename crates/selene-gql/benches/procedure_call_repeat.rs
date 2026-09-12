#![allow(missing_docs)]
//! Criterion benches for repeated procedure-call planning.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[path = "procedure_call_repeat/call_pipeline.rs"]
mod call_pipeline;
mod common;
#[path = "procedure_call_repeat/native_calls.rs"]
mod native_calls;
#[path = "procedure_call_repeat/rank_fusion.rs"]
mod rank_fusion;
#[path = "procedure_call_repeat/vector_ann_expanded.rs"]
mod vector_ann_expanded;
#[path = "procedure_call_repeat/vector_candidate_state.rs"]
mod vector_candidate_state;
#[path = "procedure_call_repeat/vector_expanded.rs"]
mod vector_expanded;
#[path = "procedure_call_repeat/vector_neighbors.rs"]
mod vector_neighbors;
#[path = "procedure_call_repeat/vector_omlx_query_roots.rs"]
mod vector_omlx_query_roots;

use std::{num::NonZeroUsize, sync::Arc};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap, Value, VectorValue};
use selene_gql::{
    BuiltinProcedureRegistry, CallPlanCache, GqlType, ProcedureContext, ProcedureError,
    ProcedureHandle, ProcedureMetadata, ProcedureMutability, ProcedureOutputColumn,
    ProcedureOutputSchema, ProcedureRegistry, ProcedureResult, ProcedureSignature, ProcedureTier,
    Session, StatementOutput,
};
use selene_graph::{SharedGraph, VectorIndexKind};

const SOURCE: &str = "CALL bench.repeat() YIELD n";
const FEATURE_STATUS_SOURCE: &str = "CALL selene.feature_status() YIELD feature_id, status, rationale, feature_name, surface, profile_relation, claim_state, evidence_status, evidence_count, profile_hash";
const REPEATS: usize = 100;
const VECTOR_SOURCE: &str =
    "CALL selene.vector_search_nodes('VectorDoc', 'embedding', $query, 10) YIELD node_id, distance";
const VECTOR_BATCH_SOURCE: &str = "CALL selene.vector_search_nodes_batch('VectorDoc', 'embedding', $queries, 10, 'squared_euclidean') YIELD query_index, node_id, distance";
const VECTOR_SCORE_SOURCE: &str = "CALL selene.vector_score_nodes('embedding', $query, $nodes, 10, 'squared_euclidean') YIELD node_id, distance";
const VECTOR_SCORE_BATCH_SOURCE: &str = "CALL selene.vector_score_nodes_batch('embedding', $queries, $nodes, 10, 'squared_euclidean') YIELD query_index, node_id, distance";
const VECTOR_ANN_SOURCE: &str = "CALL selene.vector_search_nodes_ann('VectorDoc', 'embedding', $query, 10, 'squared_euclidean', 64) YIELD node_id, distance";
const VECTOR_ANN_BATCH_SOURCE: &str = "CALL selene.vector_search_nodes_ann_batch('VectorDoc', 'embedding', $queries, 10, 'squared_euclidean', 64) YIELD query_index, node_id, distance";
const VECTOR_SCALE: usize = 1_000;
const VECTOR_DIMENSION: usize = 128;
const VECTOR_BATCH_QUERIES: usize = 8;
const VECTOR_SCORE_CANDIDATES: usize = 64;

struct RepeatRegistry {
    name: Box<[DbString]>,
    metadata: ProcedureMetadata,
}

impl RepeatRegistry {
    fn new() -> Self {
        let name = Box::from([db_string("bench"), db_string("repeat")]);
        Self {
            name,
            metadata: ProcedureMetadata::new(
                ProcedureHandle::new(1),
                ProcedureSignature::new(Vec::new()),
                ProcedureOutputSchema {
                    columns: vec![ProcedureOutputColumn::new(db_string("n"), GqlType::Integer)],
                },
                ProcedureTier::Graph,
                ProcedureMutability::Read,
            ),
        }
    }
}

impl ProcedureRegistry for RepeatRegistry {
    fn lookup(&self, name: &[DbString]) -> Option<ProcedureMetadata> {
        (name == self.name.as_ref()).then(|| self.metadata.clone())
    }

    fn execute(
        &self,
        _handle: ProcedureHandle,
        _args: &[Value],
        _ctx: &mut ProcedureContext<'_, '_>,
    ) -> Result<ProcedureResult, ProcedureError> {
        Ok(ProcedureResult {
            rows: vec![vec![Value::Int(1)]],
        })
    }
}

fn bench_procedure_call_repeat(c: &mut Criterion) {
    let registry = RepeatRegistry::new();
    let graph = SharedGraph::new(GraphId::new(71_001));
    let cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(256).expect("nonzero")));
    Session::new(&graph)
        .with_call_plan_cache(Arc::clone(&cache))
        .execute_source(SOURCE, &registry)
        .expect("warmup call executes");

    let mut group = c.benchmark_group("procedure_call_repeat");
    group.throughput(Throughput::Elements(REPEATS as u64));
    group.bench_function("no_cache", |b| {
        b.iter(|| std::hint::black_box(execute_repeated(&graph, &registry, None)));
    });
    group.bench_function("shared_cache", |b| {
        b.iter(|| {
            std::hint::black_box(execute_repeated(
                &graph,
                &registry,
                Some(Arc::clone(&cache)),
            ));
        });
    });
    group.finish();
}

fn bench_feature_status_procedure(c: &mut Criterion) {
    let registry = BuiltinProcedureRegistry::new();
    let graph = SharedGraph::new(GraphId::new(71_002));
    let cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(8).expect("nonzero")));
    Session::new(&graph)
        .with_call_plan_cache(Arc::clone(&cache))
        .execute_source(FEATURE_STATUS_SOURCE, &registry)
        .expect("warmup feature-status call executes");

    let mut group = c.benchmark_group("procedure_feature_status");
    group.throughput(Throughput::Elements(
        selene_profile::capabilities().len() as u64
    ));
    group.bench_function("shared_cache_generated_capabilities", |b| {
        b.iter(|| {
            let output = Session::new(&graph)
                .with_call_plan_cache(Arc::clone(&cache))
                .execute_source(FEATURE_STATUS_SOURCE, &registry)
                .expect("feature-status call executes");
            std::hint::black_box(output);
        });
    });
    group.finish();
}

fn bench_vector_search_procedure(c: &mut Criterion) {
    let registry = BuiltinProcedureRegistry::new();
    let graph = vector_graph(VECTOR_SCALE, VECTOR_DIMENSION);
    let indexed_graph = vector_graph_indexed(VECTOR_SCALE, VECTOR_DIMENSION);
    let hnsw_graph = vector_graph_hnsw_indexed(VECTOR_SCALE, VECTOR_DIMENSION);
    let cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(256).expect("nonzero")));
    let indexed_cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(256).expect("nonzero")));
    let exact_batch_cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(256).expect("nonzero")));
    let score_cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(256).expect("nonzero")));
    let score_batch_cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(256).expect("nonzero")));
    let hnsw_cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(256).expect("nonzero")));
    let batch_cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(256).expect("nonzero")));
    warm_vector_cache(&graph, &registry, Arc::clone(&cache), VECTOR_SOURCE);
    warm_vector_cache(
        &indexed_graph,
        &registry,
        Arc::clone(&indexed_cache),
        VECTOR_SOURCE,
    );
    warm_vector_batch_cache(
        &indexed_graph,
        &registry,
        Arc::clone(&exact_batch_cache),
        VECTOR_BATCH_SOURCE,
    );
    warm_vector_score_cache(
        &graph,
        &registry,
        Arc::clone(&score_cache),
        VECTOR_SCORE_SOURCE,
    );
    warm_vector_score_batch_cache(
        &graph,
        &registry,
        Arc::clone(&score_batch_cache),
        VECTOR_SCORE_BATCH_SOURCE,
    );
    warm_vector_cache(
        &hnsw_graph,
        &registry,
        Arc::clone(&hnsw_cache),
        VECTOR_ANN_SOURCE,
    );
    warm_vector_batch_cache(
        &hnsw_graph,
        &registry,
        Arc::clone(&batch_cache),
        VECTOR_ANN_BATCH_SOURCE,
    );

    let mut group = c.benchmark_group("procedure_vector_search");
    group.throughput(Throughput::Elements(VECTOR_SCALE as u64));
    group.bench_function("shared_cache_squared_euclidean_dim128_k10_1000", |b| {
        b.iter(|| {
            std::hint::black_box(execute_vector_search(
                &graph,
                &registry,
                Some(Arc::clone(&cache)),
                VECTOR_SOURCE,
            ));
        });
    });
    group.bench_function("shared_cache_flat_index_dim128_k10_1000", |b| {
        b.iter(|| {
            std::hint::black_box(execute_vector_search(
                &indexed_graph,
                &registry,
                Some(Arc::clone(&indexed_cache)),
                VECTOR_SOURCE,
            ));
        });
    });
    group.bench_function("shared_cache_flat_index_repeated_8x_dim128_k10_1000", |b| {
        b.iter(|| {
            std::hint::black_box(execute_vector_exact_repeated_batch(
                &indexed_graph,
                &registry,
                Some(Arc::clone(&indexed_cache)),
            ));
        });
    });
    group.bench_function("shared_cache_flat_index_batch_8x_dim128_k10_1000", |b| {
        b.iter(|| {
            std::hint::black_box(execute_vector_exact_batch(
                &indexed_graph,
                &registry,
                Some(Arc::clone(&exact_batch_cache)),
            ));
        });
    });
    group.bench_function("shared_cache_score_nodes_64_dim128_k10_1000", |b| {
        b.iter(|| {
            std::hint::black_box(execute_vector_score(
                &graph,
                &registry,
                Some(Arc::clone(&score_cache)),
            ));
        });
    });
    group.bench_function(
        "shared_cache_score_nodes_repeated_8x64_dim128_k10_1000",
        |b| {
            b.iter(|| {
                std::hint::black_box(execute_vector_score_repeated_batch(
                    &graph,
                    &registry,
                    Some(Arc::clone(&score_cache)),
                ));
            });
        },
    );
    group.bench_function("shared_cache_score_nodes_batch_8x64_dim128_k10_1000", |b| {
        b.iter(|| {
            std::hint::black_box(execute_vector_score_batch(
                &graph,
                &registry,
                Some(Arc::clone(&score_batch_cache)),
            ));
        });
    });
    group.bench_function("shared_cache_hnsw_ann_dim128_k10_1000", |b| {
        b.iter(|| {
            std::hint::black_box(execute_vector_search(
                &hnsw_graph,
                &registry,
                Some(Arc::clone(&hnsw_cache)),
                VECTOR_ANN_SOURCE,
            ));
        });
    });
    group.bench_function("shared_cache_hnsw_ann_repeated_8x_dim128_k10_1000", |b| {
        b.iter(|| {
            std::hint::black_box(execute_vector_ann_repeated_batch(
                &hnsw_graph,
                &registry,
                Some(Arc::clone(&hnsw_cache)),
            ));
        });
    });
    group.bench_function("shared_cache_hnsw_ann_batch_8x_dim128_k10_1000", |b| {
        b.iter(|| {
            std::hint::black_box(execute_vector_ann_batch(
                &hnsw_graph,
                &registry,
                Some(Arc::clone(&batch_cache)),
            ));
        });
    });
    group.finish();
}

fn execute_repeated(
    graph: &SharedGraph,
    registry: &dyn ProcedureRegistry,
    cache: Option<Arc<CallPlanCache>>,
) -> usize {
    let mut rows = 0;
    for _ in 0..REPEATS {
        let mut session = Session::new(graph);
        if let Some(cache) = cache.as_ref() {
            session = session.with_call_plan_cache(Arc::clone(cache));
        }
        match session
            .execute_source(SOURCE, registry)
            .expect("procedure call executes")
        {
            StatementOutput::Rows(table) => rows += table.row_count(),
            other => panic!("unexpected output: {other:?}"),
        }
    }
    rows
}

fn warm_vector_cache(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Arc<CallPlanCache>,
    source: &str,
) {
    let mut session = Session::new(graph).with_call_plan_cache(cache);
    session.bind_parameter(
        db_string("query"),
        Value::Vector(vector_value(0, VECTOR_DIMENSION)),
    );
    session
        .execute_source(source, registry)
        .expect("warmup vector search executes");
}

fn warm_vector_batch_cache(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Arc<CallPlanCache>,
    source: &str,
) {
    let mut session = Session::new(graph).with_call_plan_cache(cache);
    session.bind_parameter(db_string("queries"), vector_query_batch());
    session
        .execute_source(source, registry)
        .expect("warmup batched vector search executes");
}

fn warm_vector_score_cache(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Arc<CallPlanCache>,
    source: &str,
) {
    let mut session = Session::new(graph).with_call_plan_cache(cache);
    bind_vector_score_inputs(&mut session);
    session
        .execute_source(source, registry)
        .expect("warmup vector candidate scoring executes");
}

fn warm_vector_score_batch_cache(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Arc<CallPlanCache>,
    source: &str,
) {
    let mut session = Session::new(graph).with_call_plan_cache(cache);
    bind_vector_score_batch_inputs(&mut session);
    session
        .execute_source(source, registry)
        .expect("warmup batched vector candidate scoring executes");
}

fn execute_vector_search(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Option<Arc<CallPlanCache>>,
    source: &str,
) -> usize {
    let mut session = Session::new(graph);
    if let Some(cache) = cache {
        session = session.with_call_plan_cache(cache);
    }
    session.bind_parameter(
        db_string("query"),
        Value::Vector(vector_value(0, VECTOR_DIMENSION)),
    );
    match session
        .execute_source(source, registry)
        .expect("vector search procedure executes")
    {
        StatementOutput::Rows(table) => table.row_count(),
        other => panic!("unexpected output: {other:?}"),
    }
}

fn execute_vector_score(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Option<Arc<CallPlanCache>>,
) -> usize {
    let mut session = Session::new(graph);
    if let Some(cache) = cache {
        session = session.with_call_plan_cache(cache);
    }
    bind_vector_score_inputs(&mut session);
    match session
        .execute_source(VECTOR_SCORE_SOURCE, registry)
        .expect("vector candidate scoring procedure executes")
    {
        StatementOutput::Rows(table) => table.row_count(),
        other => panic!("unexpected output: {other:?}"),
    }
}

fn execute_vector_score_repeated_batch(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Option<Arc<CallPlanCache>>,
) -> usize {
    let mut rows = 0;
    for query_index in 0..VECTOR_BATCH_QUERIES {
        let mut session = Session::new(graph);
        if let Some(cache) = cache.as_ref() {
            session = session.with_call_plan_cache(Arc::clone(cache));
        }
        bind_vector_score_inputs_for(&mut session, query_index);
        match session
            .execute_source(VECTOR_SCORE_SOURCE, registry)
            .expect("single vector candidate scoring procedure executes")
        {
            StatementOutput::Rows(table) => rows += table.row_count(),
            other => panic!("unexpected output: {other:?}"),
        }
    }
    rows
}

fn execute_vector_score_batch(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Option<Arc<CallPlanCache>>,
) -> usize {
    let mut session = Session::new(graph);
    if let Some(cache) = cache {
        session = session.with_call_plan_cache(cache);
    }
    bind_vector_score_batch_inputs(&mut session);
    match session
        .execute_source(VECTOR_SCORE_BATCH_SOURCE, registry)
        .expect("batched vector candidate scoring procedure executes")
    {
        StatementOutput::Rows(table) => table.row_count(),
        other => panic!("unexpected output: {other:?}"),
    }
}

fn execute_vector_ann_repeated_batch(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Option<Arc<CallPlanCache>>,
) -> usize {
    let mut rows = 0;
    for query_index in 0..VECTOR_BATCH_QUERIES {
        let mut session = Session::new(graph);
        if let Some(cache) = cache.as_ref() {
            session = session.with_call_plan_cache(Arc::clone(cache));
        }
        session.bind_parameter(
            db_string("query"),
            Value::Vector(vector_value(query_index, VECTOR_DIMENSION)),
        );
        match session
            .execute_source(VECTOR_ANN_SOURCE, registry)
            .expect("single ANN vector search procedure executes")
        {
            StatementOutput::Rows(table) => rows += table.row_count(),
            other => panic!("unexpected output: {other:?}"),
        }
    }
    rows
}

fn execute_vector_exact_repeated_batch(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Option<Arc<CallPlanCache>>,
) -> usize {
    let mut rows = 0;
    for query_index in 0..VECTOR_BATCH_QUERIES {
        let mut session = Session::new(graph);
        if let Some(cache) = cache.as_ref() {
            session = session.with_call_plan_cache(Arc::clone(cache));
        }
        session.bind_parameter(
            db_string("query"),
            Value::Vector(vector_value(query_index, VECTOR_DIMENSION)),
        );
        match session
            .execute_source(VECTOR_SOURCE, registry)
            .expect("single exact vector search procedure executes")
        {
            StatementOutput::Rows(table) => rows += table.row_count(),
            other => panic!("unexpected output: {other:?}"),
        }
    }
    rows
}

fn execute_vector_exact_batch(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Option<Arc<CallPlanCache>>,
) -> usize {
    let mut session = Session::new(graph);
    if let Some(cache) = cache {
        session = session.with_call_plan_cache(cache);
    }
    session.bind_parameter(db_string("queries"), vector_query_batch());
    match session
        .execute_source(VECTOR_BATCH_SOURCE, registry)
        .expect("batched exact vector search procedure executes")
    {
        StatementOutput::Rows(table) => table.row_count(),
        other => panic!("unexpected output: {other:?}"),
    }
}

fn execute_vector_ann_batch(
    graph: &SharedGraph,
    registry: &BuiltinProcedureRegistry,
    cache: Option<Arc<CallPlanCache>>,
) -> usize {
    let mut session = Session::new(graph);
    if let Some(cache) = cache {
        session = session.with_call_plan_cache(cache);
    }
    session.bind_parameter(db_string("queries"), vector_query_batch());
    match session
        .execute_source(VECTOR_ANN_BATCH_SOURCE, registry)
        .expect("batched ANN vector search procedure executes")
    {
        StatementOutput::Rows(table) => table.row_count(),
        other => panic!("unexpected output: {other:?}"),
    }
}

fn vector_graph(scale: usize, dimension: usize) -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(71_002));
    let label = db_string("VectorDoc");
    let embedding_key = db_string("embedding");
    {
        let mut txn = graph.begin_write();
        {
            let mut mutator = txn.mutator();
            for idx in 0..scale {
                let props = PropertyMap::from_pairs([(
                    embedding_key.clone(),
                    Value::Vector(vector_value(idx, dimension)),
                )])
                .expect("bench vector properties are valid");
                mutator
                    .create_node(LabelSet::single(label.clone()), props)
                    .expect("bench vector node insert succeeds");
            }
        }
        txn.commit().expect("bench vector fixture commits");
    }
    graph
}

fn vector_graph_indexed(scale: usize, dimension: usize) -> SharedGraph {
    let graph = vector_graph(scale, dimension);
    graph
        .create_vector_index(
            db_string("VectorDoc"),
            db_string("embedding"),
            VectorIndexKind::Flat,
            u32::try_from(dimension).expect("bench dimension fits u32"),
        )
        .expect("bench vector index builds");
    graph
}

fn vector_graph_hnsw_indexed(scale: usize, dimension: usize) -> SharedGraph {
    let graph = vector_graph(scale, dimension);
    graph
        .create_vector_index(
            db_string("VectorDoc"),
            db_string("embedding"),
            VectorIndexKind::HnswSquaredEuclidean,
            u32::try_from(dimension).expect("bench dimension fits u32"),
        )
        .expect("bench HNSW vector index builds");
    graph
}

fn vector_query_batch() -> Value {
    Value::List(
        (0..VECTOR_BATCH_QUERIES)
            .map(|query_index| Value::Vector(vector_value(query_index, VECTOR_DIMENSION)))
            .collect(),
    )
}

fn bind_vector_score_inputs(session: &mut Session<'_>) {
    bind_vector_score_inputs_for(session, 0);
}

fn bind_vector_score_inputs_for(session: &mut Session<'_>, query_index: usize) {
    session.bind_parameter(
        db_string("query"),
        Value::Vector(vector_value(query_index, VECTOR_DIMENSION)),
    );
    session.bind_parameter(db_string("nodes"), vector_score_candidates(query_index));
}

fn bind_vector_score_batch_inputs(session: &mut Session<'_>) {
    session.bind_parameter(db_string("queries"), vector_query_batch());
    session.bind_parameter(db_string("nodes"), vector_score_candidate_batch());
}

fn vector_score_candidate_batch() -> Value {
    Value::List(
        (0..VECTOR_BATCH_QUERIES)
            .map(vector_score_candidates)
            .collect(),
    )
}

fn vector_score_candidates(query_index: usize) -> Value {
    let max_start = VECTOR_SCALE.saturating_sub(VECTOR_SCORE_CANDIDATES);
    let start = if max_start == 0 {
        0
    } else {
        (query_index * VECTOR_SCORE_CANDIDATES) % max_start
    };
    Value::List(
        (start..start + VECTOR_SCORE_CANDIDATES)
            .map(|idx| Value::NodeRef(NodeId::new((idx + 1) as u64)))
            .collect(),
    )
}

fn vector_value(seed: usize, dimension: usize) -> VectorValue {
    let components: Vec<f32> = (0..dimension)
        .map(|dim| {
            let raw = (seed.wrapping_mul(31) + dim.wrapping_mul(17)) % 1_000;
            raw as f32 / 1_000.0
        })
        .collect();
    VectorValue::new(components).expect("bench vector is valid")
}

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("bench string fits DB string cap")
}

criterion_group! {
    name = procedure_call_repeat_group;
    config = common::criterion_config();
    targets = bench_procedure_call_repeat, bench_feature_status_procedure,
        native_calls::bench_native_calls,
        call_pipeline::bench_call_pipeline,
        bench_vector_search_procedure,
        rank_fusion::bench_rank_fusion_procedure,
        vector_ann_expanded::bench_vector_ann_expanded_procedure,
        vector_candidate_state::bench_vector_candidate_state_procedure,
        vector_expanded::bench_vector_expanded_procedure,
        vector_omlx_query_roots::bench_vector_omlx_query_roots_procedure,
        vector_neighbors::bench_vector_neighbor_procedure
}
criterion_main!(procedure_call_repeat_group);
