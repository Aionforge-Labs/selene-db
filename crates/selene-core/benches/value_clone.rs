#![allow(missing_docs)]
//! `Value` clone cost, `PropertyMap`/diff construction, plus native
//! vector-value construction, JSON rendering, and serde baselines.
//!
//! `Value` boxes the formerly oversized variants, so the companion compile-time
//! `size_of::<Value>` ceiling in `value.rs` is the zero-cost re-bloat tripwire.
//! Clone, construction, and mutation rows stay visible as shape-specific
//! throughput guards.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[path = "value_clone/json.rs"]
mod json;
#[path = "value_clone/numeric.rs"]
mod numeric;
#[path = "value_clone/vector.rs"]
mod vector;

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use selene_core::logical::{Encoder, GraphDelta};
use selene_core::{
    Change, GraphId, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value, VectorValue,
    db_string,
};

const N: usize = 1_024;

// Profile-aware criterion config WITHOUT a selene-testing dep (that crate
// depends on selene-core, so importing BenchProfile here would cycle).
fn bench_config() -> Criterion {
    let (samples, ms) = match std::env::var("SELENE_BENCH_PROFILE").ok().as_deref() {
        Some("full") | Some("stress") => (30usize, 1_500u64),
        _ => (10, 500),
    };
    Criterion::default()
        .sample_size(samples)
        .warm_up_time(std::time::Duration::from_millis(100))
        .measurement_time(std::time::Duration::from_millis(ms))
}

fn encode_map_delta(map: &PropertyMap) -> GraphDelta {
    GraphDelta {
        id: GraphId::new(1),
        previous: None,
        generation: 1,
        next_node_id: 100,
        next_edge_id: 100,
        definition: None,
        backing_indexes: vec![],
        changes: vec![Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::new(),
            properties: map.clone(),
        }],
    }
}

fn encode_map_logical(map: &PropertyMap) -> Vec<u8> {
    let delta = encode_map_delta(map);
    let mut encoder = Encoder::new(Default::default()).expect("logical encoder fits limits");
    delta.encode(&mut encoder).expect("map encodes");
    encoder.finish()
}

fn encode_diff_delta(diff: &PropertyDiff) -> GraphDelta {
    GraphDelta {
        id: GraphId::new(1),
        previous: None,
        generation: 1,
        next_node_id: 100,
        next_edge_id: 100,
        definition: None,
        backing_indexes: vec![],
        changes: vec![Change::NodeUpdated {
            id: NodeId::new(1),
            labels_diff: LabelDiff::new([], []).expect("empty label diff is valid"),
            properties_diff: diff.clone(),
        }],
    }
}

fn encode_diff_logical(diff: &PropertyDiff) -> Vec<u8> {
    let delta = encode_diff_delta(diff);
    let mut encoder = Encoder::new(Default::default()).expect("logical encoder fits limits");
    delta.encode(&mut encoder).expect("diff encodes");
    encoder.finish()
}

fn span() -> jiff::Span {
    "PT1H30M".parse().expect("fixed span parses")
}

fn zoned() -> jiff::Zoned {
    "2024-06-01T12:00:00[UTC]"
        .parse()
        .expect("fixed zoned parses")
}

fn mixed_values() -> Vec<Value> {
    let span = span();
    let zoned = zoned();
    (0..N)
        .map(|i| match i % 6 {
            0 => Value::Int(i as i64),
            1 => Value::Float(i as f64 * 1.5),
            2 => Value::String(
                db_string(&format!("v{}", i % 64)).expect("string fits DB string cap"),
            ),
            3 => Value::Duration(Box::new(span)),
            4 => Value::ZonedDateTime(Box::new(zoned.clone())),
            _ => Value::Vector(VectorValue::new(vector_components(32)).expect("vector is valid")),
        })
        .collect()
}

