//! Selected-state validation is complete without retaining unselected history.

use super::tests::{directory, envelope, identity, overwrite};
use super::*;

#[test]
fn successful_open_and_publish_use_two_payload_reads_at_any_history_length() {
    for generation in [1, 32, 256] {
        let (_path, dir) = directory();
        let mut store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
        for _ in 1..generation {
            store.publish_empty().unwrap();
        }
        let id = store.manifest.store_id;
        drop(store);
        dir.reset_control_payload_opens();
        let mut reopened = EmptyStoreControl::open(&dir, &identity()).unwrap();
        assert_eq!(
            dir.control_payload_opens(),
            2,
            "retained generation {generation}"
        );
        // Fixture pruning is offline with respect to readers, under the owned
        // control writer. The selected manifest and CURRENT are never removed.
        for old in 1..generation {
            dir.remove(Path::new(&ManifestGeneration(old).name()))
                .unwrap();
        }
        dir.sync().unwrap();
        dir.reset_control_payload_opens();
        assert_eq!(reopened.publish_empty().unwrap().get(), generation + 1);
        assert_eq!(
            dir.control_payload_opens(),
            2,
            "publication base {generation}"
        );
        drop(reopened);
        dir.reset_control_payload_opens();
        let reopened = EmptyStoreControl::open(&dir, &identity()).unwrap();
        assert_eq!(reopened.manifest.store_id, id);
        assert_eq!(
            dir.control_payload_opens(),
            2,
            "pruned generation {generation}"
        );
    }
}

#[test]
fn corrupt_or_oversized_unselected_payloads_are_not_read_or_promoted() {
    let (_path, dir) = directory();
    let mut store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    store.publish_empty().unwrap();
    let selected = store.manifest.clone();
    overwrite(
        &dir,
        &ManifestGeneration(1).name(),
        &vec![0xff; MAX_CONTROL_BYTES + 1],
    );
    // A newer failed-publication orphan is not an implicit CURRENT selector.
    overwrite(
        &dir,
        &ManifestGeneration(3).name(),
        b"invalid unselected bytes",
    );
    drop(store);
    dir.reset_control_payload_opens();
    let mut reopened = EmptyStoreControl::open(&dir, &identity()).unwrap();
    assert_eq!(reopened.manifest, selected);
    assert_eq!(dir.control_payload_opens(), 2);
    // Reopen ignores the orphan; publishing that same generation still refuses
    // to overwrite different bytes, preserving the no-overwrite protocol.
    assert!(matches!(
        reopened.publish_empty(),
        Err(PersistError::Control(ControlError::Lineage))
    ));
}

#[test]
fn selected_pair_rejects_foreign_manifest_identity_even_with_matching_digest() {
    for field in 0..3 {
        let (_path, dir) = directory();
        let store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
        let mut manifest = store.manifest.clone();
        let mut selector = store.selector.clone();
        match field {
            0 => manifest.store_id = StoreId::fresh(),
            1 => manifest.epoch = StoreEpoch(2),
            _ => {
                manifest.generation = ManifestGeneration(2);
                manifest.parent = Some(Parent {
                    generation: ManifestGeneration(1),
                    digest: selector.digest,
                });
            }
        }
        let bytes = manifest.encode().unwrap();
        selector.digest = *blake3::hash(&bytes).as_bytes();
        drop(store);
        overwrite(&dir, &selector.manifest_name, &bytes);
        overwrite(&dir, CURRENT_FILE_NAME, &selector.encode().unwrap());
        assert!(matches!(
            EmptyStoreControl::open(&dir, &identity()),
            Err(PersistError::Control(ControlError::Lineage))
        ));
    }
}

#[test]
fn selected_pair_still_validates_format_compatibility_and_parent_structure() {
    for field in 0..4 {
        let (_path, dir) = directory();
        let store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
        let mut manifest = store.manifest.clone();
        let mut selector = store.selector.clone();
        match field {
            0 => manifest.format = [2, 1],
            1 => manifest.identity.unicode_version = [17, 0, 0],
            2 => manifest.identity.collation_version += 1,
            _ => {
                manifest.parent = Some(Parent {
                    generation: ManifestGeneration(1),
                    digest: [7; 32],
                })
            }
        }
        let bytes = envelope(&manifest, *b"SLEM");
        selector.digest = *blake3::hash(&bytes).as_bytes();
        drop(store);
        overwrite(&dir, &selector.manifest_name, &bytes);
        overwrite(&dir, CURRENT_FILE_NAME, &selector.encode().unwrap());
        let error = EmptyStoreControl::open(&dir, &identity()).unwrap_err();
        match field {
            0 => assert!(matches!(
                error,
                PersistError::Control(ControlError::UnsupportedVersion)
            )),
            1 | 2 => assert!(matches!(
                error,
                PersistError::Control(ControlError::Compatibility)
            )),
            _ => assert!(matches!(
                error,
                PersistError::Control(ControlError::Lineage)
            )),
        }
    }
}

#[test]
fn complete_selected_state_can_be_copied_without_ancestor_files() {
    let (_source_path, source) = directory();
    let mut store = EmptyStoreControl::create_empty(&source, identity()).unwrap();
    store.publish_empty().unwrap();
    store.publish_empty().unwrap();
    let manifest = store.manifest.clone();
    let selector = store.selector.clone();
    let (_copy_path, copy) = directory();
    let _copy_owner = StoreWriter::acquire(&copy).unwrap();
    overwrite(&copy, &selector.manifest_name, &manifest.encode().unwrap());
    overwrite(&copy, CURRENT_FILE_NAME, &selector.encode().unwrap());
    drop(_copy_owner);
    let copied = EmptyStoreControl::open(&copy, &identity()).unwrap();
    assert_eq!(copied.manifest, manifest);
    assert!(!copy.same_directory(&source).unwrap());
    // No claim of directory-authenticated StoreId or rollback resistance.
}

#[test]
fn maximum_generation_opens_without_history_but_cannot_advance() {
    let (_path, dir) = directory();
    let store = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    let mut manifest = store.manifest.clone();
    manifest.generation = ManifestGeneration(u64::MAX);
    manifest.parent = Some(Parent {
        generation: ManifestGeneration(u64::MAX - 1),
        digest: [7; 32],
    });
    let bytes = manifest.encode().unwrap();
    let selector = CurrentSelector::from_manifest(&manifest, *blake3::hash(&bytes).as_bytes());
    overwrite(&dir, &selector.manifest_name, &bytes);
    overwrite(&dir, CURRENT_FILE_NAME, &selector.encode().unwrap());
    drop(store);
    let mut reopened = EmptyStoreControl::open(&dir, &identity()).unwrap();
    let before = read_bounded(&dir, Path::new(CURRENT_FILE_NAME)).unwrap();
    assert!(matches!(
        reopened.publish_empty(),
        Err(PersistError::Control(ControlError::GenerationExhausted))
    ));
    assert_eq!(
        read_bounded(&dir, Path::new(CURRENT_FILE_NAME)).unwrap(),
        before
    );
}
