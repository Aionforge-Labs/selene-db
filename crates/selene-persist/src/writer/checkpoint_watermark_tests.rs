use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{HlcTimestamp, Origin};

use crate::{
    DEFAULT_WAL_FILE_NAME, Manifest, PersistError, SectionCompression, SnapshotBuilder,
    SnapshotConfig, SyncPolicy, WalConfig, WalReader, WalWriter,
};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-checkpoint-watermark-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    dir
}

fn builder(dir: &std::path::Path, sequence: u64) -> SnapshotBuilder {
    let mut builder = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.to_path_buf(),
        sequence,
        compression: SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    builder
        .add_section(*b"CORE", *b"META", b"empty-graph".to_vec())
        .unwrap();
    builder
}

#[test]
fn checkpoint_watermark_rotates_fresh_wal_at_sequence_one() {
    let dir = temp_dir("fresh");
    let active = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &active,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();

    let outcome = writer
        .rotate_with_checkpoint_watermark(builder(&dir, 1), HlcTimestamp::new(1, 0))
        .unwrap();

    assert_eq!(outcome.snapshot_sequence(), 1);
    assert_eq!(writer.snapshot_seq(), 1);
    assert_eq!(writer.last_sequence(), 1);
    assert_eq!(WalReader::open(&active).unwrap().snapshot_seq(), 1);
    let archive = outcome.archived_path().unwrap();
    let entry = WalReader::open(archive)
        .unwrap()
        .iterate(|_| true)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .into_entry()
        .unwrap();
    assert!(entry.header.is_checkpoint_watermark());
    assert!(entry.header.principal.is_none());
    assert!(entry.changes.is_empty());
    assert_eq!(Manifest::read(&dir).unwrap().unwrap().live_snapshot_seq, 1);

    drop(writer);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn checkpoint_watermark_append_defers_fsync_to_rotation_barrier() {
    let dir = temp_dir("deferred-fsync");
    let active = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(&active, WalConfig::default()).unwrap();

    writer
        .append_checkpoint_watermark_record(HlcTimestamp::new(1, 0))
        .unwrap();

    assert_eq!(writer.entries_since_fsync, 1);
    writer.flush().unwrap();
    assert_eq!(writer.entries_since_fsync, 0);

    drop(writer);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn checkpoint_rejects_ahead_manifest_before_appending_watermark() {
    let dir = temp_dir("manifest-ahead");
    let active = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(&active, WalConfig::default()).unwrap();
    writer
        .append(HlcTimestamp::new(1, 0), Origin::Local, None, &[])
        .unwrap();
    Manifest {
        live_snapshot_seq: 2,
        active_wal_header_seq: 2,
        compaction_epoch: 0,
        active_wal: DEFAULT_WAL_FILE_NAME.to_owned(),
        archived_wal_seqs: vec![2],
    }
    .write_atomic_with_authority(writer.authority())
    .unwrap();
    let len_before = std::fs::metadata(&active).unwrap().len();

    let error = writer
        .rotate_with_checkpoint_watermark(builder(&dir, 2), HlcTimestamp::new(2, 0))
        .unwrap_err();

    assert!(matches!(
        error,
        PersistError::WalRotationManifestAhead {
            manifest_sequence: 2,
            snapshot_sequence: 1,
        }
    ));
    assert_eq!(writer.last_sequence(), 1);
    assert_eq!(std::fs::metadata(&active).unwrap().len(), len_before);
    let entry = WalReader::open(&active)
        .unwrap()
        .iterate(|_| true)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .into_entry()
        .unwrap();
    assert!(!entry.header.is_checkpoint_watermark());
    assert!(matches!(
        writer.flush(),
        Err(PersistError::WalWriterPoisoned)
    ));

    drop(writer);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn checkpoint_sequence_mismatch_does_not_mutate_or_poison_writer() {
    let dir = temp_dir("sequence-mismatch");
    let active = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(&active, WalConfig::default()).unwrap();
    let len_before = std::fs::metadata(&active).unwrap().len();

    let error = writer
        .rotate_with_checkpoint_watermark(builder(&dir, 2), HlcTimestamp::new(1, 0))
        .unwrap_err();

    assert!(matches!(
        error,
        PersistError::WalCheckpointSequenceMismatch {
            snapshot_seq: 2,
            expected_sequence: 1,
        }
    ));
    assert_eq!(writer.last_sequence(), 0);
    assert_eq!(std::fs::metadata(&active).unwrap().len(), len_before);
    assert_eq!(
        writer
            .append(HlcTimestamp::new(2, 0), Origin::Local, None, &[])
            .unwrap(),
        1
    );

    drop(writer);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn exhausted_sequence_rejects_append_and_checkpoint_without_mutation() {
    let dir = temp_dir("sequence-exhausted");
    let active = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &active,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: u64::MAX,
        },
    )
    .unwrap();
    let len_before = std::fs::metadata(&active).unwrap().len();

    assert!(matches!(
        writer.append(HlcTimestamp::new(1, 0), Origin::Local, None, &[]),
        Err(PersistError::WalSequenceExhausted {
            last_sequence: u64::MAX,
        })
    ));
    assert!(matches!(
        writer.rotate_with_checkpoint_watermark(builder(&dir, u64::MAX), HlcTimestamp::new(2, 0),),
        Err(PersistError::WalSequenceExhausted {
            last_sequence: u64::MAX,
        })
    ));
    assert_eq!(writer.last_sequence(), u64::MAX);
    assert_eq!(std::fs::metadata(&active).unwrap().len(), len_before);

    drop(writer);
    std::fs::remove_dir_all(dir).unwrap();
}