fn mixed_property_map() -> PropertyMap {
    PropertyMap::from_pairs([
        (db_string("i").expect("key"), Value::Int(42)),
        (db_string("f").expect("key"), Value::Float(2.5)),
        (
            db_string("s").expect("key"),
            Value::String(db_string("hello").expect("value")),
        ),
        (
            db_string("d").expect("key"),
            Value::Duration(Box::new(span())),
        ),
        (
            db_string("z").expect("key"),
            Value::ZonedDateTime(Box::new(zoned())),
        ),
    ])
    .expect("property map fits core caps")
}

fn single_property_pair() -> (selene_core::DbString, Value) {
    (
        db_string("score").expect("key fits DB string cap"),
        Value::Int(42),
    )
}

fn single_compact_key_value() -> (selene_core::DbString, Option<Value>) {
    (
        db_string("score").expect("key fits DB string cap"),
        Some(Value::Int(42)),
    )
}

fn single_compact_property_map() -> PropertyMap {
    let (key, value) = single_compact_key_value();
    PropertyMap::compact([key], [value]).expect("compact property map fits core caps")
}

fn wide_property_pairs(width: usize) -> Vec<(selene_core::DbString, Value)> {
    (0..width)
        .rev()
        .map(|idx| {
            (
                db_string(&format!("wide_property_{idx:04}")).expect("key fits DB string cap"),
                Value::Int(idx as i64),
            )
        })
        .collect()
}

fn wide_property_pairs_sorted(width: usize) -> Vec<(selene_core::DbString, Value)> {
    (0..width)
        .map(|idx| {
            (
                db_string(&format!("wide_property_{idx:04}")).expect("key fits DB string cap"),
                Value::Int(idx as i64),
            )
        })
        .collect()
}

fn wide_property_keys(prefix: &str, width: usize) -> Vec<selene_core::DbString> {
    (0..width)
        .map(|idx| {
            db_string(&format!("{prefix}_{idx:04}")).expect("property key fits DB string cap")
        })
        .collect()
}

fn wide_compact_key_values(width: usize) -> (Vec<selene_core::DbString>, Vec<Option<Value>>) {
    (0..width)
        .rev()
        .map(|idx| {
            (
                db_string(&format!("compact_property_{idx:04}")).expect("key fits DB string cap"),
                Some(Value::Int(idx as i64)),
            )
        })
        .unzip()
}

fn wide_compact_key_values_sorted(
    width: usize,
) -> (Vec<selene_core::DbString>, Vec<Option<Value>>) {
    (0..width)
        .map(|idx| {
            (
                db_string(&format!("compact_property_{idx:04}")).expect("key fits DB string cap"),
                Some(Value::Int(idx as i64)),
            )
        })
        .unzip()
}

fn wide_labels(width: usize, reverse: bool) -> Vec<selene_core::DbString> {
    wide_labels_with_prefix("wide_label", width, reverse)
}

fn wide_labels_with_prefix(
    prefix: &str,
    width: usize,
    reverse: bool,
) -> Vec<selene_core::DbString> {
    let mut labels: Vec<_> = (0..width)
        .map(|idx| db_string(&format!("{prefix}_{idx:04}")).expect("label fits DB string cap"))
        .collect();
    if reverse {
        labels.reverse();
    }
    labels
}

