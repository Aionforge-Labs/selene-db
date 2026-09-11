//! Actual artifact leases, not merely retained in-memory graph snapshots.
use super::*;
use selene_db::RetentionReason;
use selene_persist::{
    StoreDirectory, control::CompatibilityIdentity, logical_stream::LogicalReader,
};

fn identity() -> CompatibilityIdentity {
    let mut hash = [0; 32];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&selene_profile::PROFILE_HASH[i * 2..i * 2 + 2], 16).unwrap();
    }
    let unicode = selene_catalog::CATALOG_UNICODE_VERSION;
    let collation = match &selene_profile::annex_b_by_id("ID022").unwrap().decision {
        selene_profile::AnnexBDecision::Selected {
            value: selene_profile::AnnexBValue::Identifier(name),
            ..
        } => *name,
        _ => panic!("production collation"),
    };
    CompatibilityIdentity::new(
        selene_profile::PROFILE_ID,
        selene_profile::PROFILE_FORMAT_VERSION,
        hash,
        [unicode.0.into(), unicode.1.into(), unicode.2.into()],
        collation,
        1,
    )
    .unwrap()
}

pub(super) fn witness(rows: usize) {
    let (dir, db) = fixture(rows, true);
    let store = StoreDirectory::open(dir.path()).unwrap();
    let expected_sequence = db.durable_status().unwrap().position.sequence;
    let mut reader = LogicalReader::open(&store, &identity(), 256 << 20).unwrap();
    // A non-authoritative, unclassified artifact demonstrates visible cleanup debt.
    std::fs::write(
        dir.path().join("SNAPSHOT-00000000000000009999.logical"),
        b"unclassified orphan",
    )
    .unwrap();
    for round in 0..3 {
        let checkpoint = db.checkpoint().unwrap();
        println!(
            "ROTATION rows_per_graph={rows} graphs={GRAPHS} indexes={} round={round} write_reservation_us={:.3} rotation_stage_us={:.3} snapshot_bytes={}",
            GRAPHS * 4,
            checkpoint.write_reservation_elapsed.as_secs_f64() * 1e6,
            checkpoint.rotation_elapsed.as_secs_f64() * 1e6,
            checkpoint.bytes
        );
        suffix(&db, 4);
    }
    let before = storage(dir.path());
    let start = Instant::now();
    let held = db.prune().unwrap();
    let held_time = start.elapsed();
    assert!(held.cleanup_error.is_none());
    let pinned: u64 = held
        .retained
        .iter()
        .filter(|a| a.reason == RetentionReason::Reader)
        .map(|a| a.artifact.bytes)
        .sum();
    let debt: u64 = held
        .retained
        .iter()
        .filter(|a| a.reason == RetentionReason::Deferred)
        .map(|a| a.artifact.bytes)
        .sum();
    let removed: u64 = held.removed.iter().map(|a| a.bytes).sum();
    assert!(pinned > 0);
    assert_eq!(before - storage(dir.path()), removed);
    let snapshot = reader.snapshot_body().unwrap();
    let mut records = 0;
    while reader.next_body().unwrap().is_some() {
        records += 1;
    }
    assert!(!reader.incomplete_tail());
    assert_eq!(reader.position().sequence, expected_sequence);
    println!(
        "LEASE rows_per_graph={rows} selected_records={records} consumed_snapshot_bytes={} retained_reader_bytes={pinned} removed_bytes={removed} deferred_bytes={debt} prune_us={:.3} total_storage_bytes={}",
        snapshot.len(),
        held_time.as_secs_f64() * 1e6,
        storage(dir.path())
    );
    drop(reader);
    let before = storage(dir.path());
    let start = Instant::now();
    let released = db.prune().unwrap();
    let elapsed = start.elapsed();
    let removed: u64 = released.removed.iter().map(|a| a.bytes).sum();
    assert!(released.cleanup_error.is_none());
    assert_eq!(removed, pinned);
    assert_eq!(before - storage(dir.path()), removed);
    assert_eq!(
        released
            .retained
            .iter()
            .filter(|a| a.artifact.name.starts_with("SNAPSHOT-")
                && a.reason != RetentionReason::Deferred)
            .count(),
        2
    );
    println!(
        "RECLAIM rows_per_graph={rows} removed_bytes={removed} prune_us={:.3} retained_files={} deferred_bytes={debt} total_storage_bytes={}",
        elapsed.as_secs_f64() * 1e6,
        released.retained.len(),
        storage(dir.path())
    );
    verify(&db, rows, 12);
    drop(db);
    let start = Instant::now();
    let db = Database::open(dir.path()).unwrap();
    let elapsed = start.elapsed();
    verify(&db, rows, 12);
    let info = db.recovery_info().unwrap();
    assert_eq!(info.verified_prefix_records, 0);
    assert_eq!(info.replayed_suffix_records, 4);
    println!(
        "BOUNDED_REOPEN rows_per_graph={rows} checkpoints=3 prefix_records=0 suffix_records=4 verified_wal_bytes={} rebuilt_indexes={} reopen_us={:.3}",
        info.position.offset,
        info.rebuilt_indexes,
        elapsed.as_secs_f64() * 1e6
    );
}
