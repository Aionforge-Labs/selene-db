use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use selene_core::{
    Value, VectorMetric, VectorTopK, VectorValue, exact_vector_top_k, vector_squared_norm,
};
use wide::f64x4;

const VECTOR_DIMS: &[usize] = &[128, 768, 1536];
const EXACT_TOP_K_CANDIDATES: usize = 2_048;
const EXACT_TOP_ONE: usize = 1;
const EXACT_TOP_K: usize = 10;
const OMLX_EXACT_TOP_K_DIMS: &[usize] = &[1024, 2560, 4096];
const OMLX_EXACT_TOP_K_CANDIDATES: &[usize] = &[64, 256, 1024, 4096];
const GPU_BASELINE_CASES: &[GpuBaselineCase] = &[
    GpuBaselineCase {
        queries: 1,
        candidates: 4096,
        dimension: 1024,
    },
    GpuBaselineCase {
        queries: 8,
        candidates: 4096,
        dimension: 1024,
    },
    GpuBaselineCase {
        queries: 8,
        candidates: 4096,
        dimension: 2560,
    },
    GpuBaselineCase {
        queries: 16,
        candidates: 4096,
        dimension: 1024,
    },
];

#[derive(Clone, Copy)]
struct GpuBaselineCase {
    queries: usize,
    candidates: usize,
    dimension: usize,
}

pub(crate) fn bench_vector_value(c: &mut Criterion) {
    let mut group = c.benchmark_group("core_vector_value");
    for &dim in VECTOR_DIMS {
        group.throughput(Throughput::Elements(dim as u64));
        group.bench_with_input(
            BenchmarkId::new("construct_validate", dim),
            &dim,
            |b, &dim| {
                b.iter_batched(
                    || vector_components(dim),
                    |components| VectorValue::new(black_box(components)).expect("vector is valid"),
                    BatchSize::SmallInput,
                );
            },
        );

        let vector = VectorValue::new(vector_components(dim)).expect("vector is valid");
        group.bench_with_input(BenchmarkId::new("clone_arc", dim), &vector, |b, vector| {
            b.iter(|| black_box(black_box(vector).clone()));
        });

        let value = Value::Vector(vector);
        group.bench_with_input(
            BenchmarkId::new("logical_roundtrip", dim),
            &value,
            |b, value| {
                b.iter_batched(
                    || value.clone(),
                    |value| {
                        let stored =
                            selene_core::StoredValue::try_from(value).expect("vector is storable");
                        let bytes = selene_core::logical::encode_value(&stored, Default::default())
                            .expect("vector encodes");
                        let decoded =
                            selene_core::logical::decode_value(&bytes, Default::default())
                                .expect("vector decodes");
                        black_box(decoded)
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

pub(crate) fn bench_vector_distance(c: &mut Criterion) {
    let mut group = c.benchmark_group("core_vector_distance");
    for &(name, metric) in &[
        ("squared_euclidean", VectorMetric::SquaredEuclidean),
        ("cosine", VectorMetric::Cosine),
        ("negative_inner_product", VectorMetric::NegativeInnerProduct),
    ] {
        for &dim in VECTOR_DIMS {
            group.throughput(Throughput::Elements(dim as u64));
            let lhs = VectorValue::new(vector_components_seeded(dim, 1)).expect("vector is valid");
            let rhs = VectorValue::new(vector_components_seeded(dim, 2)).expect("vector is valid");
            group.bench_with_input(BenchmarkId::new(name, dim), &dim, |b, _| {
                b.iter(|| {
                    metric
                        .distance(black_box(&lhs), black_box(&rhs))
                        .expect("same dim")
                });
            });
        }
    }
    group.finish();
}

pub(crate) fn bench_vector_exact_top_k(c: &mut Criterion) {
    let mut group = c.benchmark_group("core_vector_exact_top_k");
    group.throughput(Throughput::Elements(EXACT_TOP_K_CANDIDATES as u64));

    let query = VectorValue::new(vector_components_seeded(128, 0)).expect("query is valid");
    let candidates: Vec<VectorValue> = (0..EXACT_TOP_K_CANDIDATES)
        .map(|idx| VectorValue::new(vector_components_seeded(128, idx + 1)).expect("valid vector"))
        .collect();
    let candidate_refs: Vec<(usize, &VectorValue)> = candidates.iter().enumerate().collect();

    for &(name, metric) in &[
        (
            "squared_euclidean_2048x128_k10",
            VectorMetric::SquaredEuclidean,
        ),
        ("cosine_2048x128_k10", VectorMetric::Cosine),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                exact_vector_top_k(
                    metric,
                    black_box(&query),
                    candidate_refs
                        .iter()
                        .map(|(key, vector)| (black_box(*key), black_box(*vector))),
                    black_box(EXACT_TOP_K),
                )
                .expect("all dimensions match")
            });
        });
    }

    for &(name, metric) in &[
        (
            "squared_euclidean_2048x128_k1",
            VectorMetric::SquaredEuclidean,
        ),
        ("cosine_2048x128_k1", VectorMetric::Cosine),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                exact_vector_top_k(
                    metric,
                    black_box(&query),
                    candidate_refs
                        .iter()
                        .map(|(key, vector)| (black_box(*key), black_box(*vector))),
                    black_box(EXACT_TOP_ONE),
                )
                .expect("all dimensions match")
            });
        });
    }

    group.bench_function("cosine_unbound_loop_2048x128_k10", |b| {
        b.iter(|| {
            let mut top_k = VectorTopK::new(black_box(EXACT_TOP_K));
            for (key, vector) in &candidate_refs {
                let distance = VectorMetric::Cosine
                    .distance(black_box(&query), black_box(*vector))
                    .expect("all dimensions match");
                top_k.push_distance(black_box(*key), distance);
            }
            top_k.into_hits()
        });
    });

    for &dim in OMLX_EXACT_TOP_K_DIMS {
        let query = VectorValue::new(vector_components_seeded(dim, 0)).expect("query is valid");
        let max_candidate_count = OMLX_EXACT_TOP_K_CANDIDATES
            .last()
            .copied()
            .expect("candidate widths are non-empty");
        let candidates: Vec<VectorValue> = (0..max_candidate_count)
            .map(|idx| {
                VectorValue::new(vector_components_seeded(dim, idx + 1)).expect("valid vector")
            })
            .collect();
        let candidate_refs: Vec<(usize, &VectorValue)> = candidates.iter().enumerate().collect();
        for &candidate_count in OMLX_EXACT_TOP_K_CANDIDATES {
            group.throughput(Throughput::Elements(candidate_count as u64));
            group.bench_function(format!("cosine_omlx_{candidate_count}x{dim}_k10"), |b| {
                b.iter(|| {
                    exact_vector_top_k(
                        VectorMetric::Cosine,
                        black_box(&query),
                        candidate_refs
                            .iter()
                            .take(black_box(candidate_count))
                            .map(|(key, vector)| (black_box(*key), black_box(*vector))),
                        black_box(EXACT_TOP_K),
                    )
                    .expect("all dimensions match")
                });
            });
        }
    }

    group.finish();
}