// `print_stderr` deny is locally relaxed to surface the current `Value` size
// in bench output (so the CORE-06 shrink is visible run-to-run).
#[allow(clippy::print_stderr)]
fn bench_value_clone(c: &mut Criterion) {
    eprintln!(
        "[core_value_clone] size_of::<Value>() = {} bytes",
        std::mem::size_of::<Value>()
    );
    let mut group = c.benchmark_group("core_value_clone");

    let values = mixed_values();
    group.throughput(Throughput::Elements(N as u64));
    group.bench_function("vec_mixed_1024", |b| {
        b.iter(|| black_box(black_box(&values).clone()));
    });

    let map = mixed_property_map();
    group.bench_function("property_map_5", |b| {
        b.iter(|| black_box(black_box(&map).clone()));
    });

    json::bench_json_canonical(&mut group);

    let single_pair = single_property_pair();
    group.throughput(Throughput::Elements(1));
    group.bench_function("property_map_from_pairs_1", |b| {
        b.iter(|| {
            PropertyMap::from_pairs(std::iter::once(black_box(single_pair.clone())))
                .expect("property map fits core caps")
        });
    });

    let (compact_key, compact_value) = single_compact_key_value();
    group.bench_function("property_map_compact_1", |b| {
        b.iter(|| {
            PropertyMap::compact(
                std::iter::once(black_box(compact_key.clone())),
                std::iter::once(black_box(compact_value.clone())),
            )
            .expect("compact property map fits core caps")
        });
    });

    let compact_map = single_compact_property_map();
    group.bench_function("property_map_compact_logical_encode_1", |b| {
        b.iter(|| encode_map_logical(black_box(&compact_map)));
    });

    let pairs = wide_property_pairs(256);
    group.throughput(Throughput::Elements(pairs.len() as u64));
    group.bench_function("property_map_from_pairs_256_reverse", |b| {
        b.iter(|| {
            PropertyMap::from_pairs(black_box(pairs.iter().cloned()))
                .expect("property map fits core caps")
        });
    });
    let sorted_pairs = wide_property_pairs_sorted(256);
    group.bench_function("property_map_from_pairs_256_sorted", |b| {
        b.iter(|| {
            PropertyMap::from_pairs(black_box(sorted_pairs.iter().cloned()))
                .expect("property map fits core caps")
        });
    });
    let standard_map_256 =
        PropertyMap::from_pairs(sorted_pairs.iter().cloned()).expect("property map fits core caps");
    group.bench_function("property_map_standard_logical_encode_256", |b| {
        b.iter(|| encode_map_logical(black_box(&standard_map_256)));
    });
    let (compact_keys, compact_values) = wide_compact_key_values(256);
    group.bench_function("property_map_compact_256_reverse", |b| {
        b.iter(|| {
            PropertyMap::compact(
                black_box(compact_keys.iter().cloned()),
                black_box(compact_values.iter().cloned()),
            )
            .expect("compact property map fits core caps")
        });
    });
    let (compact_keys_sorted, compact_values_sorted) = wide_compact_key_values_sorted(256);
    group.bench_function("property_map_compact_256_sorted", |b| {
        b.iter(|| {
            PropertyMap::compact(
                black_box(compact_keys_sorted.iter().cloned()),
                black_box(compact_values_sorted.iter().cloned()),
            )
            .expect("compact property map fits core caps")
        });
    });
    let compact_map_256 = PropertyMap::compact(
        compact_keys_sorted.iter().cloned(),
        compact_values_sorted.iter().cloned(),
    )
    .expect("compact property map fits core caps");
    group.bench_function("property_map_compact_logical_encode_256", |b| {
        b.iter(|| encode_map_logical(black_box(&compact_map_256)));
    });

    group.finish();
}

fn bench_label_set(c: &mut Criterion) {
    let mut group = c.benchmark_group("core_label_set");
    let labels = wide_labels(100, true);
    group.throughput(Throughput::Elements(labels.len() as u64));
    group.bench_function("from_iter_100_reverse", |b| {
        b.iter(|| LabelSet::from_iter(black_box(labels.iter().cloned())));
    });
    let sorted_labels = wide_labels(100, false);
    group.bench_function("from_iter_100_sorted", |b| {
        b.iter(|| LabelSet::from_iter(black_box(sorted_labels.iter().cloned())));
    });
    group.finish();
}

