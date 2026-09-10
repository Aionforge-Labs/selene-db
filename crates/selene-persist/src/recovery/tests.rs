//! Unit tests for recovery orchestration (split from recovery.rs to keep
//! the production module under the 700-LOC file cap).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use proptest::prelude::*;
use selene_core::{Change, HlcTimestamp, NodeId, Origin};

use super::*;
use crate::{
    RecoveryProvider, RecoveryResult, SectionCompression, SnapshotBuilder, SnapshotConfig,
    WalConfig, WalWriter, snapshot_path,
};

mod replay;

mod concurrency;

#[derive(Clone, Debug, PartialEq)]
enum Event {
    Section { sub: [u8; 4], bytes: Vec<u8> },
    Change(Box<Change>),
}

struct RecordingProvider {
    tag: [u8; 4],
    events: Mutex<Vec<Event>>,
    fail_section: bool,
    fail_change: bool,
}

impl RecordingProvider {
    fn new(tag: [u8; 4]) -> Arc<Self> {
        Self::with_failures(tag, false, false)
    }

    fn with_failures(tag: [u8; 4], fail_section: bool, fail_change: bool) -> Arc<Self> {
        Arc::new(Self {
            tag,
            events: Mutex::new(Vec::new()),
            fail_section,
            fail_change,
        })
    }

    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

impl RecoveryProvider for RecordingProvider {
    fn provider_tag(&self) -> [u8; 4] {
        self.tag
    }

    fn read_section(&self, sub: [u8; 4], bytes: &[u8]) -> RecoveryResult<()> {
        if self.fail_section {
            return Err(Box::new(TestProviderError("read_section failed")));
        }
        self.events.lock().unwrap().push(Event::Section {
            sub,
            bytes: bytes.to_vec(),
        });
        Ok(())
    }

    fn on_change(&self, change: &Change) -> RecoveryResult<()> {
        if self.fail_change {
            return Err(Box::new(TestProviderError("on_change failed")));
        }
        self.events
            .lock()
            .unwrap()
            .push(Event::Change(Box::new(change.clone())));
        Ok(())
    }
}

#[derive(Debug)]
struct TestProviderError(&'static str);

impl fmt::Display for TestProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for TestProviderError {}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-recovery-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    dir
}

fn registry(providers: &[Arc<RecordingProvider>]) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    for provider in providers {
        let provider: Arc<dyn RecoveryProvider> = provider.clone();
        registry.register(provider).unwrap();
    }
    registry
}

fn change(id: u64) -> Change {
    Change::NodeDeleted {
        id: NodeId::new(id),
    }
}

fn changes(ids: &[u64]) -> Vec<Change> {
    ids.iter().copied().map(change).collect()
}

fn write_snapshot(dir: &Path, sequence: u64, sections: &[([u8; 4], [u8; 4], Vec<u8>)]) -> PathBuf {
    let mut builder = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.to_path_buf(),
        sequence,
        compression: SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    for (provider, sub, bytes) in sections {
        builder.add_section(*provider, *sub, bytes.clone()).unwrap();
    }
    let outcome = builder.finalize().unwrap();
    assert_eq!(outcome.snapshot_seq, sequence);
    snapshot_path(dir, sequence)
}

fn write_wal(dir: &Path, snapshot_seq: u64, entries: &[(Origin, Vec<Change>)]) -> PathBuf {
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    {
        let mut writer = WalWriter::open(
            &path,
            WalConfig {
                snapshot_seq,
                ..WalConfig::default()
            },
        )
        .unwrap();
        for (index, (origin, changes)) in entries.iter().enumerate() {
            writer
                .append(
                    HlcTimestamp::new(index as u64 + 1, 0),
                    *origin,
                    None,
                    changes,
                )
                .unwrap();
        }
    }
    path
}