pub(crate) fn bench_vector_gpu_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("core_vector_gpu_baseline");
    for case in GPU_BASELINE_CASES {
        let fixture = GpuBaselineFixture::build(*case);
        let score_id = fixture.id("cpu_cosine_rerank");
        group.throughput(Throughput::Elements(fixture.score_count() as u64));
        group.bench_function(score_id, |b| {
            b.iter(|| black_box(fixture.score_all()));
        });

        let copy_id = fixture.id("host_pack_f32");
        let mut buffer = vec![0.0f32; fixture.transfer_floats()];
        group.throughput(Throughput::Bytes(fixture.transfer_bytes() as u64));
        group.bench_function(copy_id, |b| {
            b.iter(|| fixture.copy_inputs(black_box(&mut buffer)));
        });

        let query_copy_id = fixture.id("host_pack_queries_f32");
        let mut query_buffer = vec![0.0f32; fixture.query_transfer_floats()];
        group.throughput(Throughput::Bytes(fixture.query_transfer_bytes() as u64));
        group.bench_function(query_copy_id, |b| {
            b.iter(|| fixture.copy_queries(black_box(&mut query_buffer)));
        });

        let candidate_copy_id = fixture.id("host_pack_candidates_f32");
        let mut candidate_buffer = vec![0.0f32; fixture.candidate_transfer_floats()];
        group.throughput(Throughput::Bytes(fixture.candidate_transfer_bytes() as u64));
        group.bench_function(candidate_copy_id, |b| {
            b.iter(|| fixture.copy_candidates(black_box(&mut candidate_buffer)));
        });

        let slab_score_id = fixture.id("cpu_cosine_resident_slab");
        group.throughput(Throughput::Elements(fixture.score_count() as u64));
        group.bench_function(slab_score_id, |b| {
            b.iter(|| black_box(fixture.score_all_resident_slab()));
        });
    }
    group.finish();
}

