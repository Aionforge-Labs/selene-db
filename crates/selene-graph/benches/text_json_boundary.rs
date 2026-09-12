//! CPU-only text/JSON costs; no expression-index or facade latency claim.
use criterion::{BenchmarkId, Criterion};
use selene_core::{
    CancellationChecker, GraphId, JsonValue, LabelSet, PropertyMap, Value, db_string,
};
use selene_graph::{CandidateStateSpec, MaintainedCandidateStateProvider, SharedGraph};
use selene_testing::BenchProfile;

#[allow(clippy::print_stderr)]
pub fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_text_json_boundary");
    for &n in BenchProfile::from_env().scales() {
        let graph = SharedGraph::new(GraphId::new(408));
        let label = db_string("Memory").unwrap();
        let body = db_string("body").unwrap();
        let payload = db_string("payload").unwrap();
        let mut tx = graph.begin_write();
        for i in 0..n {
            let text = if i % 10 == 0 {
                "memory current"
            } else {
                "memory archive"
            };
            let json = JsonValue::new(serde_json::json!({"current": i % 10 == 0})).unwrap();
            tx.mutator()
                .create_node(
                    LabelSet::single(label.clone()),
                    PropertyMap::from_pairs([
                        (body.clone(), Value::String(db_string(text).unwrap())),
                        (payload.clone(), Value::Json(json)),
                    ])
                    .unwrap(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
        let primary = graph.read();
        graph
            .create_text_index(label.clone(), body.clone())
            .unwrap();
        let snapshot = graph.read();
        let index = snapshot.text_index_for(&label, &body).unwrap();
        let oracle = primary
            .exact_text_search_nodes(&label, &body, "current", n)
            .unwrap();
        assert_eq!(index.search("current", n), oracle);
        eprintln!(
            "text_json_boundary n={n} text_index_estimated_bytes={} json_derived_index_bytes=0 (estimates, not allocator/RSS)",
            index.memory_usage().estimated_index_bytes
        );
        group.bench_function(BenchmarkId::new("text_build_rebuild", n), |b| {
            b.iter(|| std::hint::black_box(primary.build_text_index(&label, &body).unwrap()))
        });
        let spec =
            CandidateStateSpec::new(db_string("current").unwrap()).require_label(label.clone());
        group.bench_function(BenchmarkId::new("provider_rebuild", n), |b| {
            b.iter(|| {
                std::hint::black_box(
                    MaintainedCandidateStateProvider::from_graph([spec.clone()], &primary).unwrap(),
                )
            })
        });
        group.bench_function(BenchmarkId::new("text_full_scan", n), |b| {
            b.iter(|| {
                std::hint::black_box(
                    primary
                        .exact_text_search_nodes(&label, &body, "current", n)
                        .unwrap(),
                )
            })
        });
        let json = JsonValue::new(serde_json::json!({"current": true})).unwrap();
        group.bench_function(BenchmarkId::new("json_full_scan", n), |b| {
            b.iter(|| {
                std::hint::black_box(
                    primary
                        .exact_json_contains_nodes(&label, &payload, &json, n)
                        .unwrap(),
                )
            })
        });
        for percent in [1, 10, 100] {
            let count = n * percent / 100;
            let candidates = snapshot
                .bind_node_candidates(snapshot.live_node_candidates().unwrap().iter().take(count))
                .unwrap();
            let ids: Vec<_> = candidates.iter().collect();
            let expected: Vec<_> = oracle
                .iter()
                .filter(|hit| ids.contains(&hit.node_id))
                .cloned()
                .collect();
            assert_eq!(
                snapshot
                    .score_text_candidates_checked(
                        &label,
                        &body,
                        "current",
                        &candidates,
                        n,
                        CancellationChecker::disabled()
                    )
                    .unwrap(),
                expected
            );
            assert_eq!(
                snapshot
                    .exact_json_contains_candidate_nodes(&label, &payload, &json, &ids, n)
                    .unwrap()
                    .len(),
                count.div_ceil(10)
            );
            group.bench_function(
                BenchmarkId::new(format!("text_candidates_{percent}pct"), n),
                |b| {
                    b.iter(|| {
                        std::hint::black_box(
                            snapshot
                                .score_text_candidates_checked(
                                    &label,
                                    &body,
                                    "current",
                                    &candidates,
                                    n,
                                    CancellationChecker::disabled(),
                                )
                                .unwrap(),
                        )
                    })
                },
            );
            group.bench_function(
                BenchmarkId::new(format!("json_candidates_{percent}pct"), n),
                |b| {
                    b.iter(|| {
                        std::hint::black_box(
                            snapshot
                                .exact_json_contains_candidate_nodes(
                                    &label, &payload, &json, &ids, n,
                                )
                                .unwrap(),
                        )
                    })
                },
            );
        }
    }
    group.finish();
}
