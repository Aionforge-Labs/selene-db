use super::*;

const ANN_PROPERTY_FILTER_DIMENSION: usize = 128;
const ANN_PROPERTY_FILTER_K: usize = 10;
const ANN_PROPERTY_FILTER_EF_SEARCH: usize = 64;
const ANN_PROPERTY_FILTER_CARDINALITY: usize = 256;

pub(super) fn bench_exact_vector_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_exact_vector_scan");
    for scale in vector_scan_scales() {
        for &(metric_name, metric) in &[
            ("squared_euclidean", VectorMetric::SquaredEuclidean),
            ("cosine", VectorMetric::Cosine),
        ] {
            for &(index_name, index_kind) in &[
                ("unindexed", None),
                ("flat_index", Some(VectorIndexKind::Flat)),
            ] {
                let fixture = VectorFixture::build(scale, 128, index_kind);
                let memory_suffix = fixture.memory_id_suffix();
                group.throughput(Throughput::Elements(fixture.scale() as u64));
                group.bench_with_input(
                    BenchmarkId::new(
                        format!("{index_name}_{metric_name}_dim128_k10_{memory_suffix}"),
                        fixture.scale(),
                    ),
                    &fixture,
                    |b, fixture| {
                        b.iter(|| {
                            let hits = fixture
                                .graph()
                                .exact_vector_search_nodes(
                                    &fixture.label(),
                                    &fixture.embedding_key(),
                                    fixture.query(),
                                    metric,
                                    10,
                                )
                                .expect("fixture vectors have matching dimensions");
                            std::hint::black_box(hits.len());
                        });
                    },
                );
                group.bench_with_input(
                    BenchmarkId::new(
                        format!(
                            "{index_name}_{metric_name}_dim128_k10_checked_with_deadline_{memory_suffix}"
                        ),
                        fixture.scale(),
                    ),
                    &fixture,
                    |b, fixture| {
                        let checker = deadline_checker();
                        b.iter(|| {
                            let hits = fixture
                                .graph()
                                .exact_vector_search_nodes_checked(
                                    &fixture.label(),
                                    &fixture.embedding_key(),
                                    fixture.query(),
                                    metric,
                                    10,
                                    checker,
                                )
                                .expect("fixture vectors have matching dimensions");
                            std::hint::black_box(hits.len());
                        });
                    },
                );
            }
        }
    }
    group.finish();
}

pub(super) fn bench_ann_recall(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_ann_recall_validation");
    for scale in vector_scan_scales() {
        for &profile in ANN_RECALL_PROFILES {
            for &variant in profile.variants() {
                let fixture = AnnRecallFixture::build(
                    profile,
                    variant,
                    scale,
                    ANN_RECALL_QUERIES,
                    ANN_RECALL_K,
                );
                let memory_suffix = format_vector_memory_id_suffix(fixture.memory_usage());
                let profile_name = ann_recall_benchmark_name(&fixture);
                group.throughput(Throughput::Elements(
                    (fixture.scale() * fixture.query_count()) as u64,
                ));
                for &ef_search in variant.search_widths() {
                    let recall = fixture.mean_recall(ef_search);
                    let quality = fixture.mean_distance_quality(ef_search);
                    group.bench_with_input(
                        BenchmarkId::new(
                            format!(
                                "{profile_name}_d{}_k{ANN_RECALL_K}_ef{ef_search}_idbp{}_dqbp{}_{}",
                                fixture.dimension(),
                                recall_basis_points(recall),
                                recall_basis_points(quality),
                                memory_suffix
                            ),
                            fixture.scale(),
                        ),
                        &fixture,
                        |b, fixture| {
                            b.iter(|| {
                                let overlap = fixture.total_overlap(ef_search);
                                std::hint::black_box(overlap);
                            });
                        },
                    );
                }
            }
        }
    }
    group.finish();
}

pub(super) fn bench_ann_property_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_ann_property_filter");
    for scale in vector_scan_scales() {
        let fixture = AnnPropertyFilterFixture::build(scale, ANN_PROPERTY_FILTER_DIMENSION);
        let memory_suffix = fixture.memory_id_suffix();
        group.throughput(Throughput::Elements(
            fixture.allowed_candidates().len() as u64
        ));
        group.bench_with_input(
            BenchmarkId::new(
                format!(
                    "hnsw_cosine_namespace_sparse_d128_k{ANN_PROPERTY_FILTER_K}_ef{ANN_PROPERTY_FILTER_EF_SEARCH}_rows{}_{}",
                    compact_count(fixture.allowed_candidates().len() as u64),
                    memory_suffix
                ),
                fixture.scale(),
            ),
            &fixture,
            |b, fixture| {
                let checker = deadline_checker();
                b.iter(|| {
                    let hits = fixture
                        .graph()
                        .approximate_vector_search_nodes_in_candidates_checked(
                            fixture.label(),
                            fixture.embedding_key(),
                            fixture.query(),
                            fixture.allowed_candidates(),
                            ApproximateVectorSearchOptions::new(
                                VectorMetric::Cosine,
                                ANN_PROPERTY_FILTER_K,
                                ANN_PROPERTY_FILTER_EF_SEARCH,
                            ),
                            checker,
                        )
                        .expect("filtered HNSW fixture has a matching ANN index");
                    std::hint::black_box(hits.len());
                });
            },
        );
    }
    group.finish();
}

