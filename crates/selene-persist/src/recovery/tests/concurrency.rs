//! Recovery serialization against MANIFEST epoch mutation.

use std::fs::File;
use std::io::Read;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use selene_core::{Change, HlcTimestamp, Origin};

use super::{Event, RecordingProvider, change, registry, temp_dir};
use crate::manifest_lock::set_contention_hook;
use crate::{
    DEFAULT_WAL_FILE_NAME, MANIFEST_LOCK_FILE_NAME, Manifest, PersistError, PersistenceReadGuard,
    RecoveryProvider, RecoveryResult, SectionCompression, SnapshotBuilder, SnapshotConfig,
    SyncPolicy, WalConfig, WalWriter, recover, snapshot_path,
};

struct GatedProvider {
    snapshot_entered: SyncSender<()>,
    snapshot_release: Mutex<Receiver<()>>,
    wal_entered: SyncSender<()>,
    wal_release: Mutex<Receiver<()>>,
    changes: Mutex<Vec<Change>>,
}

impl RecoveryProvider for GatedProvider {
    fn provider_tag(&self) -> [u8; 4] {
        *b"CORE"
    }

    fn read_section(&self, _sub: [u8; 4], _bytes: &[u8]) -> RecoveryResult<()> {
        self.snapshot_entered.send(()).unwrap();
        self.snapshot_release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect("snapshot callback release arrives before timeout");
        Ok(())
    }

    fn on_changes(&self, changes: &[Change]) -> RecoveryResult<()> {
        self.wal_entered.send(()).unwrap();
        self.wal_release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect("WAL callback release arrives before timeout");
        self.changes.lock().unwrap().extend_from_slice(changes);
        Ok(())
    }
}

fn builder(dir: &std::path::Path, sequence: u64, bytes: &[u8]) -> SnapshotBuilder {
    let mut builder = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.to_path_buf(),
        sequence,
        compression: SectionCompression::None,
        fsync: true,
    })
    .unwrap();
    builder
        .add_section(*b"CORE", *b"META", bytes.to_vec())
        .unwrap();
    builder
}

