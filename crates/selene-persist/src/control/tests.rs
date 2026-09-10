use proptest::prelude::*;
use selene_testing::PersistenceTestPath;

use super::*;

pub(super) fn identity() -> CompatibilityIdentity {
    CompatibilityIdentity::new("fixture-profile", 1, [7; 32], [16, 0, 0], "binary", 1).unwrap()
}

pub(super) fn directory() -> (PersistenceTestPath, StoreDirectory) {
    let path = PersistenceTestPath::new();
    let dir = StoreDirectory::open(path.parent().unwrap()).unwrap();
    (path, dir)
}

// Deliberate out-of-protocol corruption fixture, not a publication API.
pub(super) fn overwrite(dir: &StoreDirectory, name: &str, bytes: &[u8]) {
    let mut file = dir.open_or_create(Path::new(name)).unwrap();
    file.set_len(0).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

// Independently construct the postcard envelope, bypassing the production
// semantic encoder so malformed identity/version/name cases reach the decoder.
pub(super) fn envelope<T: serde::Serialize>(value: &T, magic: [u8; 4]) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Envelope<'a> {
        magic: [u8; 4],
        version: u16,
        body: &'a [u8],
        digest: [u8; 32],
    }
    let body = postcard::to_stdvec(value).unwrap();
    postcard::to_stdvec(&Envelope {
        magic,
        version: 1,
        body: &body,
        digest: *blake3::hash(&body).as_bytes(),
    })
    .unwrap()
}

#[test]
fn empty_create_publish_and_reopen_preserve_durable_identity() {
    let (_path, dir) = directory();
    let mut store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    let id = store.manifest().store_id();
    let epoch = store.manifest().epoch();
    assert_ne!(id.as_bytes(), &[0; 16]);
    assert_eq!(store.manifest().generation().get(), 1);
    assert_eq!(store.publish_empty().unwrap().get(), 2);
    assert!(matches!(
        EmptyStoreControl::open(&dir, &identity()),
        Err(PersistError::WriterLockHeld)
    ));
    drop(store);
    let reopened = EmptyStoreControl::open(&dir, &identity()).unwrap();
    assert_eq!(reopened.manifest().store_id(), id);
    assert_eq!(reopened.manifest().epoch(), epoch);
    assert_eq!(reopened.manifest().generation().get(), 2);
    assert!(!dir.contains(crate::DEFAULT_WAL_FILE_NAME).unwrap());
}

#[test]
fn selected_state_reopens_and_publishes_without_unselected_ancestors() {
    let (_path, dir) = directory();
    let mut store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    store.publish_empty().unwrap();
    store.publish_empty().unwrap();
    let id = store.manifest().store_id();
    // Offline fixture pruning under the current owned writer authority.
    dir.remove(Path::new(&ManifestGeneration(1).name()))
        .unwrap();
    dir.remove(Path::new(&ManifestGeneration(2).name()))
        .unwrap();
    dir.sync().unwrap();
    drop(store);
    let mut reopened = EmptyStoreControl::open(&dir, &identity()).unwrap();
    assert_eq!(reopened.manifest().store_id(), id);
    assert_eq!(reopened.manifest().generation().get(), 3);
    assert_eq!(reopened.publish_empty().unwrap().get(), 4);
    drop(reopened);
    assert_eq!(
        EmptyStoreControl::open(&dir, &identity())
            .unwrap()
            .manifest()
            .generation()
            .get(),
        4
    );
}

#[test]
fn exclusive_create_preserves_existing_control_bytes() {
    let (_path, dir) = directory();
    drop(EmptyStoreControl::create_empty(&dir, identity()).unwrap());
    let before = read_bounded(&dir, Path::new(CURRENT_FILE_NAME)).unwrap();
    assert!(matches!(
        EmptyStoreControl::create_empty(&dir, identity()),
        Err(PersistError::Control(ControlError::AlreadyInitialized))
    ));
    assert_eq!(
        read_bounded(&dir, Path::new(CURRENT_FILE_NAME)).unwrap(),
        before
    );
}

const FAULTS: &[&str] = &[
    "manifest.create",
    "manifest.write",
    "manifest.file_sync",
    "manifest.publish",
    "manifest.dir_sync",
    "current.create",
    "current.write",
    "current.file_sync",
    "current.replace",
    "current.dir_sync",
];

#[test]
fn empty_creation_failure_matrix_never_selects_partial_control() {
    for &point in FAULTS {
        let (_path, dir) = directory();
        dir.fail_at(point);
        let error = EmptyStoreControl::create_empty(&dir, identity()).unwrap_err();
        let reopen = EmptyStoreControl::open(&dir, &identity());
        if point == "current.dir_sync" {
            assert!(matches!(
                error,
                PersistError::Control(ControlError::PublicationUncertain { .. })
            ));
            assert_eq!(reopen.unwrap().manifest().generation().get(), 1);
        } else {
            assert!(
                matches!(
                    reopen,
                    Err(PersistError::Control(ControlError::NotInitialized))
                ),
                "{point}: {reopen:?}"
            );
        }
    }
}

