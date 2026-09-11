//! Serial full-readiness measurements; no logical-charge-as-RSS claims.
use super::*;

pub fn run() {
    if let Ok(mode) = std::env::var("SELENE_RECOVERY_RSS_CHILD") {
        let dir = std::env::var_os("SELENE_RECOVERY_RSS_DIR").unwrap();
        if mode == "verify" {
            std::hint::black_box(Database::verify(&dir).unwrap());
        } else {
            std::hint::black_box(Database::open(&dir).unwrap());
        }
        return;
    }
    let mut c = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(50))
        .measurement_time(Duration::from_millis(100))
        .configure_from_args();
    for (graphs, rows) in [(3, 32), (3, 128), (12, 128)] {
        for suffix in [1, 16, 128] {
            let (dir, db) = fixture_graphs(rows, true, graphs);
            db.checkpoint().unwrap();
            for i in 0..suffix {
                db.session(&path(i % graphs))
                    .unwrap()
                    .execute("MATCH (n:Doc) WHERE n.id = 0 SET n.n = n.n + 1")
                    .unwrap();
            }
            verify_graphs(&db, rows, suffix as i64, graphs);
            let position = db.durable_status().unwrap().position;
            drop(db);
            for sample in 0..3 {
                let start = Instant::now();
                let report = Database::verify(dir.path()).unwrap();
                let verify_us = start.elapsed().as_secs_f64() * 1e6;
                assert_eq!(
                    (report.graphs, report.nodes, report.recovery.rebuilt_indexes),
                    (graphs, graphs * rows, graphs * 4)
                );
                assert_eq!(report.recovery.position, position);
                let start = Instant::now();
                let db = Database::open(dir.path()).unwrap();
                let open_us = start.elapsed().as_secs_f64() * 1e6;
                verify_graphs(&db, rows, suffix as i64, graphs);
                println!(
                    "RECOVERY graphs={graphs} rows={rows} suffix={suffix} sample={sample} verify_us={verify_us:.3} open_us={open_us:.3} selection_us={:.3} snapshot_us={:.3} framing_us={:.3} semantic_us={:.3} rebuild_us={:.3} snapshot_bytes={} wal_bytes={} indexes={}",
                    report.selection_elapsed.as_secs_f64() * 1e6,
                    report.recovery.snapshot_elapsed.as_secs_f64() * 1e6,
                    report.framing_elapsed.as_secs_f64() * 1e6,
                    report.semantic_replay_elapsed.as_secs_f64() * 1e6,
                    report.recovery.rebuild_elapsed.as_secs_f64() * 1e6,
                    report.snapshot_bytes,
                    report.captured_wal_bytes,
                    report.recovery.rebuilt_indexes
                );
                drop(db);
            }
            for mode in ["verify", "open"] {
                c.bench_function(
                    &format!("format2_readiness/{mode}_g{graphs}_n{rows}_suffix{suffix}"),
                    |b| {
                        b.iter(|| {
                            if mode == "verify" {
                                std::hint::black_box(Database::verify(dir.path()).unwrap());
                            } else {
                                std::hint::black_box(Database::open(dir.path()).unwrap());
                            }
                        })
                    },
                );
                if graphs == 12 && suffix == 128 {
                    let output = std::process::Command::new("/usr/bin/time")
                        .arg(if cfg!(target_os = "macos") {
                            "-l"
                        } else {
                            "-v"
                        })
                        .arg(std::env::current_exe().unwrap())
                        .env("SELENE_RECOVERY_RSS_CHILD", mode)
                        .env("SELENE_RECOVERY_RSS_DIR", dir.path())
                        .output()
                        .unwrap();
                    println!(
                        "RECOVERY_RSS mode={mode} graphs={graphs} rows={rows} suffix={suffix}"
                    );
                    print!("{}", String::from_utf8_lossy(&output.stdout));
                    eprint!("{}", String::from_utf8_lossy(&output.stderr));
                    assert!(output.status.success());
                }
            }
            // Complete snapshot corruption: constant-size mutation, reject before
            // semantic materialization. Timings include real selected control I/O.
            let report = Database::verify(dir.path()).unwrap();
            let snapshot = dir.path().join(report.snapshot);
            let mut bytes = std::fs::read(&snapshot).unwrap();
            bytes[128] ^= 1;
            std::fs::write(snapshot, bytes).unwrap();
            c.bench_function(
                &format!("format2_readiness/reject_g{graphs}_n{rows}_suffix{suffix}"),
                |b| {
                    b.iter(|| {
                        let error = Database::verify(dir.path()).unwrap_err();
                        assert_eq!(error.kind, selene_db::StorageErrorKind::Integrity);
                        std::hint::black_box(error);
                    })
                },
            );
        }
    }
    c.final_summary();
}