fn ann_recall_benchmark_name(fixture: &AnnRecallFixture) -> String {
    if fixture.variant_name_suffix().is_empty() {
        fixture.profile().name().to_owned()
    } else {
        format!(
            "{}_{}",
            fixture.profile().name(),
            fixture.variant_name_suffix()
        )
    }
}

pub(super) fn vector_scan_scales() -> Vec<usize> {
    std::env::var("SELENE_VECTOR_BENCH_SCALES")
        .ok()
        .and_then(|raw| {
            let mut scales: Vec<_> = raw
                .split(',')
                .filter_map(|part| part.trim().parse::<usize>().ok())
                .filter(|scale| *scale > 0)
                .collect();
            scales.sort_unstable();
            scales.dedup();
            (!scales.is_empty()).then_some(scales)
        })
        .unwrap_or_else(|| BenchProfile::from_env().scales().to_vec())
}

#[derive(Clone, Debug)]
struct VectorFixture {
    scale: usize,
    graph: SeleneGraph,
    label: DbString,
    embedding_key: DbString,
    query: VectorValue,
}

impl VectorFixture {
    fn build(scale: usize, dimension: usize, index_kind: Option<VectorIndexKind>) -> Self {
        let scale = scale.max(1);
        let label = db_string("VectorDoc").expect("bench label is valid");
        let embedding_key = db_string("embedding").expect("bench key is valid");
        let shared = SharedGraph::new(GraphId::new(9_000 + scale as u64));
        {
            let mut txn = shared.begin_write();
            let mut mutator = txn.mutator();
            for idx in 0..scale {
                let vector = Value::Vector(vector_value(idx, dimension));
                let props = PropertyMap::from_pairs([(embedding_key.clone(), vector)])
                    .expect("bench vector properties are valid");
                mutator
                    .create_node(LabelSet::single(label.clone()), props)
                    .expect("bench vector node insert succeeds");
            }
            if let Some(kind) = index_kind {
                let dimension = u32::try_from(dimension).expect("bench dimension fits u32");
                mutator
                    .create_vector_index(label.clone(), embedding_key.clone(), kind, dimension)
                    .expect("bench vector index build succeeds");
            }
            txn.commit().expect("bench vector fixture commit succeeds");
        }
        Self {
            scale,
            graph: shared.read().as_ref().clone(),
            label,
            embedding_key,
            query: vector_value(0, dimension),
        }
    }

    const fn graph(&self) -> &SeleneGraph {
        &self.graph
    }

    const fn scale(&self) -> usize {
        self.scale
    }

    fn label(&self) -> DbString {
        self.label.clone()
    }

    fn embedding_key(&self) -> DbString {
        self.embedding_key.clone()
    }

    const fn query(&self) -> &VectorValue {
        &self.query
    }

    fn memory_id_suffix(&self) -> String {
        vector_index_memory_id_suffix(&self.graph, &self.label, &self.embedding_key)
    }
}

#[derive(Clone, Debug)]
struct AnnPropertyFilterFixture {
    scale: usize,
    graph: SeleneGraph,
    label: DbString,
    embedding_key: DbString,
    query: VectorValue,
    allowed_candidates: selene_graph::CandidateSet<selene_graph::Node>,
}

