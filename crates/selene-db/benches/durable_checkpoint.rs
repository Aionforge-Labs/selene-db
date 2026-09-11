#![allow(missing_docs, clippy::print_stdout, clippy::print_stderr)]
//! Sequential consumer-shaped checkpoint/open and isolated native RSS measurements.
#[cfg(not(selene_bench_system_alloc))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use criterion::{BenchmarkId, Criterion};
use selene_db::{Database, TransactionAccessMode};
use std::{
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};
#[path = "durable_checkpoint/fixture.rs"]
mod fixture;
#[path = "durable_checkpoint/lifecycle.rs"]
mod lifecycle;
use fixture::*;

fn witness(rows: usize, count: usize, indexes: bool) -> Duration {
    let (dir, db) = fixture(rows, indexes);
    verify(&db, rows, 0);
    let start = Instant::now();
    let checkpoint = db.checkpoint().unwrap();
    let checkpoint_time = start.elapsed();
    suffix(&db, count);
    verify(&db, rows, count as i64);
    let before = db.durable_status().unwrap();
    drop(db);
    let start = Instant::now();
    let db = Database::open(dir.path()).unwrap();
    let open = start.elapsed();
    assert_eq!(db.durable_status().unwrap(), before);
    verify(&db, rows, count as i64);
    let info = db.recovery_info().unwrap();
    assert_eq!(info.replayed_suffix_records, count as u64);
    assert_eq!(info.rebuilt_indexes, if indexes { GRAPHS * 4 } else { 0 });
    println!(
        "CHECKPOINT rows_per_graph={rows} graphs={GRAPHS} suffix={count} indexes={indexes} checkpoint_us={:.3} write_reservation_us={:.3} snapshot_bytes={} snapshot_digest={:02x?} first_open_us={:.3} snapshot_load_decode_us={:.3} prefix_and_suffix_us={:.3} eager_rebuild_us={:.3} sync_us={:.3} prefix_records={} suffix_records={} verified_wal_bytes={} total_storage_bytes={}",
        checkpoint_time.as_secs_f64() * 1e6,
        checkpoint.write_reservation_elapsed.as_secs_f64() * 1e6,
        checkpoint.bytes,
        checkpoint.digest,
        open.as_secs_f64() * 1e6,
        info.snapshot_elapsed.as_secs_f64() * 1e6,
        info.wal_elapsed.as_secs_f64() * 1e6,
        info.rebuild_elapsed.as_secs_f64() * 1e6,
        info.synchronize_elapsed.as_secs_f64() * 1e6,
        info.verified_prefix_records,
        info.replayed_suffix_records,
        info.position.offset,
        storage(dir.path())
    );
    open
}

fn mixed(rows: usize) {
    let (dir, db) = fixture(rows, true);
    let held = db.session(&path(0)).unwrap();
    held.start_transaction(TransactionAccessMode::ReadOnly)
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let ready = barrier.clone();
    let writer_db = db.clone();
    let writer = thread::spawn(move || {
        let s = writer_db.session(&path(0)).unwrap();
        let mut writes = Vec::new();
        ready.wait();
        // Exactly 60 reads / 40 writes; responses are checked, not fire-and-forget.
        for i in 0..100 {
            if i % 5 < 3 {
                assert_eq!(
                    s.execute("MATCH (n:Doc) RETURN n.id").unwrap().row_count(),
                    Some(rows)
                );
            } else {
                let start = Instant::now();
                s.execute("MATCH (n:Doc) WHERE n.id = 0 SET n.n = n.n + 1")
                    .unwrap();
                writes.push(start.elapsed());
            }
        }
        writes
    });
    barrier.wait();
    for i in 0..3 {
        let checkpoint = db.checkpoint().unwrap();
        println!(
            "GROWTH rows_per_graph={rows} checkpoint={i} retained_storage_bytes={} snapshot_bytes={} reservation_us={:.3}",
            storage(dir.path()),
            checkpoint.bytes,
            checkpoint.write_reservation_elapsed.as_secs_f64() * 1e6
        );
    }
    let mut writes = writer.join().unwrap();
    writes.sort_unstable();
    assert_eq!(
        held.execute("MATCH (n:Doc) WHERE n.n = 0 RETURN n")
            .unwrap()
            .row_count(),
        Some(rows)
    );
    verify(&db, rows, 40);
    println!(
        "MIXED rows_per_graph={rows} graphs={GRAPHS} reads=60 writes=40 checkpoints=3 held_reader=true foreground_write_p50_us={:.3} p95_us={:.3} max_us={:.3}",
        writes[19].as_secs_f64() * 1e6,
        writes[37].as_secs_f64() * 1e6,
        writes[39].as_secs_f64() * 1e6
    );
    drop(held);
    drop(db);
    let db = Database::open(dir.path()).unwrap();
    verify(&db, rows, 40);
}