fn vector_components(dim: usize) -> Vec<f32> {
    vector_components_seeded(dim, 0)
}

fn vector_components_seeded(dim: usize, seed: usize) -> Vec<f32> {
    (0..dim)
        .map(|idx| (((idx * 31 + seed * 17) % 1_021) as f32 - 510.0) / 256.0)
        .collect()
}

struct GpuBaselineFixture {
    case: GpuBaselineCase,
    queries: Vec<VectorValue>,
    candidates: Vec<VectorValue>,
    query_slab: Vec<f32>,
    candidate_slab: Vec<f32>,
    query_norms: Vec<f64>,
    candidate_norms: Vec<f64>,
}

impl GpuBaselineFixture {
    fn build(case: GpuBaselineCase) -> Self {
        let queries: Vec<VectorValue> = (0..case.queries)
            .map(|idx| {
                VectorValue::new(vector_components_seeded(case.dimension, idx))
                    .expect("query vector is valid")
            })
            .collect();
        let candidates: Vec<VectorValue> = (0..case.candidates)
            .map(|idx| {
                VectorValue::new(vector_components_seeded(case.dimension, idx + 1_000))
                    .expect("candidate vector is valid")
            })
            .collect();
        let query_slab = flatten_vectors(&queries);
        let candidate_slab = flatten_vectors(&candidates);
        let query_norms = queries.iter().map(vector_squared_norm).collect();
        let candidate_norms = candidates.iter().map(vector_squared_norm).collect();
        let fixture = Self {
            case,
            queries,
            candidates,
            query_slab,
            candidate_slab,
            query_norms,
            candidate_norms,
        };
        fixture.assert_resident_slab_matches_canonical();
        fixture
    }

    fn id(&self, name: &str) -> String {
        format!(
            "{name}_q{}x{}x{}_k{EXACT_TOP_K}",
            self.case.queries, self.case.candidates, self.case.dimension
        )
    }

    const fn score_count(&self) -> usize {
        self.case.queries * self.case.candidates
    }

    const fn transfer_floats(&self) -> usize {
        (self.case.queries + self.case.candidates) * self.case.dimension
    }

    const fn transfer_bytes(&self) -> usize {
        self.transfer_floats() * std::mem::size_of::<f32>()
    }

    const fn query_transfer_floats(&self) -> usize {
        self.case.queries * self.case.dimension
    }

    const fn query_transfer_bytes(&self) -> usize {
        self.query_transfer_floats() * std::mem::size_of::<f32>()
    }

    const fn candidate_transfer_floats(&self) -> usize {
        self.case.candidates * self.case.dimension
    }

    const fn candidate_transfer_bytes(&self) -> usize {
        self.candidate_transfer_floats() * std::mem::size_of::<f32>()
    }

    fn score_all(&self) -> usize {
        let mut hits = 0;
        for query in &self.queries {
            hits += exact_vector_top_k(
                VectorMetric::Cosine,
                black_box(query),
                self.candidates
                    .iter()
                    .enumerate()
                    .map(|(key, vector)| (black_box(key), black_box(vector))),
                black_box(EXACT_TOP_K),
            )
            .expect("all dimensions match")
            .len();
        }
        hits
    }

    fn copy_inputs(&self, destination: &mut [f32]) -> usize {
        let mut offset = 0;
        for vector in self.queries.iter().chain(self.candidates.iter()) {
            let end = offset + vector.dimension();
            destination[offset..end].copy_from_slice(vector.as_slice());
            offset = end;
        }
        offset
    }

    fn copy_queries(&self, destination: &mut [f32]) -> usize {
        destination[..self.query_slab.len()].copy_from_slice(&self.query_slab);
        self.query_slab.len()
    }

    fn copy_candidates(&self, destination: &mut [f32]) -> usize {
        destination[..self.candidate_slab.len()].copy_from_slice(&self.candidate_slab);
        self.candidate_slab.len()
    }