#[test]
fn publication_failure_matrix_reopens_old_or_new_complete_state() {
    for &point in FAULTS {
        let (_path, dir) = directory();
        let mut store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
        let id = store.manifest().store_id();
        dir.fail_at(point);
        let error = store.publish_empty().unwrap_err();
        let expected = if point == "current.dir_sync" {
            assert!(matches!(
                error,
                PersistError::Control(ControlError::PublicationUncertain { .. })
            ));
            assert!(matches!(
                store.publish_empty(),
                Err(PersistError::Control(ControlError::RequiresReopen))
            ));
            2
        } else {
            1
        };
        drop(store);
        let mut reopened = EmptyStoreControl::open(&dir, &identity()).unwrap();
        assert_eq!(reopened.manifest().store_id(), id);
        assert_eq!(reopened.manifest().epoch().get(), 1);
        assert_eq!(reopened.manifest().generation().get(), expected, "{point}");
        assert_eq!(
            reopened.publish_empty().unwrap().get(),
            expected + 1,
            "retry {point}"
        );
    }
}

#[test]
fn unpublished_create_orphans_are_not_adopted_as_a_new_store() {
    let (_path, dir) = directory();
    dir.fail_at("current.create");
    assert!(EmptyStoreControl::create_empty(&dir, identity()).is_err());
    assert!(matches!(
        EmptyStoreControl::create_empty(&dir, identity()),
        Err(PersistError::Control(ControlError::UnpublishedArtifacts))
    ));
    assert!(matches!(
        EmptyStoreControl::open(&dir, &identity()),
        Err(PersistError::Control(ControlError::NotInitialized))
    ));
}

#[test]
fn initial_staging_orphan_cannot_be_reinterpreted_as_a_legacy_store() {
    let (_path, dir) = directory();
    let staged = format!(".control.{}.tmp", uuid::Uuid::new_v4());
    overwrite(&dir, &staged, b"interrupted initial manifest write");
    assert!(matches!(
        EmptyStoreControl::open(&dir, &identity()),
        Err(PersistError::Control(ControlError::NotInitialized))
    ));
    assert!(matches!(
        EmptyStoreControl::create_empty(&dir, identity()),
        Err(PersistError::Control(ControlError::UnpublishedArtifacts))
    ));
    assert!(matches!(
        crate::WalWriter::open_in(&dir, Path::new("wal.log"), crate::WalConfig::default()),
        Err(PersistError::Directory(
            crate::DirectoryError::ControlDirectory
        ))
    ));
    assert!(matches!(
        crate::AuditLog::open(&dir.locator().join("audit.log")),
        Err(PersistError::Directory(
            crate::DirectoryError::ControlDirectory
        ))
    ));
    assert!(!dir.contains("wal.log").unwrap());
    assert!(!dir.contains("audit.log").unwrap());
}

#[test]
fn immutable_collision_does_not_overwrite_foreign_lineage() {
    let (_path, dir) = directory();
    let mut store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    let foreign = EmptyManifest {
        store_id: StoreId::fresh(),
        generation: ManifestGeneration(2),
        parent: Some(Parent {
            generation: ManifestGeneration(1),
            digest: store.selector.digest,
        }),
        ..store.manifest.clone()
    };
    let bytes = foreign.encode().unwrap();
    overwrite(&dir, &foreign.generation.name(), &bytes);
    let before = read_bounded(&dir, Path::new(CURRENT_FILE_NAME)).unwrap();
    assert!(matches!(
        store.publish_empty(),
        Err(PersistError::Control(ControlError::Lineage))
    ));
    assert_eq!(
        read_bounded(&dir, Path::new(&foreign.generation.name())).unwrap(),
        bytes
    );
    assert_eq!(
        read_bounded(&dir, Path::new(CURRENT_FILE_NAME)).unwrap(),
        before
    );
}

#[test]
fn stale_handle_and_selected_corruption_are_rejected_before_publication() {
    let (_path, dir) = directory();
    let mut store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    let first = store.selector.encode().unwrap();
    store.publish_empty().unwrap();
    overwrite(&dir, CURRENT_FILE_NAME, &first);
    assert!(matches!(
        store.publish_empty(),
        Err(PersistError::Control(ControlError::Stale))
    ));
    let current = store.selector.encode().unwrap();
    overwrite(&dir, CURRENT_FILE_NAME, &current);
    let mut selected = store.manifest.clone();
    selected.epoch = StoreEpoch(9);
    overwrite(
        &dir,
        &selected.generation.name(),
        &selected.encode().unwrap(),
    );
    assert!(matches!(
        store.publish_empty(),
        Err(PersistError::Control(ControlError::Checksum))
    ));
    assert_eq!(
        read_bounded(&dir, Path::new(CURRENT_FILE_NAME)).unwrap(),
        current
    );
    assert!(!dir.contains(ManifestGeneration(3).name()).unwrap());
}

