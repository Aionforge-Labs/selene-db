//! Audit-log framework integration tests (D24 / BRIEF-Item-7 — separate
//! `audit.log`, audit Item 7 / Seam D).
//!
//! The D24 audit log is the durable "events" surface in the
//! snapshot=state / WAL=changes / audit=events split, with retention
//! independent of the WAL lineage. The procedure-pack lifecycle producer that
//! originally fed it was removed in the extension teardown; the framework
//! remains wired (persisted, torn-tail safe, reattached on recovery) for future
//! user-action audit events. These tests drive the surviving framework: the
//! `SharedGraph::with_audit_log` wiring (accept/reject), durable append +
//! torn-tail truncation at the persist layer, and recovery reattach that never
//! clobbers prior records. The complementary proof that `selene_persist::prune`
//! never touches `audit.log` lives in `selene-persist/src/retention/tests.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::GraphId;
use selene_graph::{CheckpointConfig, GraphError, SharedGraph};
use selene_persist::{
    AUDIT_KIND_RESERVED_0, AuditLog, AuditRecord, DEFAULT_AUDIT_FILE_NAME, DEFAULT_WAL_FILE_NAME,
    WalConfig,
};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-audit-lifecycle-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("temp dir created");
    dir
}

fn audit_path(dir: &Path) -> PathBuf {
    dir.join(DEFAULT_AUDIT_FILE_NAME)
}

/// A deterministic opaque engine-event record.
fn sample_record(seq: u64) -> AuditRecord {
    AuditRecord {
        recorded_at_unix_nanos: 1_000 + seq,
        kind: AUDIT_KIND_RESERVED_0,
        payload: vec![seq as u8, 0xAB, 0xCD],
    }
}

fn build_with_audit(dir: &Path, graph_id: GraphId) -> SharedGraph {
    SharedGraph::builder(graph_id)
        .with_wal(dir.join(DEFAULT_WAL_FILE_NAME), WalConfig::default())
        .unwrap()
        .with_audit_log(audit_path(dir))
        .unwrap()
        .build()
        .unwrap()
}

#[test]
fn audit_log_appends_are_durable() {
    let dir = temp_dir("durable");
    let path = audit_path(&dir);
    {
        let mut log = AuditLog::open(&path).unwrap();
        log.append(&sample_record(1)).unwrap();
        log.append(&sample_record(2)).unwrap();
    }
    let records = AuditLog::read_all(&path).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0], sample_record(1));
    assert_eq!(records[1], sample_record(2));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn checkpoint_watermark_is_not_an_audit_event() {
    let dir = temp_dir("checkpoint-watermark");
    let shared = build_with_audit(&dir, GraphId::new(206));

    let checkpoint = shared
        .checkpoint(CheckpointConfig::default())
        .expect("empty WAL-backed graph checkpoints through a watermark");

    assert_eq!(checkpoint.snapshot_sequence, 1);
    assert!(AuditLog::read_all(&audit_path(&dir)).unwrap().is_empty());
    drop(shared);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn audit_log_survives_restart_and_recovery_reattaches() {
    let dir = temp_dir("survive-recover");
    let graph_id = GraphId::new(202);

    // Seed through the standalone persist writer, then attach the graph's
    // composite WAL/audit owner. An independent audit writer may not coexist.
    {
        let mut log = AuditLog::open(&audit_path(&dir)).unwrap();
        log.append(&sample_record(1)).unwrap();
    }
    drop(build_with_audit(&dir, graph_id));
    assert_eq!(AuditLog::read_all(&audit_path(&dir)).unwrap().len(), 1);

    // Recover: the historical event persists, and recovery reattaches the
    // audit log (the file is present) so the framework stays wired post-recovery.
    let _recovered = SharedGraph::recover(&dir, graph_id).unwrap();
    assert_eq!(
        AuditLog::read_all(&audit_path(&dir)).unwrap().len(),
        1,
        "recovery must not clobber or re-derive the audit log"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recovery_truncates_torn_audit_tail() {
    let dir = temp_dir("torn-recover");
    let graph_id = GraphId::new(205);

    {
        let mut log = AuditLog::open(&audit_path(&dir)).unwrap();
        log.append(&sample_record(1)).unwrap();
    }
    drop(build_with_audit(&dir, graph_id));
    // Simulate a crash mid-append: garbage shorter than a record header on the
    // end of audit.log.
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(audit_path(&dir))
            .unwrap();
        f.write_all(&[0x00, 0xFF, 0x42]).unwrap();
    }

    // Recovery opens the audit log, which truncates the torn tail; the good
    // event survives.
    let _recovered = SharedGraph::recover(&dir, graph_id).unwrap();
    let records = AuditLog::read_all(&audit_path(&dir)).unwrap();
    assert_eq!(
        records.len(),
        1,
        "torn tail truncated, the durable event survives"
    );
    assert_eq!(records[0], sample_record(1));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn audit_log_without_wal_is_rejected() {
    let dir = temp_dir("no-wal");
    let result = SharedGraph::builder(GraphId::new(203))
        .with_audit_log(audit_path(&dir))
        .unwrap()
        .build();
    assert!(
        matches!(result, Err(GraphError::Inconsistent { .. })),
        "audit without a WAL must be rejected"
    );
    let _ = fs::remove_dir_all(dir);
}
