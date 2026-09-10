//! Unit tests for snapshot + WAL-archive retention.
//!
//! These exercise the prune planner at the file level (synthetic byte files with
//! the real names) — selection floors, the four conjunctive constraints, the
//! MANIFEST-atomic shrink, and orphan reclamation. The end-to-end "recover after
//! prune yields the identical epoch" proof lives in
//! `tests/retention_prune.rs`, which drives real rotations + recovery.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::manifest::Manifest;
use crate::manifest_lock::set_contention_hook;
use crate::snapshot_path::snapshot_path;
use crate::writer_rotation::wal_archive_path;
use crate::{DEFAULT_WAL_FILE_NAME, PersistenceReadGuard};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-retention-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).expect("temp dir created");
    dir
}

fn snap(dir: &Path, seq: u64, bytes: &[u8]) {
    fs::write(snapshot_path(dir, seq), bytes).expect("snapshot file written");
}

fn arch(dir: &Path, seq: u64, bytes: &[u8]) {
    fs::write(archive_path(dir, seq), bytes).expect("archive file written");
}

fn archive_path(dir: &Path, seq: u64) -> PathBuf {
    wal_archive_path(&dir.join(DEFAULT_WAL_FILE_NAME), seq)
}

fn write_manifest(dir: &Path, live: u64, archived: Vec<u64>) {
    Manifest {
        live_snapshot_seq: live,
        active_wal_header_seq: live,
        compaction_epoch: 0,
        active_wal: DEFAULT_WAL_FILE_NAME.to_owned(),
        archived_wal_seqs: archived,
    }
    .write_atomic(dir)
    .expect("manifest committed");
}

fn snap_exists(dir: &Path, seq: u64) -> bool {
    snapshot_path(dir, seq).exists()
}

fn arch_exists(dir: &Path, seq: u64) -> bool {
    archive_path(dir, seq).exists()
}

/// Backdate a file's modification time by `ago`.
fn backdate(path: &Path, ago: Duration) {
    let when = SystemTime::now() - ago;
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open to set mtime")
        .set_modified(when)
        .expect("mtime set");
}

#[test]
fn default_policy_matches_d26() {
    let policy = RetentionPolicy::default();
    assert_eq!(policy.keep_n_snapshots, 2);
    assert_eq!(policy.keep_n_wal_archives, 4);
    assert_eq!(policy.max_total_size_bytes, None);
    assert_eq!(policy.time_based, None);
    assert_eq!(DEFAULT_KEEP_SNAPSHOTS, 2);
    assert_eq!(DEFAULT_KEEP_WAL_ARCHIVES, 4);
}

#[test]
fn parse_wal_archive_filename_round_trips() {
    let path = archive_path(Path::new("/tmp"), 42);
    assert_eq!(
        parse_wal_archive_filename(path.file_name().unwrap()),
        Some(42)
    );
}

#[test]
fn parse_wal_archive_filename_rejects_tmp_and_garbage() {
    use std::ffi::OsStr;
    assert_eq!(
        parse_wal_archive_filename(OsStr::new("wal.7.archive.tmp.123.0")),
        None
    );
    assert_eq!(parse_wal_archive_filename(OsStr::new("wal..archive")), None);
    assert_eq!(parse_wal_archive_filename(OsStr::new("wal.log")), None);
    assert_eq!(
        parse_wal_archive_filename(OsStr::new("snapshot.7.snap")),
        None
    );
    assert_eq!(
        parse_wal_archive_filename(OsStr::new("wal.x.archive")),
        None
    );
}

