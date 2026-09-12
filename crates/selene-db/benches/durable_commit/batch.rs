//! Separate staging, synchronized acknowledgment, and rollback-cleanup costs.

use criterion::{Criterion, Throughput};
use selene_db::{CreatePolicy, Database, ObjectPath, SchemaPath, Session};
use std::time::{Duration, Instant};

const ROWS: usize = 2049;
const INSERT: &str = "MATCH (s:Seed) INSERT (:Item {id: s.k})";

fn fixture() -> (tempfile::TempDir, Database, Session) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::create(dir.path()).unwrap();
    let graph = ObjectPath::regular("selene", "batch", "data").unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "batch").unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    db.catalog()
        .create_graph(&graph, None, CreatePolicy::Strict)
        .unwrap();
    let session = db.session(&graph).unwrap();
    session.execute("INSERT (:Seed {k: 0})").unwrap();
    for shift in 0..11 {
        session
            .execute(&format!(
                "MATCH (s:Seed) INSERT (:Seed {{k: s.k + {}}})",
                1 << shift,
            ))
            .unwrap();
    }
    session.execute("INSERT (:Seed {k: 2048})").unwrap();
    (dir, db, session)
}

pub(super) fn measurements(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_mutation_2049");
    group.throughput(Throughput::Elements(ROWS as u64));
    for phase in ["stage_no_ack", "durable_commit_ack", "rollback_cleanup"] {
        let (_dir, _db, session) = fixture();
        group.bench_function(phase, |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    session.execute("START TRANSACTION").unwrap();
                    let stage = Instant::now();
                    let staged = session.execute(INSERT).unwrap();
                    let stage_elapsed = stage.elapsed();
                    assert_eq!(staged.write_summary().unwrap().change_count(), ROWS);
                    let finish = Instant::now();
                    if phase == "durable_commit_ack" {
                        session.execute("COMMIT").unwrap();
                    } else {
                        session.execute("ROLLBACK").unwrap();
                    }
                    let finish_elapsed = finish.elapsed();
                    elapsed += if phase == "stage_no_ack" {
                        stage_elapsed
                    } else {
                        finish_elapsed
                    };
                    if phase == "durable_commit_ack" {
                        assert_eq!(
                            session
                                .execute("MATCH (n:Item) RETURN n")
                                .unwrap()
                                .row_count(),
                            Some(ROWS)
                        );
                        session.execute("MATCH (n:Item) DELETE n").unwrap();
                    } else {
                        assert_eq!(
                            session
                                .execute("MATCH (n:Item) RETURN n")
                                .unwrap()
                                .row_count(),
                            Some(0)
                        );
                    }
                }
                elapsed
            })
        });
    }
    group.finish();
}