    fn score_all_resident_slab(&self) -> usize {
        let mut hits = 0;
        for query_idx in 0..self.case.queries {
            let query = self.query_slice(query_idx);
            let query_norm = self.query_norms[query_idx];
            let mut top_k = VectorTopK::new(EXACT_TOP_K);
            for candidate_idx in 0..self.case.candidates {
                let candidate = self.candidate_slice(candidate_idx);
                let distance = cosine_distance_with_norms(
                    black_box(query),
                    black_box(candidate),
                    black_box(query_norm),
                    black_box(self.candidate_norms[candidate_idx]),
                );
                top_k.push_distance(black_box(candidate_idx), distance);
            }
            hits += top_k.into_hits().len();
        }
        hits
    }

    fn assert_resident_slab_matches_canonical(&self) {
        for query_idx in 0..self.case.queries {
            let exact = exact_vector_top_k(
                VectorMetric::Cosine,
                &self.queries[query_idx],
                self.candidates.iter().enumerate(),
                EXACT_TOP_K,
            )
            .expect("fixture vectors are comparable");
            let slab = self.resident_slab_top_k(query_idx);
            assert_eq!(exact.len(), slab.len(), "resident slab hit count drifted");
            for (exact, slab) in exact.iter().zip(slab.iter()) {
                assert_eq!(exact.key, slab.key, "resident slab key drifted");
                assert!(
                    (exact.distance - slab.distance).abs() <= f64::EPSILON,
                    "resident slab distance drifted: exact={} slab={}",
                    exact.distance,
                    slab.distance
                );
            }
        }
    }

    fn resident_slab_top_k(&self, query_idx: usize) -> Vec<selene_core::VectorSearchHit<usize>> {
        let query = self.query_slice(query_idx);
        let query_norm = self.query_norms[query_idx];
        let mut top_k = VectorTopK::new(EXACT_TOP_K);
        for candidate_idx in 0..self.case.candidates {
            let distance = cosine_distance_with_norms(
                query,
                self.candidate_slice(candidate_idx),
                query_norm,
                self.candidate_norms[candidate_idx],
            );
            top_k.push_distance(candidate_idx, distance);
        }
        top_k.into_hits()
    }

    fn query_slice(&self, index: usize) -> &[f32] {
        vector_window(&self.query_slab, index, self.case.dimension)
    }

    fn candidate_slice(&self, index: usize) -> &[f32] {
        vector_window(&self.candidate_slab, index, self.case.dimension)
    }
}

fn flatten_vectors(vectors: &[VectorValue]) -> Vec<f32> {
    let total_components = vectors.iter().map(VectorValue::dimension).sum();
    let mut slab = Vec::with_capacity(total_components);
    for vector in vectors {
        slab.extend_from_slice(vector.as_slice());
    }
    slab
}

fn vector_window(slab: &[f32], index: usize, dimension: usize) -> &[f32] {
    &slab[index * dimension..index * dimension + dimension]
}

fn cosine_distance_with_norms(
    query: &[f32],
    candidate: &[f32],
    query_norm: f64,
    candidate_norm: f64,
) -> f64 {
    let similarity = dot_slices(query, candidate) / (query_norm.sqrt() * candidate_norm.sqrt());
    let distance = 1.0 - similarity.clamp(-1.0, 1.0);
    if distance == 0.0 { 0.0 } else { distance }
}

fn dot_slices(lhs: &[f32], rhs: &[f32]) -> f64 {
    let mut chunks_lhs = lhs.chunks_exact(4);
    let mut chunks_rhs = rhs.chunks_exact(4);
    let mut product = f64x4::ZERO;
    for (lhs, rhs) in chunks_lhs.by_ref().zip(chunks_rhs.by_ref()) {
        let lhs = f64x4_from_f32(lhs);
        let rhs = f64x4_from_f32(rhs);
        product += lhs * rhs;
    }
    let mut product = product.reduce_add();
    for (&lhs, &rhs) in chunks_lhs.remainder().iter().zip(chunks_rhs.remainder()) {
        product += f64::from(lhs) * f64::from(rhs);
    }
    product
}

fn f64x4_from_f32(chunk: &[f32]) -> f64x4 {
    f64x4::from([
        f64::from(chunk[0]),
        f64::from(chunk[1]),
        f64::from(chunk[2]),
        f64::from(chunk[3]),
    ])
}