#[test]
fn selector_identity_epoch_generation_and_child_name_are_validated() {
    for change in 0..5 {
        let (_path, dir) = directory();
        let store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
        let mut selector = store.selector.clone();
        match change {
            0 => selector.store_id = StoreId::fresh(),
            1 => selector.epoch = StoreEpoch(2),
            2 => selector.generation = ManifestGeneration(2),
            3 => selector.manifest_name = "../outside".into(),
            _ => selector.manifest_name = "/outside".into(),
        }
        drop(store);
        let bytes = envelope(&selector, *b"SLCU");
        overwrite(&dir, CURRENT_FILE_NAME, &bytes);
        assert!(
            matches!(
                EmptyStoreControl::open(&dir, &identity()),
                Err(PersistError::Control(ControlError::Lineage))
            ),
            "case {change}"
        );
        assert_eq!(
            read_bounded(&dir, Path::new(CURRENT_FILE_NAME)).unwrap(),
            bytes
        );
    }
}

#[test]
fn foreign_profile_unicode_collation_and_version_fail_before_publication() {
    let (_path, dir) = directory();
    let store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    let mut manifest = store.manifest.clone();
    drop(store);
    let before = read_bounded(&dir, Path::new(CURRENT_FILE_NAME)).unwrap();
    for change in 0..4 {
        let mut expected = identity();
        match change {
            0 => expected.profile_hash = [9; 32],
            1 => expected.profile_version = 2,
            2 => expected.unicode_version = [17, 0, 0],
            _ => expected.collation_version = 2,
        }
        assert!(matches!(
            EmptyStoreControl::open(&dir, &expected),
            Err(PersistError::Control(ControlError::Compatibility))
        ));
        assert_eq!(
            read_bounded(&dir, Path::new(CURRENT_FILE_NAME)).unwrap(),
            before
        );
    }
    manifest.format = [3, 0];
    assert!(matches!(
        EmptyManifest::decode(&envelope(&manifest, *b"SLEM")),
        Err(PersistError::Control(ControlError::UnsupportedVersion))
    ));
}

#[test]
fn checksums_bounds_trailing_bytes_and_generation_overflow_are_rejected() {
    let (_path, dir) = directory();
    let store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    let bytes = store.manifest.encode().unwrap();
    for n in 0..bytes.len() {
        assert!(EmptyManifest::decode(&bytes[..n]).is_err());
        let mut corrupt = bytes.clone();
        corrupt[n] ^= 1;
        assert!(EmptyManifest::decode(&corrupt).is_err());
    }
    let mut trailing = bytes;
    trailing.push(0);
    assert!(EmptyManifest::decode(&trailing).is_err());
    assert!(matches!(
        CurrentSelector::decode(&vec![0; MAX_CONTROL_BYTES + 1]),
        Err(PersistError::Control(ControlError::TooLarge))
    ));
    assert!(matches!(
        ManifestGeneration(u64::MAX).next(),
        Err(PersistError::Control(ControlError::GenerationExhausted))
    ));
    drop(store);
    overwrite(&dir, CURRENT_FILE_NAME, &vec![0; MAX_CONTROL_BYTES + 1]);
    assert!(matches!(
        EmptyStoreControl::open(&dir, &identity()),
        Err(PersistError::Control(ControlError::TooLarge))
    ));
}

#[test]
fn control_and_legacy_data_protocols_cannot_mix_in_either_direction() {
    let (_path, dir) = directory();
    drop(
        crate::WalWriter::open_in(&dir, Path::new("wal.log"), crate::WalConfig::default()).unwrap(),
    );
    let before = std::fs::read(dir.locator().join("wal.log")).unwrap();
    assert!(matches!(
        EmptyStoreControl::create_empty(&dir, identity()),
        Err(PersistError::Control(ControlError::MixedArtifacts(_)))
    ));
    assert_eq!(
        std::fs::read(dir.locator().join("wal.log")).unwrap(),
        before
    );

    let (_path2, dir2) = directory();
    drop(EmptyStoreControl::create_empty(&dir2, identity()).unwrap());
    assert!(matches!(
        crate::WalWriter::open_in(&dir2, Path::new("wal.log"), crate::WalConfig::default()),
        Err(PersistError::Directory(
            crate::DirectoryError::ControlDirectory
        ))
    ));
    assert!(matches!(
        crate::SnapshotBuilder::new_in(&dir2, crate::SnapshotConfig::default()).finalize(),
        Err(PersistError::Directory(
            crate::DirectoryError::ControlDirectory
        ))
    ));
    assert!(matches!(
        crate::AuditLog::open(&dir2.locator().join("audit.log")),
        Err(PersistError::Directory(
            crate::DirectoryError::ControlDirectory
        ))
    ));
    assert!(matches!(
        crate::recover_guarded(
            &PersistenceReadGuard::acquire_in(&dir2).unwrap(),
            &crate::ProviderRegistry::new()
        ),
        Err(PersistError::Directory(
            crate::DirectoryError::ControlDirectory
        ))
    ));
    assert!(!dir2.contains("wal.log").unwrap());
}

proptest! {
    #[test]
    fn bounded_control_decoders_handle_arbitrary_bytes(bytes in prop::collection::vec(any::<u8>(), 0..MAX_CONTROL_BYTES + 32)) {
        let _ = EmptyManifest::decode(&bytes);
        let _ = CurrentSelector::decode(&bytes);
    }
}