fn assert_provider_failed(
    error: PersistError,
    expected_provider: [u8; 4],
    expected_sub: Option<[u8; 4]>,
    expected_source: &str,
) {
    match error {
        PersistError::ProviderFailed {
            provider,
            sub,
            source,
        } => {
            assert_eq!(provider, expected_provider);
            assert_eq!(sub, expected_sub);
            assert_eq!(source.to_string(), expected_source);
        }
        other => panic!("expected provider failure, got {other:?}"),
    }
}

#[test]
fn recover_empty_dir_returns_empty_outcome() {
    let dir = temp_dir("empty");
    let outcome = recover(&dir, &ProviderRegistry::new()).unwrap();
    assert_eq!(outcome, RecoveryOutcome::empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_missing_directory_is_not_an_empty_database() {
    let dir = temp_dir("missing-directory");
    fs::remove_dir_all(&dir).unwrap();

    let error = recover(&dir, &ProviderRegistry::new()).unwrap_err();

    assert!(
        matches!(error, PersistError::Io(source) if source.kind() == std::io::ErrorKind::NotFound)
    );
}

#[cfg(unix)]
#[test]
fn recover_dangling_directory_alias_is_not_an_empty_database() {
    use std::os::unix::fs::symlink;

    let root = temp_dir("dangling-directory-alias");
    let alias = root.join("live");
    symlink(root.join("missing"), &alias).unwrap();

    let error = recover(&alias, &ProviderRegistry::new()).unwrap_err();

    assert!(
        matches!(error, PersistError::Io(source) if source.kind() == std::io::ErrorKind::NotFound)
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn recover_snapshot_only_routes_sections() {
    let dir = temp_dir("snapshot-only");
    write_snapshot(
        &dir,
        12,
        &[
            (*b"CORE", *b"META", b"meta".to_vec()),
            (*b"DEMO", *b"SUBT", b"demo".to_vec()),
        ],
    );
    let core = RecordingProvider::new(*b"CORE");
    let demo = RecordingProvider::new(*b"DEMO");
    let outcome = recover(&dir, &registry(&[core.clone(), demo.clone()])).unwrap();
    assert_eq!(outcome.applied_snapshot_seq, 12);
    assert_eq!(outcome.last_wal_seq, 12);
    assert_eq!(outcome.providers_invoked, vec![*b"CORE", *b"DEMO"]);
    assert_eq!(outcome.snapshot_providers_invoked, vec![*b"CORE", *b"DEMO"]);
    assert_eq!(
        core.events(),
        vec![Event::Section {
            sub: *b"META",
            bytes: b"meta".to_vec()
        }]
    );
    assert_eq!(
        demo.events(),
        vec![Event::Section {
            sub: *b"SUBT",
            bytes: b"demo".to_vec()
        }]
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_unknown_provider_is_rejected() {
    let dir = temp_dir("unknown");
    write_snapshot(&dir, 1, &[(*b"MISS", *b"BODY", b"body".to_vec())]);
    let core = RecordingProvider::new(*b"CORE");
    let err = recover(&dir, &registry(std::slice::from_ref(&core))).unwrap_err();
    assert!(matches!(
        err,
        PersistError::UnknownProvider { provider, sub }
            if provider == *b"MISS" && sub == *b"BODY"
    ));
    assert!(core.events().is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_wal_only_no_snapshot() {
    let dir = temp_dir("wal-only");
    write_wal(
        &dir,
        0,
        &[
            (Origin::Local, changes(&[1])),
            (Origin::Local, changes(&[2])),
            (Origin::Local, changes(&[3])),
        ],
    );
    let core = RecordingProvider::new(*b"CORE");
    let outcome = recover(&dir, &registry(std::slice::from_ref(&core))).unwrap();
    assert_eq!(outcome.applied_snapshot_seq, 0);
    assert_eq!(outcome.last_wal_seq, 3);
    assert_eq!(outcome.snapshot_providers_invoked, Vec::<[u8; 4]>::new());
    assert_eq!(outcome.wal_changes_applied, 3);
    assert_eq!(
        core.events(),
        changes(&[1, 2, 3])
            .into_iter()
            .map(|change| Event::Change(Box::new(change)))
            .collect::<Vec<_>>()
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_snapshot_and_wal_streams_only_post_snapshot_changes() {
    let dir = temp_dir("snapshot-wal");
    write_snapshot(&dir, 100, &[(*b"CORE", *b"META", b"meta".to_vec())]);
    write_wal(
        &dir,
        100,
        &[
            (Origin::Local, changes(&[101])),
            (Origin::Local, changes(&[102])),
            (Origin::Local, changes(&[103])),
        ],
    );
    let core = RecordingProvider::new(*b"CORE");
    let outcome = recover(&dir, &registry(std::slice::from_ref(&core))).unwrap();
    assert_eq!(outcome.applied_snapshot_seq, 100);
    assert_eq!(outcome.last_wal_seq, 103);
    assert_eq!(outcome.snapshot_providers_invoked, vec![*b"CORE"]);
    assert_eq!(outcome.wal_changes_applied, 3);
    assert_eq!(
        core.events(),
        vec![
            Event::Section {
                sub: *b"META",
                bytes: b"meta".to_vec()
            },
            Event::Change(Box::new(change(101))),
            Event::Change(Box::new(change(102))),
            Event::Change(Box::new(change(103))),
        ]
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_wal_snapshot_mismatch_is_rejected() {
    let dir = temp_dir("mismatch");
    write_snapshot(&dir, 50, &[]);
    write_wal(&dir, 100, &[]);
    let err = recover(&dir, &ProviderRegistry::new()).unwrap_err();
    assert!(matches!(
        err,
        PersistError::WalSnapshotMismatch {
            wal_snapshot_seq: 100,
            snapshot_seq: 50
        }
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_replicated_dedupe_skips_repeated_entries() {
    let dir = temp_dir("dedupe");
    let origin = Origin::Replicated {
        source_node_id: NodeId::new(9),
        source_seq: 77,
    };
    write_wal(
        &dir,
        0,
        &[(origin, changes(&[1, 2])), (origin, changes(&[3, 4]))],
    );
    let core = RecordingProvider::new(*b"CORE");
    let outcome = recover(&dir, &registry(std::slice::from_ref(&core))).unwrap();
    assert_eq!(outcome.last_wal_seq, 2);
    assert_eq!(outcome.wal_changes_applied, 2);
    assert_eq!(outcome.replicated_changes_deduplicated, 2);
    assert_eq!(
        core.events(),
        vec![
            Event::Change(Box::new(change(1))),
            Event::Change(Box::new(change(2)))
        ]
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_provider_read_section_error_propagates() {
    let dir = temp_dir("section-fail");
    write_snapshot(&dir, 1, &[(*b"CORE", *b"META", b"meta".to_vec())]);
    write_wal(&dir, 1, &[(Origin::Local, changes(&[2]))]);
    let core = RecordingProvider::with_failures(*b"CORE", true, false);
    let err = recover(&dir, &registry(std::slice::from_ref(&core))).unwrap_err();
    assert_provider_failed(err, *b"CORE", Some(*b"META"), "read_section failed");
    assert!(core.events().is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_provider_on_change_error_propagates() {
    let dir = temp_dir("change-fail");
    write_wal(&dir, 0, &[(Origin::Local, changes(&[1]))]);
    let core = RecordingProvider::with_failures(*b"CORE", false, true);
    let err = recover(&dir, &registry(std::slice::from_ref(&core))).unwrap_err();
    assert_provider_failed(err, *b"CORE", None, "on_change failed");
    assert!(core.events().is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_truncated_wal_stops_at_last_valid_sequence() {
    let dir = temp_dir("truncated-wal");
    let path = write_wal(
        &dir,
        0,
        &[
            (Origin::Local, changes(&[1])),
            (Origin::Local, changes(&[2])),
            (Origin::Local, changes(&[3])),
        ],
    );
    let len = fs::metadata(&path).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(len - 1)
        .unwrap();
    let core = RecordingProvider::new(*b"CORE");
    let outcome = recover(&dir, &registry(std::slice::from_ref(&core))).unwrap();
    assert_eq!(outcome.last_wal_seq, 2);
    assert_eq!(outcome.wal_changes_applied, 2);
    assert_eq!(
        core.events(),
        vec![
            Event::Change(Box::new(change(1))),
            Event::Change(Box::new(change(2)))
        ]
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_corrupt_snapshot_body_hash_is_hard_failure() {
    let dir = temp_dir("corrupt-snapshot");
    let path = write_snapshot(&dir, 1, &[(*b"CORE", *b"META", b"meta".to_vec())]);
    let mut bytes = fs::read(&path).unwrap();
    *bytes.last_mut().unwrap() ^= 0x01;
    fs::write(&path, bytes).unwrap();
    let core = RecordingProvider::new(*b"CORE");
    let err = recover(&dir, &registry(std::slice::from_ref(&core))).unwrap_err();
    assert!(matches!(err, PersistError::BodyHashMismatch { .. }));
    assert!(core.events().is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn recover_rejects_symlink_active_wal_before_provider_callbacks() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("symlink-active-wal");
    write_snapshot(&dir, 1, &[(*b"CORE", *b"META", b"meta".to_vec())]);
    let backing = dir.join("backing.log");
    drop(
        WalWriter::open(
            &backing,
            WalConfig {
                snapshot_seq: 1,
                ..WalConfig::default()
            },
        )
        .unwrap(),
    );
    let active = dir.join(DEFAULT_WAL_FILE_NAME);
    let anchored_active = dir.canonicalize().unwrap().join(DEFAULT_WAL_FILE_NAME);
    symlink(&backing, &active).unwrap();
    let core = RecordingProvider::new(*b"CORE");

    let error = recover(&dir, &registry(std::slice::from_ref(&core))).unwrap_err();

    assert!(matches!(
        error,
        PersistError::WalPathNotRegular { path } if path == anchored_active
    ));
    assert!(core.events().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

fn tag_strategy() -> impl Strategy<Value = [u8; 4]> {
    (65_u8..=90, 65_u8..=90, 65_u8..=90, 65_u8..=90).prop_map(|(a, b, c, d)| [a, b, c, d])
}

proptest! {
    #[test]
    fn recovery_replays_exact_change_sequence_to_every_provider(
        tags in proptest::collection::vec(tag_strategy(), 1..=32)
            .prop_filter("provider tags must be unique", |tags| {
                let mut seen = std::collections::BTreeSet::new();
                tags.iter().all(|tag| seen.insert(*tag))
            }),
        ids in proptest::collection::vec(1_u64..10_000, 1..=32),
    ) {
        let dir = temp_dir("prop-replay");
        let entries: Vec<_> = ids
            .iter()
            .copied()
            .map(|id| (Origin::Local, vec![change(id)]))
            .collect();
        write_wal(&dir, 0, &entries);
        let providers: Vec<_> = tags.iter().copied().map(RecordingProvider::new).collect();
        let outcome = recover(&dir, &registry(&providers)).unwrap();
        let expected_changes = changes(&ids);
        prop_assert_eq!(outcome.wal_changes_applied, expected_changes.len() as u64);
        for provider in providers {
            let observed: Vec<_> = provider
                .events()
                .into_iter()
                .filter_map(|event| match event {
                    Event::Change(change) => Some(*change),
                    Event::Section { .. } => None,
                })
                .collect();
            prop_assert_eq!(observed.as_slice(), expected_changes.as_slice());
        }
        let _ = fs::remove_dir_all(dir);
    }
}
