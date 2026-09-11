use super::*;
use std::{collections::BTreeMap, io::Read, path::Path};

pub(super) fn artifacts(dir: &StoreDirectory) -> BTreeMap<String, Vec<u8>> {
    dir.entries()
        .unwrap()
        .into_iter()
        .map(|name| {
            let mut bytes = Vec::new();
            dir.open_read(&name)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            (name.to_string_lossy().into_owned(), bytes)
        })
        .collect()
}

#[test]
fn reopened_writer_preserves_acknowledged_prefix_and_excludes_proved_rollback() {
    let (_temp, dir, mut wal) = fixture();
    wal.checkpoint(b"initial", 0).unwrap();
    commit(&mut wal, &[b"acknowledged"]);
    let group = wal.prepare(&[b"canceled"], Compression::Raw, 1024).unwrap();
    wal.fault = Some(Fault::PartialAppend);
    let failure = wal
        .commit(group, || false, |_| panic!("must not publish"))
        .unwrap_err();
    assert_eq!(failure.durability, Durability::Canceled);
    drop(wal);
    let before = artifacts(&dir);
    let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reopen.snapshot_body().unwrap(), b"initial");
    assert_eq!(reopen.next_body().unwrap().unwrap(), b"acknowledged");
    assert!(reopen.next_body().unwrap().is_none());
    let mut wal = reopen.finish().unwrap();
    assert_eq!(before, artifacts(&dir));
    commit(&mut wal, &[b"second acknowledgment"]);
    assert_eq!(wal.progress().acknowledged.unwrap().sequence, 2);
}

#[test]
fn checkpoint_fault_matrix_selects_only_old_or_complete_new_snapshot_and_fences() {
    for point in [
        "snapshot.create",
        "snapshot.write",
        "snapshot.partial_write",
        "snapshot.file_sync",
        "snapshot.publish",
        "snapshot.dir_sync",
        "rotation.seal",
        "rotation.create",
        "rotation.file_sync",
        "rotation.dir_sync",
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
        "rotation.handle_swap",
    ] {
        let (_temp, dir, mut wal) = fixture();
        wal.checkpoint(b"initial", 0).unwrap();
        commit(&mut wal, &[b"acknowledged"]);
        dir.fail_at(point);
        let error = wal.checkpoint(b"complete image", 1).unwrap_err();
        assert!(wal.is_fenced(), "{point}");
        assert!(wal.prepare(&[b"later"], Compression::Raw, 1024).is_err());
        let selected_new = matches!(point, "current.dir_sync" | "rotation.handle_swap");
        if selected_new {
            assert!(matches!(
                error,
                StreamError::Persist(crate::PersistError::Control(
                    crate::ControlError::PublicationUncertain { .. }
                ))
            ));
        }
        drop(wal);
        let before = artifacts(&dir);
        let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
        assert_eq!(
            reopen.snapshot_body().unwrap(),
            if selected_new {
                b"complete image".as_slice()
            } else {
                b"initial".as_slice()
            },
            "{point}"
        );
        let mut suffix = Vec::new();
        while let Some(body) = reopen.next_body().unwrap() {
            suffix.push(body);
        }
        assert_eq!(suffix.len(), usize::from(!selected_new));
        let mut wal = reopen.finish().unwrap();
        assert_eq!(artifacts(&dir), before, "open mutated bytes at {point}");
        commit(&mut wal, &[b"post reopen"]);
        assert_eq!(wal.progress().acknowledged.unwrap().sequence, 2);
    }
}

#[test]
fn rotating_checkpoints_reopen_without_ancestor_reads_and_resume_complete_tail() {
    let (_temp, dir, mut wal) = fixture();
    wal.checkpoint(b"initial", 0).unwrap();
    commit(&mut wal, &[b"one"]);
    let first = wal.checkpoint(b"one image", 1).unwrap();
    commit(&mut wal, &[b"two"]);
    let second = wal.checkpoint(b"two image", 2).unwrap();
    assert!(second.generation > first.generation);
    // Test-only offline removal proves ancestors are not implicit selection inputs.
    for name in dir.entries().unwrap() {
        let text = name.to_string_lossy();
        if text.starts_with("MANIFEST-")
            && text != format!("MANIFEST-{:020}.control", second.generation)
        {
            dir.remove(Path::new(&name)).unwrap();
        }
    }
    let group = wal
        .prepare(&[b"synchronized-unacknowledged"], Compression::Raw, 1024)
        .unwrap();
    let failure = wal
        .commit(
            group,
            || false,
            |_| Err(StreamError::Protocol("publication interrupted")),
        )
        .unwrap_err();
    assert_eq!(failure.durability, Durability::Committed);
    assert!(wal.checkpoint(b"must not select", 3).is_err());
    drop(wal);
    let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reopen.snapshot_body().unwrap(), b"two image");
    assert_eq!(
        reopen.next_body().unwrap().unwrap(),
        b"synchronized-unacknowledged"
    );
    assert!(reopen.next_body().unwrap().is_none());
    assert_eq!((reopen.prefix_records(), reopen.suffix_records()), (0, 1));
    let mut wal = reopen.finish().unwrap();
    assert_eq!(wal.progress().acknowledged, None); // never invent a prior acknowledgment
    commit(&mut wal, &[b"four"]);
    assert_eq!(wal.progress().synchronized.sequence, 4);
}

#[test]
fn damaged_authoritative_bytes_are_unchanged_and_reader_cannot_salvage_after_failure() {
    for damage in [
        "snapshot",
        "missing",
        "wal-tail",
        "wal-checksum",
        "foreign",
        "orphan",
    ] {
        let (_temp, dir, mut wal) = fixture();
        let checkpoint = wal.checkpoint(b"image", 0).unwrap();
        commit(&mut wal, &[b"one"]);
        drop(wal);
        if damage == "missing" {
            dir.remove(Path::new(&checkpoint.name)).unwrap();
        } else if damage == "snapshot" {
            let mut file = dir.open_write(Path::new(&checkpoint.name)).unwrap();
            file.seek(SeekFrom::Start(170)).unwrap();
            file.write_all(b"!").unwrap();
        } else if damage == "orphan" {
            dir.create_new(Path::new("SNAPSHOT-00000000000000009999.logical"))
                .unwrap()
                .write_all(b"newest is not selected")
                .unwrap();
        } else {
            let mut file = dir
                .open_write(Path::new(crate::control::logical::LOG_NAME))
                .unwrap();
            match damage {
                "wal-tail" => {
                    file.seek(SeekFrom::End(0)).unwrap();
                    file.write_all(b"SLTX").unwrap();
                }
                "foreign" => {
                    file.seek(SeekFrom::Start(40)).unwrap();
                    file.write_all(&[7; 16]).unwrap();
                }
                _ => {
                    file.seek(SeekFrom::Start(165)).unwrap();
                    file.write_all(b"!").unwrap();
                }
            }
        }
        let before = artifacts(&dir);
        let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
        if matches!(damage, "missing" | "snapshot") {
            assert!(reopen.snapshot_body().is_err());
        } else if damage == "orphan" {
            assert_eq!(reopen.snapshot_body().unwrap(), b"image");
        } else {
            let mut failed = false;
            loop {
                match reopen.next_body() {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            assert!(failed, "{damage}");
            assert!(reopen.next_body().is_err());
        }
        drop(reopen);
        assert_eq!(before, artifacts(&dir), "{damage}");
        // Failed open/reader drop releases writer ownership.
        drop(StoreWriter::acquire_existing(&dir).unwrap());
    }
}
