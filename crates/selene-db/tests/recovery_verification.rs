//! Public disk verification uses full open readiness while another owner holds LOCK.
use selene_db::*;
use std::{collections::BTreeMap, ffi::OsString, path::Path};

fn artifacts(path: &Path) -> BTreeMap<OsString, Vec<u8>> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name(), std::fs::read(e.path()).unwrap())
        })
        .collect()
}

#[cfg(unix)]
#[test]
fn unknown_namespace_artifact_is_named_for_initialized_and_uninitialized_stores() {
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStringExt;
    for initialized in [false, true] {
        for (name, diagnostic) in [
            (OsString::from("foreign.data"), "foreign.data"),
            (OsString::from("foreign\nentry"), "foreign\\nentry"),
            // The native macOS filesystem rejects this name at creation (EILSEQ).
            // Native Linux exercises the non-UTF-8 namespace rejection branch.
            #[cfg(target_os = "linux")]
            (
                OsString::from_vec(b"foreign\xff".to_vec()),
                "foreign\\u{fffd}",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            if initialized {
                drop(Database::create(dir.path()).unwrap());
            }
            std::fs::write(dir.path().join(&name), b"untrusted contents").unwrap();
            let before = artifacts(dir.path());
            let verified = Database::verify(dir.path()).unwrap_err();
            let opened = Database::open(dir.path()).err().unwrap();
            for error in [&verified, &opened] {
                assert_eq!(
                    (error.phase, error.kind),
                    (StoragePhase::Select, StorageErrorKind::UnsupportedFormat)
                );
                assert_eq!(
                    error.artifact.as_deref(),
                    Some(diagnostic),
                    "initialized={initialized}"
                );
                assert!(std::error::Error::source(error).is_some());
                let rendered = format!("{error} {error:?}");
                assert!(!rendered.contains(&dir.path().display().to_string()));
                assert!(!rendered.contains("untrusted contents"));
            }
            assert_eq!(artifacts(dir.path()), before);
        }
    }
}

#[test]
fn online_full_verification_matches_stopped_open_and_preserves_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::create(dir.path()).unwrap();
    let path = ObjectPath::regular("selene", "memory", "data").unwrap();
    db.catalog()
        .create_schema(
            &SchemaPath::regular("selene", "memory").unwrap(),
            CreatePolicy::Strict,
        )
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let s = db.session(&path).unwrap();
    s.execute("INSERT (:Doc {n: 1, text: 'one'}), (:Doc {n: 2, text: 'two'})")
        .unwrap();
    s.execute("CALL selene.create_text_index('Doc', 'text')")
        .unwrap();
    db.checkpoint().unwrap();
    s.execute("INSERT (:Doc {n: 3, text: 'three'})").unwrap();
    let before = artifacts(dir.path());
    let report = Database::verify(dir.path()).unwrap();
    assert_eq!((report.graphs, report.nodes, report.edges), (1, 3, 0));
    assert_eq!(report.recovery.rebuilt_indexes, 1);
    assert_eq!(report.recovery.replayed_suffix_records, 1);
    assert_eq!(
        report.recovery.synchronize_elapsed,
        std::time::Duration::ZERO
    );
    assert_eq!(
        report.recovery.position,
        db.durable_status().unwrap().position
    );
    assert_eq!(
        Database::open(dir.path()).err().unwrap().kind,
        StorageErrorKind::Contention
    );
    assert_eq!(artifacts(dir.path()), before);
    drop(s);
    drop(db);
    let reopened = Database::open(dir.path()).unwrap();
    let info = reopened.recovery_info().unwrap();
    assert_eq!(info.position, report.recovery.position);
    assert_eq!(info.rebuilt_indexes, report.recovery.rebuilt_indexes);
    assert_eq!(
        info.replayed_suffix_records,
        report.recovery.replayed_suffix_records
    );
    assert_eq!(artifacts(dir.path()), before);
}

#[cfg(unix)]
#[test]
fn read_only_permissions_and_missing_coordination_do_not_create_or_repair() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    drop(Database::create(dir.path()).unwrap());
    let before = artifacts(dir.path());
    struct Permissions<'a>(&'a Path);
    impl Drop for Permissions<'_> {
        fn drop(&mut self) {
            std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o700)).unwrap();
            for entry in std::fs::read_dir(self.0).unwrap() {
                std::fs::set_permissions(
                    entry.unwrap().path(),
                    std::fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            }
        }
    }
    let restore = Permissions(dir.path());
    for name in before.keys() {
        std::fs::set_permissions(
            dir.path().join(name),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();
    }
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    assert_eq!(Database::verify(dir.path()).unwrap().graphs, 0);
    assert_eq!(artifacts(dir.path()), before);
    drop(restore);
    for name in ["CURRENT", "LOCK", "MANIFEST.lock"] {
        let bytes = std::fs::read(dir.path().join(name)).unwrap();
        std::fs::remove_file(dir.path().join(name)).unwrap();
        let damaged = artifacts(dir.path());
        let verified = Database::verify(dir.path()).unwrap_err();
        let opened = Database::open(dir.path()).err().unwrap();
        assert_eq!(verified.kind, opened.kind, "{name}");
        assert_eq!(verified.phase, opened.phase);
        assert_eq!(artifacts(dir.path()), damaged);
        std::fs::write(dir.path().join(name), bytes).unwrap();
    }
    let absent = dir.path().join("absent");
    assert_eq!(
        Database::verify(&absent).unwrap_err().phase,
        StoragePhase::Anchor
    );
    assert!(!absent.exists());
}

#[cfg(unix)]
#[test]
fn symlink_selector_fails_before_writer_access_and_does_not_touch_target() {
    let root = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir_in(root.path()).unwrap();
    drop(Database::create(dir.path()).unwrap());
    let target = root.path().join("unrelated");
    std::fs::write(&target, b"not store authority").unwrap();
    let current = dir.path().join("CURRENT");
    std::fs::remove_file(&current).unwrap();
    std::os::unix::fs::symlink(&target, &current).unwrap();
    let before = artifacts(dir.path());
    let error = Database::verify(dir.path()).unwrap_err();
    assert_eq!(
        (error.phase, error.kind),
        (StoragePhase::Select, StorageErrorKind::InvalidArtifact)
    );
    assert_eq!(error.artifact.as_deref(), Some("CURRENT"));
    assert_eq!(Database::open(dir.path()).err().unwrap().kind, error.kind);
    assert_eq!(std::fs::read_link(&current).unwrap(), target);
    assert_eq!(std::fs::read(&target).unwrap(), b"not store authority");
    assert_eq!(artifacts(dir.path()), before);
}
