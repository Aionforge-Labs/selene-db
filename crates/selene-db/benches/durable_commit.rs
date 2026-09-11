#![allow(missing_docs, clippy::print_stdout)]
//! Real-file stream groups and actual facade acknowledgments; no background queue.

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use selene_db::benchmark::measure_commits;
use selene_persist::{
    StoreDirectory,
    control::{CompatibilityIdentity, EmptyStoreControl},
    logical_frame::Compression,
    logical_stream::{LogicalReader, LogicalWal},
};
use std::time::{Duration, Instant};

fn identity() -> CompatibilityIdentity {
    CompatibilityIdentity::new("stream-benchmark", 1, [8; 32], [17, 0, 0], "binary", 1).unwrap()
}

fn stream(groups: u64, size: usize, tails: bool) -> (Duration, Vec<Duration>, u64) {
    let temp = tempfile::tempdir().unwrap();
    let dir = StoreDirectory::open(temp.path()).unwrap();
    let mut wal =
        LogicalWal::create(EmptyStoreControl::create_empty(&dir, identity()).unwrap()).unwrap();
    let mut samples = Vec::new();
    let mut elapsed = Duration::ZERO;
    let mut visible = 0u64;
    for _ in 0..groups {
        // Simultaneous bounded group admission: each member shares this start.
        let start = Instant::now();
        let bodies: Vec<_> = (0..size)
            .map(|i| {
                let mut body = [42; 256];
                body[..8].copy_from_slice(&(visible + i as u64 + 1).to_le_bytes());
                body
            })
            .collect();
        let refs: Vec<_> = bodies.iter().map(|body| body.as_slice()).collect();
        let group = wal.prepare(&refs, Compression::Raw, 1024).unwrap();
        let position = wal
            .commit(
                group,
                || false,
                |publication| {
                    visible += size as u64;
                    publication.mark_published();
                    Ok(())
                },
            )
            .unwrap();
        let duration = start.elapsed();
        elapsed += duration;
        if tails {
            samples.extend(std::iter::repeat_n(duration, size));
        }
        assert_eq!(position.sequence, visible);
    }
    let bytes = wal.progress().synchronized.offset;
    assert_eq!(
        wal.progress().acknowledged,
        Some(wal.progress().synchronized)
    );
    drop(wal);
    let mut reader = LogicalReader::open(&dir, &identity(), 1024).unwrap();
    let mut replayed = 0u64;
    while let Some(body) = reader.next_body().unwrap() {
        replayed += 1;
        assert_eq!(body.len(), 256);
        assert_eq!(&body[..8], &replayed.to_le_bytes());
    }
    assert_eq!(replayed, visible);
    assert!(!reader.incomplete_tail());
    (elapsed, samples, bytes)
}

fn report(label: &str, samples: &mut [Duration], elapsed: Duration, bytes: u64) {
    samples.sort_unstable();
    let percentile = |p: usize| samples[(samples.len() * p).div_ceil(100) - 1].as_secs_f64() * 1e6;
    println!(
        "ACK {label} n={} p50_us={:.3} p95_us={:.3} p99_us={:.3} min_us={:.3} max_us={:.3} ack_per_s={:.1} bytes={bytes}",
        samples.len(),
        percentile(50),
        percentile(95),
        percentile(99),
        samples[0].as_secs_f64() * 1e6,
        samples[samples.len() - 1].as_secs_f64() * 1e6,
        samples.len() as f64 / elapsed.as_secs_f64()
    );
}

fn measurements(c: &mut Criterion) {
    let mut group = c.benchmark_group("format2_commit");
    for size in [1usize, 4, 16, 32] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_function(BenchmarkId::new("durable_stream_group", size), |b| {
            b.iter_custom(|iterations| stream(iterations, size, false).0);
        });
        let (elapsed, mut samples, bytes) = stream(256, size, true);
        report(
            &format!("durable_stream_group/{size}"),
            &mut samples,
            elapsed,
            bytes,
        );
    }
    group.throughput(Throughput::Elements(1));
    for initial in [64, 1024] {
        for named in [false, true] {
            let label = format!("durable_facade_{}", if named { "named" } else { "unbound" });
            group.bench_function(BenchmarkId::new(&label, initial), |b| {
                b.iter_custom(|mut iterations| {
                    let mut elapsed = Duration::ZERO;
                    while iterations != 0 {
                        let count = iterations.min(64) as usize;
                        let temp = tempfile::tempdir().unwrap();
                        let dir = StoreDirectory::open(temp.path()).unwrap();
                        elapsed += measure_commits(&dir, named, initial, count)
                            .latency
                            .iter()
                            .sum::<Duration>();
                        iterations -= count as u64;
                    }
                    elapsed
                });
            });
            let temp = tempfile::tempdir().unwrap();
            let dir = StoreDirectory::open(temp.path()).unwrap();
            let mut measured = measure_commits(&dir, named, initial, 256);
            let elapsed = measured.latency.iter().sum();
            report(
                &format!("{label}/{initial}"),
                &mut measured.latency,
                elapsed,
                measured.bytes,
            );
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let dir = StoreDirectory::open(temp.path()).unwrap();
    let wal =
        LogicalWal::create(EmptyStoreControl::create_empty(&dir, identity()).unwrap()).unwrap();
    group.bench_function("buffered_prepare_only_no_append_or_ack", |b| {
        b.iter(|| {
            std::hint::black_box(wal.prepare(&[&[42; 256]], Compression::Raw, 1024).unwrap());
        })
    });
    assert_eq!(wal.progress().written.sequence, 0);
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10).warm_up_time(Duration::from_millis(100)).measurement_time(Duration::from_millis(500));
    targets = measurements
}
criterion_main!(benches);