impl AnnPropertyFilterFixture {
    fn build(scale: usize, dimension: usize) -> Self {
        let scale = scale.max(1);
        let namespace_count = scale.min(ANN_PROPERTY_FILTER_CARDINALITY);
        let filter_bucket = namespace_count / 2;
        let label = db_string("ScopedVectorDoc").expect("bench label is valid");
        let embedding_key = db_string("embedding").expect("bench key is valid");
        let namespace_key = db_string("namespace").expect("bench key is valid");
        let shared = SharedGraph::new(GraphId::new(9_500 + scale as u64));
        {
            let mut txn = shared.begin_write();
            let mut mutator = txn.mutator();
            for idx in 0..scale {
                let vector = Value::Vector(vector_value(idx, dimension));
                let namespace = Value::String(namespace_value(idx % namespace_count));
                let props = PropertyMap::from_pairs([
                    (embedding_key.clone(), vector),
                    (namespace_key.clone(), namespace),
                ])
                .expect("bench vector properties are valid");
                mutator
                    .create_node(LabelSet::single(label.clone()), props)
                    .expect("bench vector node insert succeeds");
            }
            mutator
                .create_property_index(label.clone(), namespace_key.clone(), TypedIndexKind::String)
                .expect("bench namespace index build succeeds");
            mutator
                .create_vector_index(
                    label.clone(),
                    embedding_key.clone(),
                    VectorIndexKind::HnswCosine,
                    u32::try_from(dimension).expect("bench dimension fits u32"),
                )
                .expect("bench HNSW vector index build succeeds");
            txn.commit()
                .expect("bench filtered vector fixture commit succeeds");
        }
        let graph = shared.read().as_ref().clone();
        let filter_value = Value::String(namespace_value(filter_bucket));
        let allowed_candidates = graph
            .node_candidates_with_property_any(&label, &namespace_key, &[filter_value])
            .expect("bench namespace index is available")
            .expect("bench namespace index admits filter");
        assert!(
            !allowed_candidates.is_empty(),
            "bench namespace filter should admit at least one candidate"
        );
        Self {
            scale,
            graph,
            label,
            embedding_key,
            query: vector_value(0, dimension),
            allowed_candidates,
        }
    }

    const fn graph(&self) -> &SeleneGraph {
        &self.graph
    }

    const fn scale(&self) -> usize {
        self.scale
    }

    const fn label(&self) -> &DbString {
        &self.label
    }

    const fn embedding_key(&self) -> &DbString {
        &self.embedding_key
    }

    const fn query(&self) -> &VectorValue {
        &self.query
    }

    fn allowed_candidates(&self) -> &selene_graph::CandidateSet<selene_graph::Node> {
        &self.allowed_candidates
    }

    fn memory_id_suffix(&self) -> String {
        vector_index_memory_id_suffix(&self.graph, &self.label, &self.embedding_key)
    }
}

fn namespace_value(bucket: usize) -> DbString {
    db_string(&format!("ns_{bucket:04}")).expect("bench namespace is valid")
}

fn vector_index_memory_id_suffix(
    graph: &SeleneGraph,
    label: &DbString,
    property: &DbString,
) -> String {
    graph
        .vector_index_for(label, property)
        .map(|index| format_vector_memory_id_suffix(index.memory_usage()))
        .unwrap_or_else(|| "noidx".to_owned())
}

fn format_vector_memory_id_suffix(usage: VectorIndexMemoryUsage) -> String {
    let ann_suffix = if usage.hnsw_entries > 0 {
        format!(
            "he{}l{}d{}g{}z{}u{}p{}x{}a{}",
            compact_usize(usage.hnsw_entries),
            compact_usize(usage.hnsw_live_entries),
            compact_usize(usage.hnsw_deleted_entries),
            compact_usize(usage.hnsw_link_count),
            compact_usize(usage.hnsw_level_zero_link_count),
            compact_usize(usage.hnsw_upper_layer_link_count),
            compact_usize(usage.hnsw_max_layer_count),
            compact_usize(usage.hnsw_max_links_per_layer),
            compact_usize(usage.hnsw_average_links_per_entry_basis_points)
        )
    } else if usage.ivf_entries > 0 {
        format!(
            "ve{}l{}d{}c{}q{}a{}",
            compact_usize(usage.ivf_entries),
            compact_usize(usage.ivf_live_entries),
            compact_usize(usage.ivf_deleted_entries),
            compact_usize(usage.ivf_centroids),
            compact_usize(usage.ivf_list_count),
            compact_usize(usage.ivf_assigned_entries)
        )
    } else {
        "flat".to_owned()
    };
    format!(
        "m{}-{}_n{}_{}",
        usage.estimated_index_bytes / 1024,
        usage.estimated_reachable_bytes / 1024,
        compact_count(usage.indexed_rows),
        ann_suffix,
    )
}

fn compact_usize(value: usize) -> String {
    compact_count(u64::try_from(value).unwrap_or(u64::MAX))
}

fn compact_count(value: u64) -> String {
    if value >= 1_000 && value.is_multiple_of(1_000) {
        format!("{}k", value / 1_000)
    } else {
        value.to_string()
    }
}

pub(super) fn vector_value(seed: usize, dimension: usize) -> VectorValue {
    VectorValue::new(vector_components(seed, dimension)).expect("bench vector is valid")
}

fn vector_components(seed: usize, dimension: usize) -> Vec<f32> {
    (0..dimension)
        .map(|dim| {
            let raw = (seed.wrapping_mul(31) + dim.wrapping_mul(17)) % 1_000;
            raw as f32 / 1_000.0
        })
        .collect()
}

fn recall_basis_points(recall: f64) -> u64 {
    (recall * 10_000.0).round() as u64
}
