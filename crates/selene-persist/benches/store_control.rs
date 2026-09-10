#![allow(missing_docs)]
//! Empty-control costs with real native synchronization and isolated setup.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod common;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use selene_persist::{
    StoreDirectory, StoreWriter,
    control::{CompatibilityIdentity, EmptyStoreControl},
};

fn identity() -> CompatibilityIdentity {
    CompatibilityIdentity::new("benchmark-fixture", 1, [7; 32], [16, 0, 0], "binary", 1).unwrap()
}

fn bench_store_control(c: &mut Criterion) {
    let mut group = c.benchmark_group("persist_store_control");
    let root = common::TempDir::new("anchor");
    group.bench_function("anchor", |b| {
        b.iter(|| {
            std::hint::black_box(StoreDirectory::open(root.path()).unwrap());
        })
    });
    let dir = StoreDirectory::open(root.path()).unwrap();
    drop(EmptyStoreControl::create_empty(&dir, identity()).unwrap());
    let expected = identity();
    group.bench_function("open_empty", |b| {
        b.iter(|| {
            std::hint::black_box(EmptyStoreControl::open(&dir, &expected).unwrap());
        })
    });
    group.bench_function("create_empty", |b| {
        b.iter_batched(
            || {
                let root = common::TempDir::new("create");
                let dir = StoreDirectory::open(root.path()).unwrap();
                (root, dir)
            },
            |(root, dir)| {
                let control = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
                (control, root) // Drop handles/fixture outside the measured interval.
            },
            BatchSize::PerIteration,
        )
    });
    group.bench_function("publish_empty", |b| {
        b.iter_batched(
            || {
                let root = common::TempDir::new("publish");
                let dir = StoreDirectory::open(root.path()).unwrap();
                let control = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
                (root, control)
            },
            |(root, mut control)| {
                assert_eq!(control.publish_empty().unwrap().get(), 2);
                (control, root)
            },
            BatchSize::PerIteration,
        )
    });
    group.finish();
}

fn bench_control_history(c: &mut Criterion) {
    let mut group = c.benchmark_group("persist_store_control_history");
    let expected = identity();
    for generation in [1_u64, 32, 256] {
        let root = common::TempDir::new("control-history");
        let dir = StoreDirectory::open(root.path()).unwrap();
        let mut store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
        for _ in 1..generation {
            store.publish_empty().unwrap();
        }
        drop(store);
        group.bench_function(BenchmarkId::new("retained", generation), |b| {
            b.iter(|| {
                std::hint::black_box(EmptyStoreControl::open(&dir, &expected).unwrap());
            })
        });
        // Offline fixture pruning, not a production retention API. All readers
        // have finished; retain writer ownership and sync the removed entries.
        let owner = StoreWriter::acquire(&dir).unwrap();
        for old in 1..generation {
            std::fs::remove_file(root.path().join(format!("MANIFEST-{old:020}.control"))).unwrap();
        }
        dir.sync().unwrap();
        drop(owner);
        group.bench_function(BenchmarkId::new("pruned", generation), |b| {
            b.iter(|| {
                std::hint::black_box(EmptyStoreControl::open(&dir, &expected).unwrap());
            })
        });
    }
    group.finish();
}

criterion_group! {
    name = store_control;
    config = common::criterion_config();
    targets = bench_store_control, bench_control_history
}
criterion_main!(store_control);
