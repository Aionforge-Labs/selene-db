//! Full logical transaction bytes/CPU and bounded rejection. No append or fsync.

use criterion::{BenchmarkId, Throughput};
use selene_core::{Change, Value, db_string, logical::Limits};
use selene_graph::logical_transaction::LogicalTransaction;
use selene_persist::logical_frame::{self as frame, Boundary, Compression, Decoded};
use std::{hint::black_box, io::Write, process::Command};

mod common;
#[path = "logical_wal/fixtures.rs"]
mod fixtures;

#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn encode(tx: &LogicalTransaction, policy: Compression) -> Vec<u8> {
    let body = tx.encode(Limits::default()).unwrap();
    frame::encode(&body, fixtures::context(), policy, frame::MAX_PAYLOAD).unwrap()
}
fn decode(bytes: &[u8]) -> LogicalTransaction {
    let Decoded::Complete { body, .. } = frame::decode(
        bytes,
        fixtures::context(),
        Boundary::SealedEnd,
        frame::MAX_PAYLOAD,
    )
    .unwrap() else {
        panic!("complete frame")
    };
    LogicalTransaction::decode(&body, Limits::default()).unwrap()
}
fn rss() -> i64 {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    assert!(output.status.success());
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse::<i64>()
        .unwrap()
        * 1024
}
fn memory_child(spec: &str) {
    let (shape, count) = spec.split_once(':').unwrap();
    let tx = fixtures::transaction(shape, count.parse().unwrap());
    let bytes = encode(&tx, Compression::Auto);
    drop(tx);
    let before = rss();
    let Decoded::Complete { body, .. } = frame::decode(
        &bytes,
        fixtures::context(),
        Boundary::SealedEnd,
        frame::MAX_PAYLOAD,
    )
    .unwrap() else {
        panic!("complete")
    };
    let (decoded, charged, items) =
        LogicalTransaction::decode_accounted(&body, Limits::default()).unwrap();
    writeln!(std::io::stdout().lock(), "memory {spec} frame_capacity={} expanded_owned={} charged_bytes={charged} items={items} rss={} rss_delta={}",
        bytes.capacity(), if matches!(body, std::borrow::Cow::Owned(_)) { body.len() } else { 0 }, rss(), rss() - before).unwrap();
    black_box(decoded);
}
fn main() {
    if let Ok(spec) = std::env::var("SELENE_LOGICAL_MEMORY_CHILD") {
        memory_child(&spec);
        return;
    }
    let mut criterion = common::criterion_config().configure_from_args();
    let mut group = criterion.benchmark_group("logical_wal");
    for shape in ["scalar", "vector", "json", "list", "record", "catalog"] {
        for count in [1, 64, 1024] {
            if shape == "catalog" && count == 1024 {
                continue;
            }
            let tx = fixtures::transaction(shape, count);
            let raw = encode(&tx, Compression::Raw);
            let auto = encode(&tx, Compression::Auto);
            let body = tx.encode(Limits::default()).unwrap();
            let (_, charged, items) =
                LogicalTransaction::decode_accounted(&body, Limits::default()).unwrap();
            writeln!(std::io::stdout().lock(), "bytes {shape}/{count} raw={} auto={} body={} overhead={} charged_bytes={charged} items={items}", raw.len(), auto.len(), body.len(), frame::FRAME_OVERHEAD).unwrap();
            assert_eq!(decode(&raw), tx);
            assert_eq!(decode(&auto), tx);
            group.throughput(Throughput::Bytes(body.len() as u64));
            for (name, policy, encoded) in [
                ("raw", Compression::Raw, &raw),
                ("auto", Compression::Auto, &auto),
            ] {
                group.bench_function(
                    BenchmarkId::new(format!("{shape}_{name}_encode"), count),
                    |b| b.iter(|| black_box(encode(black_box(&tx), policy))),
                );
                group.bench_function(
                    BenchmarkId::new(format!("{shape}_{name}_decode"), count),
                    |b| b.iter(|| black_box(decode(black_box(encoded)))),
                );
            }
        }
    }
    for size in [4095, 4096, 8192] {
        let mut tx = fixtures::transaction("threshold", 1);
        let original = tx.encode(Limits::default()).unwrap().len();
        let padding = vec![b'x'; size - original];
        if let Change::NodeCreated { properties, .. } = &mut tx.graphs[0].changes[0] {
            properties
                .set(db_string("payload").unwrap(), Value::Bytes(padding.into()))
                .unwrap();
        }
        let body = tx.encode(Limits::default()).unwrap();
        assert_eq!(body.len(), size);
        let encoded = encode(&tx, Compression::Auto);
        writeln!(
            std::io::stdout().lock(),
            "threshold body={size} frame={} codec={}",
            encoded.len(),
            encoded[12]
        )
        .unwrap();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(BenchmarkId::new("threshold_auto_encode", size), |b| {
            b.iter(|| black_box(encode(black_box(&tx), Compression::Auto)))
        });
        group.bench_function(BenchmarkId::new("threshold_auto_decode", size), |b| {
            b.iter(|| black_box(decode(black_box(&encoded))))
        });
    }
    // Isolate the cost of attempting compression and retaining RAW for entropy.
    // These two framing-only rows are not semantic transaction throughput rows.
    for size in [4096, 65_536] {
        let mut entropy = vec![0; size];
        blake3::Hasher::new().finalize_xof().fill(&mut entropy);
        let encoded = frame::encode(
            &entropy,
            fixtures::context(),
            Compression::Auto,
            frame::MAX_PAYLOAD,
        )
        .unwrap();
        assert_eq!(encoded[12], 0);
        writeln!(
            std::io::stdout().lock(),
            "incompressible body={size} frame={} codec=0",
            encoded.len()
        )
        .unwrap();
        group.throughput(Throughput::Bytes(size as u64));
        for (name, policy) in [("raw", Compression::Raw), ("auto", Compression::Auto)] {
            group.bench_function(
                BenchmarkId::new(format!("frame_incompressible_{name}"), size),
                |b| {
                    b.iter(|| {
                        black_box(
                            frame::encode(
                                black_box(&entropy),
                                fixtures::context(),
                                policy,
                                frame::MAX_PAYLOAD,
                            )
                            .unwrap(),
                        )
                    })
                },
            );
        }
    }
    // Maximum permitted encoded integrity work: the whole 256 MiB is checked
    // before the invalid semantic body version is rejected. This is not fsync.
    let maximum = frame::encode(
        &vec![0; frame::MAX_PAYLOAD],
        fixtures::context(),
        Compression::Raw,
        frame::MAX_PAYLOAD,
    )
    .unwrap();
    group.throughput(Throughput::Bytes(frame::MAX_PAYLOAD as u64));
    group.bench_function("reject_complete_maximum_256mib", |b| {
        b.iter(|| {
            let Decoded::Complete { body, .. } = frame::decode(
                black_box(&maximum),
                fixtures::context(),
                Boundary::SealedEnd,
                frame::MAX_PAYLOAD,
            )
            .unwrap() else {
                panic!("complete")
            };
            assert!(LogicalTransaction::decode(&body, Limits::default()).is_err());
        })
    });
    let mut oversized = maximum[..160].to_vec();
    oversized[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    let hash = blake3::hash(&oversized[..128]);
    oversized[128..160].copy_from_slice(hash.as_bytes());
    group.throughput(Throughput::Elements(1));
    group.bench_function("reject_oversized_header", |b| {
        b.iter(|| {
            assert!(
                frame::decode(
                    black_box(&oversized),
                    fixtures::context(),
                    Boundary::UnsealedEnd,
                    frame::MAX_PAYLOAD
                )
                .is_err()
            )
        })
    });
    group.finish();
    for shape in ["scalar", "vector", "json", "list", "record", "catalog"] {
        let count = if shape == "catalog" { 64 } else { 1024 };
        let output = Command::new(std::env::current_exe().unwrap())
            .env("SELENE_LOGICAL_MEMORY_CHILD", format!("{shape}:{count}"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "memory child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::io::stdout().write_all(&output.stdout).unwrap();
    }
    criterion.final_summary();
}