#[test]
fn recovery_blocks_rotation_through_snapshot_and_wal_replay() {
    let dir = temp_dir("rotation-serialization");
    let wal = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &wal,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();
    writer
        .append(HlcTimestamp::new(1, 0), Origin::Local, None, &[change(1)])
        .unwrap();
    writer
        .rotate_with_manifest(builder(&dir, 1, b"epoch-one"))
        .unwrap();
    writer
        .append(HlcTimestamp::new(2, 0), Origin::Local, None, &[change(2)])
        .unwrap();
    writer.flush().unwrap();

    let (snapshot_entered_tx, snapshot_entered_rx) = sync_channel(0);
    let (snapshot_release_tx, snapshot_release_rx) = sync_channel(0);
    let (wal_entered_tx, wal_entered_rx) = sync_channel(0);
    let (wal_release_tx, wal_release_rx) = sync_channel(0);
    let provider = Arc::new(GatedProvider {
        snapshot_entered: snapshot_entered_tx,
        snapshot_release: Mutex::new(snapshot_release_rx),
        wal_entered: wal_entered_tx,
        wal_release: Mutex::new(wal_release_rx),
        changes: Mutex::new(Vec::new()),
    });
    let recovery_provider: Arc<dyn RecoveryProvider> = provider.clone();
    let recovery_dir = dir.clone();
    let (recovery_done_tx, recovery_done_rx) = sync_channel(1);
    let recovery = thread::spawn(move || {
        let mut registry = crate::ProviderRegistry::new();
        registry.register(recovery_provider).unwrap();
        recovery_done_tx
            .send(recover(&recovery_dir, &registry))
            .unwrap();
    });
    snapshot_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("recovery reaches the old snapshot callback");

    let (contended_tx, contended_rx) = sync_channel(0);
    let (rotation_done_tx, rotation_done_rx) = sync_channel(0);
    let rotation_dir = dir.clone();
    let rotation = thread::spawn(move || {
        set_contention_hook(move || contended_tx.send(()).unwrap());
        let outcome = writer.rotate_with_manifest(builder(&rotation_dir, 2, b"epoch-two"));
        rotation_done_tx.send(outcome).unwrap();
    });
    contended_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("rotation reaches the recovery read lock");
    assert_eq!(Manifest::read(&dir).unwrap().unwrap().live_snapshot_seq, 1);

    snapshot_release_tx.send(()).unwrap();
    wal_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("recovery reaches the old WAL tail callback");
    assert!(
        matches!(
            rotation_done_rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ),
        "rotation must remain blocked throughout WAL replay"
    );
    assert_eq!(Manifest::read(&dir).unwrap().unwrap().live_snapshot_seq, 1);
    wal_release_tx.send(()).unwrap();

    let recovered = recovery_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("recovery completes after both callback gates release")
        .unwrap();
    recovery.join().unwrap();
    assert_eq!(recovered.applied_snapshot_seq, 1);
    assert_eq!(recovered.last_wal_seq, 2);
    assert_eq!(recovered.wal_changes_applied, 1);
    assert_eq!(*provider.changes.lock().unwrap(), vec![change(2)]);
    let rotated = rotation_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("rotation completes after recovery releases")
        .unwrap();
    assert_eq!(rotated.snapshot_sequence(), 2);
    rotation.join().unwrap();
    assert_eq!(Manifest::read(&dir).unwrap().unwrap().live_snapshot_seq, 2);

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recovery_lock_obstruction_fails_before_provider_callbacks() {
    let dir = temp_dir("lock-obstruction");
    std::fs::create_dir(dir.join(MANIFEST_LOCK_FILE_NAME)).unwrap();
    let core = RecordingProvider::new(*b"CORE");

    let error = recover(&dir, &registry(std::slice::from_ref(&core))).unwrap_err();

    assert!(matches!(
        error,
        PersistError::Directory(crate::DirectoryError::NotRegular(_))
    ));
    assert!(core.events().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn guarded_bounded_backup_restores_selected_prefix_while_rotation_waits() {
    let source = temp_dir("guarded-backup-source");
    let destination = temp_dir("guarded-backup-destination");
    let wal = source.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &wal,
        WalConfig {
            sync_policy: SyncPolicy::OnFlushOnly,
            snapshot_seq: 0,
        },
    )
    .unwrap();
    writer
        .append(HlcTimestamp::new(1, 0), Origin::Local, None, &[change(1)])
        .unwrap();
    writer
        .rotate_with_manifest(builder(&source, 1, b"epoch-one"))
        .unwrap();
    writer
        .append(HlcTimestamp::new(2, 0), Origin::Local, None, &[change(2)])
        .unwrap();
    writer.flush().unwrap();

    let guard = PersistenceReadGuard::acquire(&source).unwrap();
    let manifest = guard.read_manifest().unwrap().unwrap();
    assert_eq!(manifest.live_snapshot_seq, 1);
    let wal_prefix_len = File::open(&wal).unwrap().metadata().unwrap().len();

    // Appends do not take the epoch lock. This later commit must stay outside
    // the captured prefix even though it is present before the copy begins.
    writer
        .append(HlcTimestamp::new(3, 0), Origin::Local, None, &[change(3)])
        .unwrap();
    writer.flush().unwrap();

    let (contended_tx, contended_rx) = sync_channel(0);
    let (rotation_done_tx, rotation_done_rx) = sync_channel(1);
    let rotation_source = source.clone();
    let rotation = thread::spawn(move || {
        set_contention_hook(move || contended_tx.send(()).unwrap());
        rotation_done_tx
            .send(writer.rotate_with_manifest(builder(&rotation_source, 3, b"epoch-three")))
            .unwrap();
    });
    contended_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("rotation waits for the backup read guard");

    std::fs::copy(
        snapshot_path(guard.dir(), manifest.live_snapshot_seq),
        snapshot_path(&destination, manifest.live_snapshot_seq),
    )
    .unwrap();
    let mut source_wal = File::open(&wal).unwrap().take(wal_prefix_len);
    let mut destination_wal = File::create(destination.join(DEFAULT_WAL_FILE_NAME)).unwrap();
    assert_eq!(
        std::io::copy(&mut source_wal, &mut destination_wal).unwrap(),
        wal_prefix_len
    );
    assert!(matches!(
        rotation_done_rx.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    ));
    drop(guard);

    rotation_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("rotation completes after backup releases")
        .unwrap();
    rotation.join().unwrap();

    let core = RecordingProvider::new(*b"CORE");
    let outcome = recover(&destination, &registry(std::slice::from_ref(&core))).unwrap();
    assert_eq!(outcome.applied_snapshot_seq, 1);
    assert_eq!(outcome.last_wal_seq, 2);
    assert_eq!(
        core.events(),
        vec![
            Event::Section {
                sub: *b"META",
                bytes: b"epoch-one".to_vec(),
            },
            Event::Change(Box::new(change(2))),
        ]
    );

    std::fs::remove_dir_all(source).unwrap();
    std::fs::remove_dir_all(destination).unwrap();
}
