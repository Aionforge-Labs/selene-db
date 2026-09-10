use std::fs::{self, OpenOptions};
use std::io::Read;
use std::io::Write;
use std::sync::Arc;

use selene_core::{Change, NodeId, Origin, PropertyMap, Value, db_string};

use super::*;
use crate::writer::append::WAL_RECORD_BUFFER_RETAIN_LIMIT;
use crate::{MAX_PRINCIPAL_BYTES, WAL_FILE_HEADER_LEN, WalReader};

fn temp_path(_name: &str) -> selene_testing::PersistenceTestPath {
    selene_testing::PersistenceTestPath::new()
}

fn changes() -> Vec<Change> {
    vec![Change::NodeCreated {
        id: NodeId::new(1),
        labels: selene_core::LabelSet::single(db_string("writer.node").unwrap()),
        properties: PropertyMap::new(),
    }]
}

fn bytes_change(byte_count: usize) -> Vec<Change> {
    vec![Change::NodeCreated {
        id: NodeId::new(1),
        labels: selene_core::LabelSet::single(db_string("writer.node").unwrap()),
        properties: PropertyMap::from_pairs([(
            db_string("writer.bytes").unwrap(),
            Value::Bytes(Arc::from(vec![0x5a_u8; byte_count])),
        )])
        .unwrap(),
    }]
}

#[test]
fn open_new_file_writes_header() {
    let path = temp_path("open-new");
    {
        let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
        assert_eq!(writer.last_sequence(), 0);
    }
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        WAL_FILE_HEADER_LEN as u64
    );
    let _ = fs::remove_file(path);
}

#[test]
fn writer_reports_its_active_path() {
    let path = temp_path("active-path");
    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    let expected = path
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap()
        .join(path.file_name().unwrap());
    assert_eq!(writer.path(), expected);
    drop(writer);
    let _ = fs::remove_file(path);
}