fn rss_child(rows: usize, hold: bool) {
    let (dir, db) = fixture(rows, true);
    let reader = hold.then(|| {
        let s = db.session(&path(0)).unwrap();
        s.start_transaction(TransactionAccessMode::ReadOnly)
            .unwrap();
        s
    });
    for round in 0..3 {
        for graph in 0..GRAPHS {
            db.session(&path(graph))
                .unwrap()
                .execute(&format!(
                    "MATCH (n:Doc) SET n.text = 'generation {round} {}'",
                    "retained memory ".repeat(32)
                ))
                .unwrap();
        }
        let checkpoint = db.checkpoint().unwrap();
        println!(
            "RSS_WORKLOAD rows_per_graph={rows} graphs={GRAPHS} held_reader={hold} checkpoint={round} snapshot_bytes={} total_storage_bytes={}",
            checkpoint.bytes,
            storage(dir.path())
        );
    }
    if let Some(reader) = &reader {
        assert_eq!(
            reader
                .execute("MATCH (n:Doc) WHERE n.text STARTS WITH 'alpha' RETURN n")
                .unwrap()
                .row_count(),
            Some(rows)
        );
    }
    verify(&db, rows, 0);
    // /usr/bin/time measures native process peak RSS including setup, snapshots,
    // preflight and indexes. It is not allocator accounting or a held-reader-only delta.
}

fn main() {
    if let Ok(mode) = std::env::var("SELENE_CHECKPOINT_RSS_CHILD") {
        let (rows, hold) = mode.split_once(':').unwrap();
        rss_child(rows.parse().unwrap(), hold == "held");
        return;
    }
    let mut c = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(50))
        .measurement_time(Duration::from_millis(100))
        .configure_from_args();
    for rows in [32, 256] {
        for _ in 0..3 {
            lifecycle::witness(rows);
        }
        for indexes in [false, true] {
            for count in [1, 16] {
                let (dir, db) = fixture(rows, indexes);
                db.checkpoint().unwrap();
                suffix(&db, count);
                drop(db);
                c.bench_with_input(
                    BenchmarkId::new(
                        "format2_open",
                        format!("g3_n{rows}_suffix{count}_indexes{indexes}"),
                    ),
                    &(),
                    |b, _| {
                        b.iter_custom(|iterations| {
                            let mut elapsed = Duration::ZERO;
                            for _ in 0..iterations {
                                let start = Instant::now();
                                let db = Database::open(dir.path()).unwrap();
                                elapsed += start.elapsed();
                                verify(&db, rows, count as i64);
                                drop(db);
                            }
                            elapsed
                        })
                    },
                );
                for _ in 0..3 {
                    witness(rows, count, indexes);
                }
            }
        }
        mixed(rows);
        for mode in ["released", "held"] {
            println!(
                "ISOLATED_NATIVE_RSS rows_per_graph={rows} reader={mode} command=/usr/bin/time -l <this-benchmark>"
            );
            let output = std::process::Command::new("/usr/bin/time")
                .arg(if cfg!(target_os = "macos") {
                    "-l"
                } else {
                    "-v"
                })
                .arg(std::env::current_exe().unwrap())
                .env("SELENE_CHECKPOINT_RSS_CHILD", format!("{rows}:{mode}"))
                .output()
                .unwrap();
            print!("{}", String::from_utf8_lossy(&output.stdout));
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success());
        }
    }
    c.final_summary();
}