#[test]
fn prune_without_manifest_is_noop() {
    let dir = temp_dir("no-manifest");
    snap(&dir, 1, b"s1");
    arch(&dir, 1, b"a1");
    let outcome = prune(&dir, &RetentionPolicy::default()).unwrap();
    assert_eq!(outcome, PruneOutcome::default());
    // Nothing deleted without an authoritative epoch to protect.
    assert!(snap_exists(&dir, 1));
    assert!(arch_exists(&dir, 1));
    assert!(dir.join(crate::STORE_LOCK_FILE_NAME).is_file());
    assert!(dir.join(crate::MANIFEST_LOCK_FILE_NAME).is_file());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_keep_zero_snapshots_still_retains_live() {
    let dir = temp_dir("keep-zero");
    snap(&dir, 9, b"older");
    snap(&dir, 10, b"live");
    write_manifest(&dir, 10, vec![]);

    let policy = RetentionPolicy {
        keep_n_snapshots: 0,
        keep_n_wal_archives: 0,
        ..RetentionPolicy::default()
    };
    let outcome = prune(&dir, &policy).unwrap();

    // The live snapshot survives even at keep_n = 0; the predecessor is reclaimed.
    assert!(snap_exists(&dir, 10));
    assert!(!snap_exists(&dir, 9));
    assert_eq!(outcome.retained_snapshots, vec![10]);
    assert_eq!(outcome.deleted_snapshots, vec![9]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_default_keeps_live_plus_one_predecessor() {
    let dir = temp_dir("default-snaps");
    for seq in [6, 7, 8, 9, 10] {
        snap(&dir, seq, b"x");
    }
    write_manifest(&dir, 10, vec![]);

    let outcome = prune(&dir, &RetentionPolicy::default()).unwrap();

    // keep_n_snapshots = 2 → live (10) + newest predecessor (9).
    assert_eq!(outcome.retained_snapshots, vec![9, 10]);
    assert_eq!(outcome.deleted_snapshots, vec![6, 7, 8]);
    assert!(snap_exists(&dir, 9) && snap_exists(&dir, 10));
    assert!(!snap_exists(&dir, 6) && !snap_exists(&dir, 7) && !snap_exists(&dir, 8));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn read_guard_pins_backup_artifacts_until_reader_releases() {
    let dir = temp_dir("read-guard-prune");
    snap(&dir, 9, b"backup-predecessor");
    snap(&dir, 10, b"live");
    write_manifest(&dir, 10, vec![]);
    let guard = PersistenceReadGuard::acquire(&dir).unwrap();
    assert_eq!(
        guard.read_manifest().unwrap().unwrap().live_snapshot_seq,
        10
    );

    let (contended_tx, contended_rx) = sync_channel(0);
    let (finished_tx, finished_rx) = sync_channel(0);
    let worker_dir = dir.clone();
    let worker = thread::spawn(move || {
        set_contention_hook(move || contended_tx.send(()).unwrap());
        let outcome = prune(
            &worker_dir,
            &RetentionPolicy {
                keep_n_snapshots: 1,
                keep_n_wal_archives: 0,
                ..RetentionPolicy::default()
            },
        )
        .unwrap();
        finished_tx.send(outcome).unwrap();
    });

    contended_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("prune reaches shared read-guard contention");
    assert!(snap_exists(&dir, 9));
    assert_eq!(
        finished_rx.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    );

    drop(guard);
    let outcome = finished_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("prune completes after backup read guard releases");
    assert_eq!(outcome.deleted_snapshots, vec![9]);
    assert!(!snap_exists(&dir, 9));
    worker.join().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn prune_keeps_newest_archives_and_shrinks_manifest() {
    let dir = temp_dir("default-archs");
    snap(&dir, 10, b"live");
    for seq in [1, 2, 3, 4, 5, 6] {
        arch(&dir, seq, b"a");
    }
    write_manifest(&dir, 10, vec![1, 2, 3, 4, 5, 6]);

    let outcome = prune(&dir, &RetentionPolicy::default()).unwrap();

    // keep_n_wal_archives = 4 → newest four (3,4,5,6).
    assert_eq!(outcome.retained_wal_archives, vec![3, 4, 5, 6]);
    assert_eq!(outcome.deleted_wal_archives, vec![1, 2]);
    assert!(!arch_exists(&dir, 1) && !arch_exists(&dir, 2));
    assert!(arch_exists(&dir, 6));

    // The committed MANIFEST shrank to the retained set and round-trips.
    let manifest = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(manifest.archived_wal_seqs, vec![3, 4, 5, 6]);
    assert_eq!(manifest.live_snapshot_seq, 10);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_deletes_orphan_snapshot_above_live() {
    let dir = temp_dir("orphan-snap");
    snap(&dir, 5, b"live");
    snap(&dir, 7, b"orphan"); // crashed-rotation orphan, seq > live
    write_manifest(&dir, 5, vec![]);

    let outcome = prune(&dir, &RetentionPolicy::default()).unwrap();

    assert!(snap_exists(&dir, 5));
    assert!(!snap_exists(&dir, 7));
    assert_eq!(outcome.deleted_snapshots, vec![7]);
    assert_eq!(outcome.retained_snapshots, vec![5]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_reclaims_superseded_orphan_archive_without_adopting_it() {
    let dir = temp_dir("orphan-arch");
    snap(&dir, 10, b"live");
    arch(&dir, 9, b"tracked");
    arch(&dir, 3, b"orphan"); // on disk, NOT in the manifest, seq < live
    write_manifest(&dir, 10, vec![9]);

    let outcome = prune(&dir, &RetentionPolicy::default()).unwrap();

    // The superseded orphan (3) is reclaimed; the tracked archive (9) survives.
    assert!(!arch_exists(&dir, 3));
    assert!(arch_exists(&dir, 9));
    assert_eq!(outcome.deleted_wal_archives, vec![3]);
    // The manifest never adopts the orphan — it lists only the tracked survivor.
    let manifest = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(manifest.archived_wal_seqs, vec![9]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_does_not_touch_archive_orphan_at_or_above_live() {
    let dir = temp_dir("orphan-arch-boundary");
    snap(&dir, 10, b"live");
    arch(&dir, 10, b"boundary"); // untracked, seq == live: rotation's domain
    arch(&dir, 11, b"future"); // untracked, seq > live
    write_manifest(&dir, 10, vec![]);

    let outcome = prune(&dir, &RetentionPolicy::default()).unwrap();

    // Current-epoch-boundary orphans are left to rotation's idempotent retry.
    assert!(arch_exists(&dir, 10));
    assert!(arch_exists(&dir, 11));
    assert!(outcome.deleted_wal_archives.is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_leaves_active_wal_untouched() {
    let dir = temp_dir("active-wal");
    snap(&dir, 10, b"live");
    fs::write(dir.join(DEFAULT_WAL_FILE_NAME), b"active-wal-bytes").unwrap();
    write_manifest(&dir, 10, vec![]);

    prune(&dir, &RetentionPolicy::default()).unwrap();

    assert!(dir.join(DEFAULT_WAL_FILE_NAME).exists());
    assert_eq!(
        fs::read(dir.join(DEFAULT_WAL_FILE_NAME)).unwrap(),
        b"active-wal-bytes"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_is_idempotent() {
    let dir = temp_dir("idempotent");
    for seq in [6, 7, 8, 9, 10] {
        snap(&dir, seq, b"x");
    }
    for seq in [1, 2, 3, 4, 5, 6] {
        arch(&dir, seq, b"a");
    }
    write_manifest(&dir, 10, vec![1, 2, 3, 4, 5, 6]);

    let first = prune(&dir, &RetentionPolicy::default()).unwrap();
    assert!(!first.deleted_snapshots.is_empty());

    let second = prune(&dir, &RetentionPolicy::default()).unwrap();
    assert!(second.deleted_snapshots.is_empty());
    assert!(second.deleted_wal_archives.is_empty());
    assert_eq!(second.bytes_reclaimed, 0);
    assert_eq!(first.retained_snapshots, second.retained_snapshots);
    assert_eq!(first.retained_wal_archives, second.retained_wal_archives);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_size_constraint_evicts_oldest_and_keeps_live() {
    let dir = temp_dir("size");
    // 100 bytes each: snapshots 9 (pred) + 10 (live), archives 7 + 8.
    snap(&dir, 9, &[0_u8; 100]);
    snap(&dir, 10, &[0_u8; 100]);
    arch(&dir, 7, &[0_u8; 100]);
    arch(&dir, 8, &[0_u8; 100]);
    write_manifest(&dir, 10, vec![7, 8]);

    let policy = RetentionPolicy {
        max_total_size_bytes: Some(250),
        ..RetentionPolicy::default()
    };
    let outcome = prune(&dir, &policy).unwrap();

    // Count selection keeps all four (400 bytes). Size budget 250 evicts oldest
    // first (7, then 8 → 200 ≤ 250), never the live snapshot.
    assert!(snap_exists(&dir, 10));
    assert!(snap_exists(&dir, 9));
    assert!(!arch_exists(&dir, 7));
    assert!(!arch_exists(&dir, 8));
    assert_eq!(outcome.retained_snapshots, vec![9, 10]);
    assert!(outcome.retained_wal_archives.is_empty());
    assert_eq!(outcome.deleted_wal_archives, vec![7, 8]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_size_constraint_never_evicts_live_even_when_alone_over_budget() {
    let dir = temp_dir("size-live-only");
    snap(&dir, 10, &[0_u8; 1000]); // live, larger than the budget
    write_manifest(&dir, 10, vec![]);

    let policy = RetentionPolicy {
        max_total_size_bytes: Some(10),
        ..RetentionPolicy::default()
    };
    let outcome = prune(&dir, &policy).unwrap();

    // The live snapshot is exempt; the budget cannot force its deletion.
    assert!(snap_exists(&dir, 10));
    assert_eq!(outcome.retained_snapshots, vec![10]);
    assert!(outcome.deleted_snapshots.is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_time_constraint_evicts_aged_and_keeps_live() {
    let dir = temp_dir("time");
    snap(&dir, 10, b"live");
    snap(&dir, 9, b"old-pred");
    arch(&dir, 8, b"old-arch");
    arch(&dir, 7, b"fresh-arch");
    write_manifest(&dir, 10, vec![7, 8]);

    // Age the live snapshot, the predecessor, and archive 8 well past the cutoff;
    // leave archive 7 fresh.
    backdate(&snapshot_path(&dir, 10), Duration::from_secs(3600));
    backdate(&snapshot_path(&dir, 9), Duration::from_secs(3600));
    backdate(&archive_path(&dir, 8), Duration::from_secs(3600));

    let policy = RetentionPolicy {
        time_based: Some(Duration::from_secs(60)),
        ..RetentionPolicy::default()
    };
    let outcome = prune(&dir, &policy).unwrap();

    // Aged predecessor + aged archive evicted; fresh archive kept; aged-but-live
    // snapshot exempt.
    assert!(snap_exists(&dir, 10)); // live exempt despite age
    assert!(!snap_exists(&dir, 9)); // aged predecessor evicted
    assert!(!arch_exists(&dir, 8)); // aged archive evicted
    assert!(arch_exists(&dir, 7)); // fresh archive retained
    assert_eq!(outcome.retained_snapshots, vec![10]);
    assert_eq!(outcome.retained_wal_archives, vec![7]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_reports_bytes_reclaimed() {
    let dir = temp_dir("bytes");
    snap(&dir, 10, b"live");
    snap(&dir, 8, &[0_u8; 30]); // pruned (keep 2: live + 9)
    snap(&dir, 9, b"pred");
    arch(&dir, 1, &[0_u8; 70]); // pruned (keep 4, only one archive but seq<live)
    write_manifest(&dir, 10, vec![1]);

    let policy = RetentionPolicy {
        keep_n_snapshots: 2,
        keep_n_wal_archives: 0, // drop the single archive
        ..RetentionPolicy::default()
    };
    let outcome = prune(&dir, &policy).unwrap();

    // Deleted: snapshot 8 (30 bytes) + archive 1 (70 bytes) = 100.
    assert_eq!(outcome.bytes_reclaimed, 100);
    assert_eq!(outcome.deleted_snapshots, vec![8]);
    assert_eq!(outcome.deleted_wal_archives, vec![1]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_drops_manifest_seq_whose_file_vanished() {
    let dir = temp_dir("missing-file");
    snap(&dir, 10, b"live");
    arch(&dir, 9, b"present");
    // Manifest lists archive 8 too, but its file is missing (crash hygiene).
    write_manifest(&dir, 10, vec![8, 9]);

    let outcome = prune(&dir, &RetentionPolicy::default()).unwrap();

    // The vanished seq is dropped from the rewritten manifest; the present one
    // is retained.
    let manifest = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(manifest.archived_wal_seqs, vec![9]);
    assert!(arch_exists(&dir, 9));
    assert_eq!(outcome.retained_wal_archives, vec![9]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_ignores_audit_log_file() {
    // Seam-D guarantee (Item 7): snapshot/WAL retention never touches audit.log,
    // so engine audit events survive WAL-archive pruning. prune scans only
    // `snapshot.{seq}.snap` / `wal.{seq}.archive` names; `audit.log` matches
    // neither and is left untouched even under the most aggressive policy.
    let dir = temp_dir("ignore-audit");
    snap(&dir, 10, b"live");
    arch(&dir, 1, b"a");
    arch(&dir, 2, b"a");
    let audit = dir.join("audit.log");
    fs::write(&audit, b"SLAU-engine-audit-events").unwrap();
    write_manifest(&dir, 10, vec![1, 2]);

    let policy = RetentionPolicy {
        keep_n_snapshots: 0,
        keep_n_wal_archives: 0,
        max_total_size_bytes: Some(1),
        time_based: Some(Duration::from_nanos(1)),
    };
    prune(&dir, &policy).unwrap();

    assert!(audit.exists(), "audit.log must survive WAL/snapshot prune");
    assert_eq!(fs::read(&audit).unwrap(), b"SLAU-engine-audit-events");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_idempotent_noop_leaves_manifest_byte_identical() {
    // The "do not rewrite when the retained archive set is unchanged"
    // optimization avoids a needless dir-fsync + torn-MANIFEST window. Pin it:
    // run prune to a stable state, snapshot the committed MANIFEST bytes, run a
    // second prune under the same policy, and assert the MANIFEST is byte-for-byte
    // untouched (the second prune is a true no-op).
    let dir = temp_dir("idempotent-noop");
    snap(&dir, 10, b"live");
    snap(&dir, 9, b"pred");
    arch(&dir, 8, b"a8");
    arch(&dir, 9, b"a9");
    write_manifest(&dir, 10, vec![8, 9]);

    let policy = RetentionPolicy::default();
    // First prune reaches the stable retained set.
    let first = prune(&dir, &policy).unwrap();

    let manifest_path = dir.join(crate::MANIFEST_FILE_NAME);
    let bytes_after_first = fs::read(&manifest_path).unwrap();

    // Second prune under the same policy must change nothing.
    let second = prune(&dir, &policy).unwrap();
    assert!(
        second.deleted_snapshots.is_empty() && second.deleted_wal_archives.is_empty(),
        "second prune must reclaim nothing"
    );
    assert_eq!(second.bytes_reclaimed, 0);
    assert_eq!(second.retained_snapshots, first.retained_snapshots);
    assert_eq!(second.retained_wal_archives, first.retained_wal_archives);

    let bytes_after_second = fs::read(&manifest_path).unwrap();
    assert_eq!(
        bytes_after_first, bytes_after_second,
        "idempotent prune must leave the MANIFEST byte-identical"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_size_constraint_evicts_into_predecessor_snapshots_but_keeps_live() {
    let dir = temp_dir("size-into-snaps");
    // Three snapshots, no archives; count keeps all three (live + 2 predecessors).
    snap(&dir, 8, &[0_u8; 100]);
    snap(&dir, 9, &[0_u8; 100]);
    snap(&dir, 10, &[0_u8; 100]); // live
    write_manifest(&dir, 10, vec![]);

    let policy = RetentionPolicy {
        keep_n_snapshots: 3, // count alone would retain 8, 9, 10
        keep_n_wal_archives: 0,
        max_total_size_bytes: Some(150),
        time_based: None,
    };
    let outcome = prune(&dir, &policy).unwrap();

    // Size budget 150 forces eviction *past* archives into predecessor snapshots
    // (evict 8 → 200, evict 9 → 100 ≤ 150), but the live snapshot 10 is exempt.
    assert!(snap_exists(&dir, 10));
    assert!(!snap_exists(&dir, 9));
    assert!(!snap_exists(&dir, 8));
    assert_eq!(outcome.retained_snapshots, vec![10]);
    assert_eq!(outcome.deleted_snapshots, vec![8, 9]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_wal_only_epoch_live_zero_keeps_newest_snapshots() {
    let dir = temp_dir("live-zero");
    // live_snapshot_seq = 0 → no snapshot is authoritative (WAL-only epoch).
    // None of the on-disk snapshots is the live one, so there is no floor to
    // force-retain; count selection keeps the newest keep_n.
    for seq in [3, 4, 5] {
        snap(&dir, seq, b"x");
    }
    write_manifest(&dir, 0, vec![]);

    let policy = RetentionPolicy {
        keep_n_snapshots: 2,
        ..RetentionPolicy::default()
    };
    let outcome = prune(&dir, &policy).unwrap();

    // Newest two retained; oldest reclaimed. No live exemption applies at live=0.
    assert_eq!(outcome.retained_snapshots, vec![4, 5]);
    assert_eq!(outcome.deleted_snapshots, vec![3]);
    assert!(snap_exists(&dir, 5) && snap_exists(&dir, 4) && !snap_exists(&dir, 3));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prune_conjunctive_count_time_and_size_compose() {
    let dir = temp_dir("conjunctive");
    // Snapshots 9 (pred) + 10 (live), 50 bytes each; archives 5,6,7,8, 100 each.
    snap(&dir, 9, &[0_u8; 50]);
    snap(&dir, 10, &[0_u8; 50]);
    for seq in [5, 6, 7, 8] {
        arch(&dir, seq, &[0_u8; 100]);
    }
    write_manifest(&dir, 10, vec![5, 6, 7, 8]);

    // Age archives 5 and 6 past the cutoff; leave 7, 8 (and both snapshots) fresh.
    backdate(&archive_path(&dir, 5), Duration::from_secs(3600));
    backdate(&archive_path(&dir, 6), Duration::from_secs(3600));

    let policy = RetentionPolicy {
        keep_n_snapshots: 2,                       // count → snapshots {9,10}
        keep_n_wal_archives: 4,                    // count → archives {5,6,7,8}
        time_based: Some(Duration::from_secs(60)), // time → drops aged {5,6}
        max_total_size_bytes: Some(250),           // size → 100(snaps)+200(archs) > 250, drop 7
    };
    let outcome = prune(&dir, &policy).unwrap();

    // All three constraints compose: count admitted 4 archives, time cut to {7,8},
    // size (oldest-first, live-exempt) cut to {8}. Snapshots untouched (100 ≤ 250
    // once archives shrink). Deleted archives = {5,6} (time) ∪ {7} (size).
    assert_eq!(outcome.retained_snapshots, vec![9, 10]);
    assert_eq!(outcome.retained_wal_archives, vec![8]);
    assert_eq!(outcome.deleted_wal_archives, vec![5, 6, 7]);
    let manifest = Manifest::read(&dir).unwrap().unwrap();
    assert_eq!(manifest.archived_wal_seqs, vec![8]);
    let _ = fs::remove_dir_all(dir);
}
