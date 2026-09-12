#![cfg(any(target_os = "linux", target_os = "macos"))]

use selene_testing::PersistenceTestPath;
use std::io::Write;
use std::os::unix::fs::symlink;

use super::*;

#[test]
fn names_are_rejected_before_any_child_side_effect() {
    let fixture = PersistenceTestPath::new();
    let dir = StoreDirectory::open(fixture.parent().unwrap()).unwrap();
    for name in [
        "", ".", "..", "../wal", "x/y", "a\\b", "C:wal", "/outside", "wal/",
    ] {
        let path = Path::new(name);
        assert!(matches!(
            dir.create_new(path),
            Err(PersistError::Directory(DirectoryError::InvalidName(_)))
        ));
        assert!(dir.rename(path, Path::new("valid")).is_err());
        assert!(dir.publish_new(Path::new("valid"), path).is_err());
        assert!(dir.remove(path).is_err());
    }
    assert!(dir.entries().unwrap().is_empty());
}

#[test]
fn symlinks_hardlinks_and_nonregular_entries_cannot_be_mutably_opened() {
    let fixture = PersistenceTestPath::new();
    let dir = StoreDirectory::open(fixture.parent().unwrap()).unwrap();
    std::fs::write(dir.locator().join("target"), b"untouched").unwrap();
    symlink("target", dir.locator().join("link")).unwrap();
    assert!(matches!(
        dir.open_write(Path::new("link")),
        Err(PersistError::Directory(DirectoryError::NotRegular(_)))
    ));
    std::fs::create_dir(dir.locator().join("directory")).unwrap();
    assert!(matches!(
        dir.open_write(Path::new("directory")),
        Err(PersistError::Directory(DirectoryError::NotRegular(_)))
    ));
    std::fs::hard_link(dir.locator().join("target"), dir.locator().join("hardlink")).unwrap();
    assert!(matches!(
        dir.open_write(Path::new("hardlink")),
        Err(PersistError::Directory(DirectoryError::NotRegular(_)))
    ));
    assert_eq!(
        std::fs::read(dir.locator().join("target")).unwrap(),
        b"untouched"
    );
}

#[test]
fn final_symlink_swap_after_preflight_is_not_followed() {
    let fixture = PersistenceTestPath::new();
    let dir = StoreDirectory::open(fixture.parent().unwrap()).unwrap();
    std::fs::write(dir.locator().join("child"), b"old").unwrap();
    std::fs::write(dir.locator().join("target"), b"untouched").unwrap();
    let root = dir.locator().to_path_buf();
    dir.before_open(move || {
        std::fs::remove_file(root.join("child")).unwrap();
        symlink("target", root.join("child")).unwrap();
    });
    assert!(dir.open_write(Path::new("child")).is_err());
    assert_eq!(
        std::fs::read(dir.locator().join("target")).unwrap(),
        b"untouched"
    );
}

#[test]
fn already_open_handle_survives_rename_before_engine_acceptance() {
    let fixture = PersistenceTestPath::new();
    let original = fixture.parent().unwrap().join("store");
    let retained = original.with_file_name("retained");
    std::fs::create_dir(&original).unwrap();
    let file = File::open(&original).unwrap();
    std::fs::rename(&original, &retained).unwrap();
    std::fs::create_dir(&original).unwrap();
    let dir = StoreDirectory::from_file(file, &original).unwrap();
    dir.create_new(Path::new("artifact"))
        .unwrap()
        .write_all(b"anchored")
        .unwrap();
    dir.sync().unwrap();
    assert_eq!(
        std::fs::read(retained.join("artifact")).unwrap(),
        b"anchored"
    );
    assert_eq!(std::fs::read_dir(original).unwrap().count(), 0);
    let file = File::open(retained.join("artifact")).unwrap();
    assert!(matches!(
        StoreDirectory::from_file(file, "not-a-dir"),
        Err(PersistError::Directory(DirectoryError::NotDirectory))
    ));
}

#[test]
fn directory_aliases_and_clones_share_one_nonreentrant_writer_domain() {
    let fixture = PersistenceTestPath::new();
    let original = fixture.parent().unwrap().join("store");
    let alias = original.with_file_name("alias");
    std::fs::create_dir(&original).unwrap();
    symlink(&original, &alias).unwrap();
    let dir = StoreDirectory::open(&original).unwrap();
    let alias_dir = StoreDirectory::open(&alias).unwrap();
    assert!(dir.same_directory(&alias_dir).unwrap());
    let owner = StoreWriter::acquire(&dir).unwrap();
    let shared_owner = owner.clone();
    assert!(matches!(
        StoreWriter::acquire(&dir.clone()),
        Err(PersistError::WriterLockHeld)
    ));
    assert!(matches!(
        StoreWriter::acquire(&alias_dir),
        Err(PersistError::WriterLockHeld)
    ));
    drop(owner);
    assert!(matches!(
        StoreWriter::acquire(&dir),
        Err(PersistError::WriterLockHeld)
    ));
    drop(shared_owner);
    drop(StoreWriter::acquire(&alias_dir).unwrap());
    assert!(dir.contains(STORE_LOCK_FILE_NAME).unwrap());
    for name in [STORE_LOCK_FILE_NAME, crate::MANIFEST_LOCK_FILE_NAME] {
        assert!(matches!(
            dir.remove(Path::new(name)),
            Err(PersistError::Directory(DirectoryError::CoordinationEntry(
                _
            )))
        ));
        assert!(dir.rename(Path::new("artifact"), Path::new(name)).is_err());
        assert!(
            dir.publish_new(Path::new("artifact"), Path::new(name))
                .is_err()
        );
    }
}

#[test]
fn writer_domain_excludes_other_processes() {
    const ENV: &str = "SELENE_CAPABILITY_WRITER_CHILD";
    if let Some(path) = std::env::var_os(ENV) {
        let dir = StoreDirectory::open(Path::new(&path)).unwrap();
        assert!(matches!(
            StoreWriter::acquire(&dir),
            Err(PersistError::WriterLockHeld)
        ));
        return;
    }
    let fixture = PersistenceTestPath::new();
    let dir = StoreDirectory::open(fixture.parent().unwrap()).unwrap();
    let owner = StoreWriter::acquire(&dir).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "store_directory::tests::writer_domain_excludes_other_processes",
            "--nocapture",
        ])
        .env(ENV, dir.locator())
        .status()
        .unwrap();
    assert!(status.success());
    drop(owner);
    drop(StoreWriter::acquire(&dir).unwrap());
}