fn bench_change_diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("core_change_diff");

    let single_pair = single_property_pair();
    group.throughput(Throughput::Elements(1));
    group.bench_function("property_diff_set_1", |b| {
        b.iter(|| {
            PropertyDiff::new(
                std::iter::once(black_box(single_pair.clone())),
                std::iter::empty::<selene_core::DbString>(),
            )
            .expect("one-property diff is valid")
        });
    });

    let pairs = wide_property_pairs(256);
    group.throughput(Throughput::Elements(pairs.len() as u64));
    group.bench_function("property_diff_set_256_reverse", |b| {
        b.iter(|| {
            PropertyDiff::new(
                black_box(pairs.iter().cloned()),
                std::iter::empty::<selene_core::DbString>(),
            )
            .expect("wide property diff is valid")
        });
    });
    let sorted_pairs = wide_property_pairs_sorted(256);
    group.bench_function("property_diff_set_256_sorted", |b| {
        b.iter(|| {
            PropertyDiff::new(
                black_box(sorted_pairs.iter().cloned()),
                std::iter::empty::<selene_core::DbString>(),
            )
            .expect("wide property diff is valid")
        });
    });
    let removed_properties = wide_property_keys("removed_property", 256);
    group.bench_function("property_diff_removed_256_sorted", |b| {
        b.iter(|| {
            PropertyDiff::new(
                std::iter::empty::<(selene_core::DbString, Value)>(),
                black_box(removed_properties.iter().cloned()),
            )
            .expect("wide property removal diff is valid")
        });
    });
    let disjoint_set_pairs = wide_property_pairs_sorted(128);
    let disjoint_removed_properties = wide_property_keys("disjoint_removed_property", 128);
    group.bench_function("property_diff_set_removed_128_each_sorted", |b| {
        b.iter(|| {
            PropertyDiff::new(
                black_box(disjoint_set_pairs.iter().cloned()),
                black_box(disjoint_removed_properties.iter().cloned()),
            )
            .expect("wide disjoint property diff is valid")
        });
    });
    let property_diff_256 =
        PropertyDiff::new(sorted_pairs.iter().cloned(), []).expect("wide property diff is valid");
    group.bench_function("property_diff_logical_encode_256_sorted", |b| {
        b.iter(|| encode_diff_logical(black_box(&property_diff_256)));
    });

    group.throughput(Throughput::Elements(100));
    for (name, reverse) in [
        ("label_diff_added_100_reverse", true),
        ("label_diff_added_100_sorted", false),
    ] {
        let labels = wide_labels(100, reverse);
        group.bench_function(name, |b| {
            b.iter(|| {
                LabelDiff::new(black_box(labels.iter().cloned()), [])
                    .expect("wide label diff is valid")
            });
        });
    }
    let removed_labels = wide_labels_with_prefix("removed_label", 100, false);
    group.bench_function("label_diff_removed_100_sorted", |b| {
        b.iter(|| {
            LabelDiff::new([], black_box(removed_labels.iter().cloned()))
                .expect("wide removed label diff is valid")
        });
    });
    let add_labels = wide_labels_with_prefix("added_label", 50, false);
    let remove_labels = wide_labels_with_prefix("removed_label", 50, false);
    group.bench_function("label_diff_added_removed_50_each_sorted", |b| {
        b.iter(|| {
            LabelDiff::new(
                black_box(add_labels.iter().cloned()),
                black_box(remove_labels.iter().cloned()),
            )
            .expect("wide disjoint label diff is valid")
        });
    });
    let sorted_labels = wide_labels(100, false);
    let label_diff_100 =
        LabelDiff::new(sorted_labels.iter().cloned(), []).expect("wide label diff is valid");
    group.bench_function("label_diff_postcard_encode_100_sorted", |b| {
        b.iter(|| {
            postcard::to_allocvec(black_box(&label_diff_100)).expect("label diff serializes")
        });
    });

    group.finish();
}

fn vector_components(dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|idx| (((idx * 31) % 1_021) as f32 - 510.0) / 256.0)
        .collect()
}

criterion_group! {
    name = value_clone;
    config = bench_config();
    targets = bench_value_clone, bench_label_set, bench_change_diff, numeric::bench_numeric_keys, vector::bench_vector_value, vector::bench_vector_distance, vector::bench_vector_exact_top_k, vector::bench_vector_gpu_baseline
}
criterion_main!(value_clone);
