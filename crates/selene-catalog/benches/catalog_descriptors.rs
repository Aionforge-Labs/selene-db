#![allow(missing_docs)]
//! Canonical-name lookup, immutable snapshot clone/read, and structural memory
//! accounting for representative flat catalogs.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::{hint::black_box, time::Duration};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use selene_catalog::{
    CatalogDescriptor, CatalogGeneration, CatalogId, CatalogName, CatalogObjectId, CatalogSnapshot,
    CatalogSnapshotBuilder, CreationMetadata, DirectoryId, GraphId, SchemaId,
};

#[path = "catalog_descriptors/declarations.rs"]
mod catalog_declarations;

fn generation() -> CatalogGeneration {
    CatalogGeneration::new(1).expect("benchmark generation is nonzero")
}

fn name(value: impl Into<String>) -> CatalogName {
    CatalogName::regular(value).expect("benchmark names are regular identifiers")
}

fn snapshot(object_count: usize) -> CatalogSnapshot {
    let catalog_id = CatalogId::new(1).unwrap();
    let root_id = DirectoryId::new(1).unwrap();
    let schema_id = SchemaId::new(1).unwrap();
    let creation = CreationMetadata::new(generation(), None);
    let catalog =
        CatalogDescriptor::catalog(catalog_id, name("selene"), generation(), creation.clone())
            .unwrap();
    let root =
        CatalogDescriptor::root_directory(root_id, catalog_id, generation(), creation.clone())
            .unwrap();
    let mut builder = CatalogSnapshotBuilder::new(generation(), catalog, root).unwrap();
    builder
        .insert(
            CatalogDescriptor::schema(
                schema_id,
                name("public"),
                root_id,
                generation(),
                creation.clone(),
            )
            .unwrap(),
        )
        .unwrap();
    for raw in 1..=object_count {
        builder
            .insert(
                CatalogDescriptor::graph(
                    GraphId::new(raw as u64).unwrap(),
                    name(format!("object_{raw:05}")),
                    schema_id,
                    generation(),
                    creation.clone(),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
    }
    builder.build().unwrap()
}

fn scales() -> &'static [usize] {
    match std::env::var("SELENE_BENCH_PROFILE").ok().as_deref() {
        Some("full") | Some("stress") => &[100, 1_000, 10_000],
        _ => &[100, 1_000],
    }
}

fn criterion_config() -> Criterion {
    let (samples, measurement_ms) = match std::env::var("SELENE_BENCH_PROFILE").ok().as_deref() {
        Some("full") | Some("stress") => (30, 1_500),
        _ => (10, 500),
    };
    Criterion::default()
        .sample_size(samples)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(measurement_ms))
}

#[allow(clippy::print_stderr)]
fn bench_catalog_descriptors(c: &mut Criterion) {
    let mut lookup = c.benchmark_group("catalog_descriptor/canonical_name_lookup");
    for &object_count in scales() {
        let snapshot = snapshot(object_count);
        let query = name(format!("object_{:05}", object_count / 2));
        let schema_id = SchemaId::new(1).unwrap();
        let memory = snapshot.memory_accounting();
        eprintln!(
            "[catalog_descriptor_memory] objects={object_count} descriptors={} descriptor_accounted_bytes={} bytes_per_descriptor={:.2} dictionary_entries={} dictionary_accounted_bytes={} bytes_per_dictionary_entry={:.2} exclusions=declaration_payload_heap,allocator_metadata,btreemap_node_slack,arc_control_blocks",
            memory.descriptor_count(),
            memory.descriptor_bytes(),
            memory.descriptor_bytes() as f64 / memory.descriptor_count() as f64,
            memory.dictionary_entry_count(),
            memory.dictionary_bytes(),
            memory.dictionary_bytes() as f64 / memory.dictionary_entry_count() as f64,
        );
        lookup.throughput(Throughput::Elements(1));
        lookup.bench_with_input(
            BenchmarkId::from_parameter(object_count),
            &object_count,
            |b, _| {
                b.iter(|| {
                    black_box(
                        snapshot
                            .schema_object(schema_id, black_box(&query))
                            .expect("benchmark object exists"),
                    )
                });
            },
        );
    }
    lookup.finish();

    let mut snapshot_group = c.benchmark_group("catalog_descriptor/snapshot");
    for &object_count in scales() {
        let snapshot = snapshot(object_count);
        let target = CatalogObjectId::Graph(GraphId::new((object_count / 2) as u64).unwrap());
        snapshot_group.bench_with_input(
            BenchmarkId::new("clone_arc", object_count),
            &object_count,
            |b, _| b.iter(|| black_box(black_box(&snapshot).clone())),
        );
        snapshot_group.bench_with_input(
            BenchmarkId::new("read_by_id", object_count),
            &object_count,
            |b, _| {
                b.iter(|| {
                    black_box(
                        snapshot
                            .descriptor(black_box(target))
                            .expect("benchmark object exists"),
                    )
                });
            },
        );
    }
    snapshot_group.finish();
}

criterion_group! {
    name = catalog_descriptors;
    config = criterion_config();
    targets = bench_catalog_descriptors, catalog_declarations::bench
}
criterion_main!(catalog_descriptors);