#[test]
fn relative_writer_path_stays_bound_across_cwd_change() {
    const CHILD_ENV: &str = "SELENE_TEST_RELATIVE_WAL_CWD_CHILD";
    if std::env::var_os(CHILD_ENV).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "writer::tests::relative_writer_path_stays_bound_across_cwd_change",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .status()
            .unwrap();
        assert!(status.success(), "isolated CWD-change child succeeds");
        return;
    }

    let original_dir = std::env::current_dir().unwrap();
    let root = temp_path("relative-cwd-root");
    let open_dir = root.join("opened");
    let later_dir = root.join("later");
    fs::create_dir_all(&open_dir).unwrap();
    fs::create_dir_all(&later_dir).unwrap();
    std::env::set_current_dir(&open_dir).unwrap();
    let mut writer =
        WalWriter::open(Path::new(DEFAULT_WAL_FILE_NAME), WalConfig::default()).unwrap();
    assert_eq!(
        writer.path().canonicalize().unwrap(),
        open_dir.join(DEFAULT_WAL_FILE_NAME).canonicalize().unwrap()
    );
    assert_eq!(
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
            .unwrap(),
        1
    );
    writer.flush().unwrap();

    std::env::set_current_dir(&later_dir).unwrap();
    let mut builder = crate::SnapshotBuilder::new(crate::SnapshotConfig {
        dir: writer.path().parent().unwrap().to_path_buf(),
        sequence: 1,
        compression: crate::SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    builder
        .add_section(*b"TEST", *b"BODY", b"stable".to_vec())
        .unwrap();
    writer.rotate_with_manifest(builder).unwrap();
    assert!(open_dir.join(crate::MANIFEST_FILE_NAME).is_file());
    assert!(crate::snapshot_path(&open_dir, 1).is_file());
    assert!(!later_dir.join(crate::MANIFEST_FILE_NAME).exists());
    assert!(!crate::snapshot_path(&later_dir, 1).exists());

    drop(writer);
    std::env::set_current_dir(original_dir).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn relative_snapshot_directory_preflight_is_cwd_safe() {
    const CHILD_ENV: &str = "SELENE_TEST_RELATIVE_SNAPSHOT_DIR_CHILD";
    if std::env::var_os(CHILD_ENV).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "writer::tests::relative_snapshot_directory_preflight_is_cwd_safe",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .status()
            .unwrap();
        assert!(status.success(), "isolated relative-dir child succeeds");
        return;
    }

    let original_dir = std::env::current_dir().unwrap();
    let root = temp_path("relative-snapshot-dir-root");
    let same_dir = root.join("same");
    let open_dir = root.join("opened");
    let later_dir = root.join("later");
    fs::create_dir_all(&same_dir).unwrap();
    fs::create_dir_all(&open_dir).unwrap();
    fs::create_dir_all(&later_dir).unwrap();

    // A direct relative API use (`wal.log` plus builder dir `.`) resolves to
    // the same canonical directory and is accepted.
    std::env::set_current_dir(&same_dir).unwrap();
    let mut same_writer = WalWriter::open(
        Path::new(DEFAULT_WAL_FILE_NAME),
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();
    same_writer
        .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
        .unwrap();
    let same_builder = crate::SnapshotBuilder::new(crate::SnapshotConfig {
        dir: PathBuf::from("."),
        sequence: 1,
        compression: crate::SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    same_writer.rotate_with_manifest(same_builder).unwrap();
    drop(same_writer);

    // The builder anchors at construction: a later CWD change cannot redirect
    // publication, and the retained matching directory remains usable.
    std::env::set_current_dir(&open_dir).unwrap();
    let mut moved_writer = WalWriter::open(
        Path::new(DEFAULT_WAL_FILE_NAME),
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();
    moved_writer
        .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
        .unwrap();
    let moved_builder = crate::SnapshotBuilder::new(crate::SnapshotConfig {
        dir: PathBuf::from("."),
        sequence: 1,
        compression: crate::SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    std::env::set_current_dir(&later_dir).unwrap();
    moved_writer.rotate_with_manifest(moved_builder).unwrap();
    assert_eq!(moved_writer.entries_since_fsync(), 0);
    assert!(open_dir.join(crate::MANIFEST_FILE_NAME).exists());
    assert!(!later_dir.join(crate::MANIFEST_FILE_NAME).exists());
    drop(moved_writer);

    std::env::set_current_dir(original_dir).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn append_assigns_monotonic_sequences() {
    let path = temp_path("seq");
    let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    assert_eq!(
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
            .unwrap(),
        1
    );
    assert_eq!(
        writer
            .append(HlcTimestamp::new(1, 1), Origin::Local, None, &changes())
            .unwrap(),
        2
    );
    assert_eq!(writer.last_sequence(), 2);
    let _ = fs::remove_file(path);
}

#[test]
fn record_buffer_reuse_does_not_retain_huge_entries() {
    let path = temp_path("record-buffer-cap");
    let mut writer = WalWriter::open_with_compression(
        &path,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
        WalCompression::disabled(),
    )
    .unwrap();
    writer
        .append(
            HlcTimestamp::new(1, 0),
            Origin::Local,
            None,
            &bytes_change(WAL_RECORD_BUFFER_RETAIN_LIMIT + 1024),
        )
        .unwrap();
    assert_eq!(writer.record.capacity(), 0);

    writer
        .append(HlcTimestamp::new(2, 0), Origin::Local, None, &changes())
        .unwrap();
    assert!(writer.record.capacity() <= WAL_RECORD_BUFFER_RETAIN_LIMIT);
    let _ = fs::remove_file(path);
}

#[test]
fn reopen_recovers_last_sequence() {
    let path = temp_path("reopen");
    {
        let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
            .unwrap();
    }
    let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    assert_eq!(writer.last_sequence(), 1);
    assert_eq!(
        writer
            .append(HlcTimestamp::new(2, 0), Origin::Local, None, &changes())
            .unwrap(),
        2
    );
    let _ = fs::remove_file(path);
}

#[test]
fn principal_overflow_does_not_increment_sequence() {
    let path = temp_path("principal");
    let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    let len_before = fs::metadata(&path).unwrap().len();
    let err = writer
        .append(
            HlcTimestamp::zero(),
            Origin::Local,
            Some(Arc::from(vec![1_u8; MAX_PRINCIPAL_BYTES + 1])),
            &changes(),
        )
        .unwrap_err();
    assert!(matches!(err, PersistError::PrincipalTooLarge { .. }));
    assert_eq!(writer.last_sequence(), 0);
    assert_eq!(fs::metadata(&path).unwrap().len(), len_before);
    let _ = fs::remove_file(path);
}

#[test]
fn partial_tail_is_truncated_on_open() {
    let path = temp_path("tail");
    {
        let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
            .unwrap();
        writer.flush().unwrap();
    }
    let valid_len = fs::metadata(&path).unwrap().len();
    {
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[0, 1, 2]).unwrap();
    }
    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    assert_eq!(writer.last_sequence(), 1);
    assert_eq!(fs::metadata(&path).unwrap().len(), valid_len);
    let _ = fs::remove_file(path);
}

/// Build a fixed prefix carrying a VALID prefix checksum around caller-chosen
/// fields.
///
/// Under v2 any 32 bytes of garbage reached the length checks, so these tests
/// used to just append junk. Under v3 an unauthenticated prefix is rejected
/// before a single field is looked at — so a test that wants to exercise a
/// post-prefix guard has to author a frame the prefix checksum accepts, or it
/// silently tests the checksum instead of the guard it names.
fn checksum_valid_prefix(payload_len: u32, principal_len_p1: u16, extent: &[u8]) -> [u8; 40] {
    let mut prefix = [0_u8; 40];
    prefix[4..8].copy_from_slice(&payload_len.to_le_bytes());
    prefix[12..20].copy_from_slice(&2_u64.to_le_bytes());
    prefix[34..36].copy_from_slice(&principal_len_p1.to_le_bytes());
    prefix[36..40].copy_from_slice(&crate::payload::checksum_lo(extent).to_le_bytes());
    let checksum = crate::payload::checksum_lo(&prefix[4..40]);
    prefix[0..4].copy_from_slice(&checksum.to_le_bytes());
    prefix
}

/// An authenticated prefix declaring a payload far past end of file.
///
/// The declared extent is what decides this, not the 256 MiB cap: the frame
/// says it runs to roughly 4 GiB and the file ends 40 bytes later, so nothing
/// can follow it and it is a tear.
#[test]
fn oversized_payload_len_in_tail_truncates_on_open() {
    let path = temp_path("tail-payload-oversize");
    {
        let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
            .unwrap();
        writer.flush().unwrap();
    }
    let valid_len = fs::metadata(&path).unwrap().len();
    {
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&checksum_valid_prefix(u32::MAX, 0, &[]))
            .unwrap();
    }
    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    assert_eq!(writer.last_sequence(), 1);
    assert_eq!(
        writer.tail_repair().expect("repair").reason,
        crate::WalTailReason::ShortFrame
    );
    assert_eq!(fs::metadata(&path).unwrap().len(), valid_len);
    let _ = fs::remove_file(path);
}

/// An authenticated prefix declaring a principal one byte over the 4 KiB cap,
/// with those bytes actually present so the frame does fit in the file.
///
/// That is what makes this reach `PrincipalTooLarge` at all: the guard lives
/// after the prefix checksum, so it is only reachable through a frame whose
/// extent is well-formed and whose principal length is merely illegal.
#[test]
fn oversized_principal_len_in_tail_truncates_on_open() {
    let path = temp_path("tail-principal-oversize");
    {
        let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
            .unwrap();
        writer.flush().unwrap();
    }
    let valid_len = fs::metadata(&path).unwrap().len();
    let extent = vec![7_u8; MAX_PRINCIPAL_BYTES + 1];
    let oversized_p1 = u16::try_from(MAX_PRINCIPAL_BYTES + 2).unwrap();
    {
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&checksum_valid_prefix(0, oversized_p1, &extent))
            .unwrap();
        file.write_all(&extent).unwrap();
    }
    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    assert_eq!(writer.last_sequence(), 1);
    assert_eq!(
        writer.tail_repair().expect("repair").reason,
        crate::WalTailReason::CorruptFinalFrame,
        "the cap is enforced after the prefix, so this is a stage-2 verdict"
    );
    assert_eq!(fs::metadata(&path).unwrap().len(), valid_len);
    let _ = fs::remove_file(path);
}

#[test]
fn checksum_corrupt_tail_is_truncated_on_open() {
    let path = temp_path("tail-checksum-corrupt");
    let valid_len = {
        let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
            .unwrap();
        let valid_len = writer.committed_offset();
        writer
            .append(HlcTimestamp::new(2, 0), Origin::Local, None, &changes())
            .unwrap();
        writer.flush().unwrap();
        valid_len
    };
    {
        let corrupt_offset = valid_len + crate::entry_header::FIXED_ENTRY_HEADER_BYTES as u64;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(corrupt_offset)).unwrap();
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 0xFF;
        file.seek(SeekFrom::Start(corrupt_offset)).unwrap();
        file.write_all(&byte).unwrap();
        file.sync_data().unwrap();
    }

    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    assert_eq!(writer.last_sequence(), 1);
    assert_eq!(writer.committed_offset(), valid_len);
    assert_eq!(fs::metadata(&path).unwrap().len(), valid_len);
    let _ = fs::remove_file(path);
}

#[test]
fn group_commit_defers_fsync_until_threshold() {
    let path = temp_path("group");
    let mut writer = WalWriter::open(
        &path,
        WalConfig {
            sync_policy: SyncPolicy::EveryN(3),
            snapshot_seq: 0,
        },
    )
    .unwrap();
    assert_eq!(writer.sync_policy.as_every_n(), Some(3));
    writer
        .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
        .unwrap();
    writer
        .append(HlcTimestamp::new(1, 1), Origin::Local, None, &changes())
        .unwrap();
    assert_eq!(writer.entries_since_fsync, 2);
    writer
        .append(HlcTimestamp::new(1, 2), Origin::Local, None, &changes())
        .unwrap();
    assert_eq!(writer.entries_since_fsync, 0);
    let _ = fs::remove_file(path);
}

#[test]
fn wal_config_default_is_every_n_1() {
    assert_eq!(WalConfig::default().sync_policy, SyncPolicy::EveryN(1));
}

#[test]
fn with_fsync_every_n_preserves_legacy_threshold() {
    assert_eq!(
        WalConfig::with_fsync_every_n(7),
        WalConfig {
            sync_policy: SyncPolicy::EveryN(7),
            snapshot_seq: 0,
        }
    );
}

#[test]
fn every_n_zero_normalizes_to_one_on_open() {
    let path = temp_path("normalize");
    let writer = WalWriter::open(
        &path,
        WalConfig {
            sync_policy: SyncPolicy::EveryN(0),
            snapshot_seq: 0,
        },
    )
    .unwrap();
    assert_eq!(writer.sync_policy.as_every_n(), Some(1));
    let _ = fs::remove_file(path);
}

#[test]
fn on_flush_only_accumulates_until_explicit_flush() {
    let path = temp_path("on-flush");
    let mut writer = WalWriter::open(
        &path,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();
    for tick in 0..3 {
        writer
            .append(HlcTimestamp::new(1, tick), Origin::Local, None, &changes())
            .unwrap();
    }
    assert_eq!(writer.entries_since_fsync, 3);
    writer.flush().unwrap();
    assert_eq!(writer.entries_since_fsync, 0);
    let _ = fs::remove_file(path);
}

#[test]
fn on_flush_only_drop_does_not_fsync() {
    assert!(!SyncPolicy::OnFlushOnly.syncs_on_drop());
    assert!(SyncPolicy::EveryN(1).syncs_on_drop());
}

#[test]
fn replicated_origin_round_trips_node_id_and_source_seq() {
    let path = temp_path("origin");
    {
        let mut writer = WalWriter::open(&path, WalConfig::default()).unwrap();
        writer
            .append(
                HlcTimestamp::new(1, 0),
                Origin::Replicated {
                    source_node_id: NodeId::new(77),
                    source_seq: 9,
                },
                None,
                &changes(),
            )
            .unwrap();
    }
    let reader = WalReader::open(&path).unwrap();
    let entry = reader.iterate(|_| true).unwrap().next().unwrap().unwrap();
    assert_eq!(
        entry.header.origin,
        Origin::Replicated {
            source_node_id: NodeId::new(77),
            source_seq: 9,
        }
    );
    let _ = fs::remove_file(path);
}

#[test]
fn snapshot_seq_seeds_first_appended_sequence() {
    let path = temp_path("snapshot-seq-seed");
    let mut writer = WalWriter::open(
        &path,
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq: 100,
        },
    )
    .unwrap();
    // First append must follow snapshot_seq + 1 = 101, not start at 1.
    let seq = writer
        .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
        .unwrap();
    assert_eq!(seq, 101);
    assert_eq!(writer.last_sequence(), 101);
    let _ = fs::remove_file(path);
}

#[test]
fn reopen_uses_header_snapshot_seq_not_config() {
    let path = temp_path("reopen-snapshot-seq");
    // Create with snapshot_seq=100
    {
        let mut writer = WalWriter::open(
            &path,
            WalConfig {
                sync_policy: SyncPolicy::EveryN(1),
                snapshot_seq: 100,
            },
        )
        .unwrap();
        assert_eq!(writer.last_sequence(), 100);
        writer
            .append(HlcTimestamp::new(1, 0), Origin::Local, None, &changes())
            .unwrap();
    }
    // Reopen with a stale config.snapshot_seq=0 — header wins, last
    // appended sequence (101) is recovered.
    let writer = WalWriter::open(
        &path,
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq: 0,
        },
    )
    .unwrap();
    assert_eq!(writer.last_sequence(), 101);
    let _ = fs::remove_file(path);
}

#[test]
fn concurrent_open_admits_exactly_one_writer() {
    // The exclusive OS lock is the single-writer invariant. Race N threads to
    // create and open the same initially absent WAL: exactly one must win (Ok),
    // every other must observe WriterLockHeld — never a second initializer or
    // writer corrupting the log. Every winning writer is parked in a shared vec
    // and NOT dropped until all threads have finished their open attempt, so a
    // winner's lock cannot be released and re-acquired mid-race. That makes the
    // count deterministically exactly one.
    use std::sync::{Arc as StdArc, Barrier, Mutex};

    let path = temp_path("concurrent-lock");

    const THREADS: usize = 8;
    let start = StdArc::new(Barrier::new(THREADS));
    // Winners deposit their live writer here; the lock stays held until the
    // vec is dropped after every join.
    let winners: StdArc<Mutex<Vec<WalWriter>>> = StdArc::new(Mutex::new(Vec::new()));
    let path = StdArc::new(path);
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let start = StdArc::clone(&start);
            let winners = StdArc::clone(&winners);
            let path = StdArc::clone(&path);
            std::thread::spawn(move || {
                start.wait();
                match WalWriter::open(&path, WalConfig::default()) {
                    Ok(writer) => {
                        winners.lock().unwrap().push(writer);
                        true
                    }
                    Err(PersistError::WriterLockHeld) => false,
                    Err(other) => panic!("unexpected open error: {other:?}"),
                }
            })
        })
        .collect();

    let mut wins = 0_usize;
    for handle in handles {
        if handle.join().unwrap() {
            wins += 1;
        }
    }
    assert_eq!(
        wins, 1,
        "exactly one concurrent writer may hold the exclusive lock"
    );
    assert_eq!(winners.lock().unwrap().len(), 1);
    // Drop the single winning writer (releasing the OS lock) before cleanup.
    winners.lock().unwrap().clear();
    let _ = fs::remove_file(path.as_path());
}

#[test]
fn existing_zero_length_wal_is_rejected_without_initializing_in_place() {
    let path = temp_path("existing-empty");
    drop(fs::File::create(&path).unwrap());

    assert!(matches!(
        WalWriter::open(&path, WalConfig::default()),
        Err(PersistError::TruncatedFileHeader)
    ));
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    let _ = fs::remove_file(path);
}

#[test]
fn second_writer_open_returns_writer_lock_held() {
    let path = temp_path("lock");
    let _first = WalWriter::open(&path, WalConfig::default()).unwrap();
    let second = WalWriter::open(&path, WalConfig::default());
    assert!(matches!(second, Err(PersistError::WriterLockHeld)));
    drop(_first);
    // After the first writer drops, a fresh open must succeed (the
    // OS releases the file lock when the File handle closes).
    let third = WalWriter::open(&path, WalConfig::default());
    assert!(third.is_ok());
    drop(third);
    let _ = fs::remove_file(path);
}
