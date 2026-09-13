//! Compact real-facade corruption matrix. Mutations repair outer integrity to reach
//! their intended owning validator; expected categories derive from physical fields.
use super::*;
use selene_persist::logical_stream::{
    ControlMutation, mutate_control_fixture, mutate_snapshot_fixture, repair_fixture_integrity,
};
use std::{collections::BTreeMap, ffi::OsString};

fn bytes(dir: &Path) -> BTreeMap<OsString, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name(), std::fs::read(e.path()).unwrap())
        })
        .collect()
}

fn check(
    dir: &Path,
    phase: StoragePhase,
    kind: StorageErrorKind,
    artifact: &str,
    offset: Option<u64>,
) {
    let before = bytes(dir);
    let verify = Database::verify(dir).unwrap_err();
    assert_eq!((verify.phase, verify.kind), (phase, kind), "{verify:?}");
    assert_eq!(verify.artifact.as_deref(), Some(artifact));
    if let Some(offset) = offset {
        assert_eq!(verify.offset, Some(offset));
    }
    assert_eq!(bytes(dir), before);
    let open = Database::open(dir).err().expect("no healthy database");
    assert_eq!((open.phase, open.kind), (verify.phase, verify.kind));
    assert_eq!(open.artifact, verify.artifact);
    assert_eq!(open.offset, verify.offset);
    assert_eq!(open.expected_sequence, verify.expected_sequence);
    assert_eq!(open.observed_sequence, verify.observed_sequence);
    assert!(std::error::Error::source(&open).is_some());
    assert!(!format!("{open:?} {open}").contains(&dir.display().to_string()));
    assert_eq!(bytes(dir), before);
}

fn fixture(rotated: bool) -> (tempfile::TempDir, VerificationReport) {
    let (dir, db, path) = super::tests::fixture();
    if rotated {
        db.checkpoint().unwrap();
    }
    db.session(&path)
        .unwrap()
        .execute("INSERT (:N {id: 1, text: 'one'})")
        .unwrap();
    db.session(&path)
        .unwrap()
        .execute("INSERT (:N {id: 2, text: 'two'})")
        .unwrap();
    let report = Database::verify(dir.path()).unwrap();
    drop(db);
    (dir, report)
}

#[test]
fn public_selected_control_missing_and_compatibility_matrix() {
    use StorageErrorKind as K;
    for rotated in [false, true] {
        for which in 0..3 {
            let (dir, report) = fixture(rotated);
            let (name, phase) = match which {
                0 => (&report.manifest, StoragePhase::Select),
                1 => (&report.snapshot, StoragePhase::Snapshot),
                _ => (&report.wal, StoragePhase::Select),
            };
            std::fs::remove_file(dir.path().join(name)).unwrap();
            check(dir.path(), phase, K::MissingArtifact, name, None);
        }
        for (mutation, kind) in [
            (ControlMutation::Store, K::Lineage),
            (ControlMutation::Epoch, K::Lineage),
            (ControlMutation::Generation, K::Lineage),
            (ControlMutation::Version, K::UnsupportedFormat),
            (ControlMutation::Profile, K::Compatibility),
            (ControlMutation::Unicode, K::Compatibility),
            (ControlMutation::Collation, K::Compatibility),
        ] {
            let (dir, report) = fixture(rotated);
            mutate_control_fixture(
                &StoreDirectory::open(dir.path()).unwrap(),
                &compatibility().unwrap(),
                mutation,
            )
            .unwrap();
            check(
                dir.path(),
                StoragePhase::Select,
                kind,
                &report.manifest,
                None,
            );
        }
        for name_index in 0..2 {
            let (dir, report) = fixture(rotated);
            let name = if name_index == 0 {
                "CURRENT"
            } else {
                &report.manifest
            };
            let mut content = std::fs::read(dir.path().join(name)).unwrap();
            let last = content.len() - 1;
            content[last] ^= 1;
            std::fs::write(dir.path().join(name), content).unwrap();
            check(dir.path(), StoragePhase::Select, K::Integrity, name, None);
        }
    }
}

#[test]
fn public_snapshot_physical_and_rehashed_semantic_matrix() {
    use StorageErrorKind as K;
    for rotated in [false, true] {
        for (offset, kind) in [
            (8, K::UnsupportedFormat),
            (32, K::ForeignStore),
            (48, K::ForeignEpoch),
            (56, K::ForeignSegment),
            (104, K::DigestLineage),
            (88, K::Lineage),
            (168, K::UnsupportedFormat),
            (172, K::Corruption),
        ] {
            let (dir, report) = fixture(rotated);
            mutate_snapshot_fixture(
                &StoreDirectory::open(dir.path()).unwrap(),
                &compatibility().unwrap(),
                |b| {
                    b[offset] ^= 4;
                    repair_fixture_integrity(b, 168);
                },
            )
            .unwrap();
            check(
                dir.path(),
                StoragePhase::Snapshot,
                kind,
                &report.snapshot,
                Some(if offset >= 168 { 168 } else { 0 }),
            );
        }
        for (cut, kind) in [(1, K::IncompleteRequired), (100, K::IncompleteRequired)] {
            let (dir, report) = fixture(rotated);
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(dir.path().join(&report.snapshot))
                .unwrap();
            file.set_len(cut).unwrap();
            check(
                dir.path(),
                StoragePhase::Snapshot,
                kind,
                &report.snapshot,
                Some(0),
            );
        }
        let (dir, report) = fixture(rotated);
        mutate_snapshot_fixture(
            &StoreDirectory::open(dir.path()).unwrap(),
            &compatibility().unwrap(),
            |b| {
                // v1 mandatory catalog: generation, nine watermarks, then u32 count.
                let count = 168 + 4 + 1 + 8 + 9 * 8;
                b[count..count + 4].copy_from_slice(&4097u32.to_le_bytes());
                repair_fixture_integrity(b, 168);
            },
        )
        .unwrap();
        check(
            dir.path(),
            StoragePhase::Snapshot,
            K::ResourceLimit,
            &report.snapshot,
            Some(168),
        );
    }
}

#[test]
fn public_wal_integrity_lineage_sequence_and_tail_matrix() {
    use StorageErrorKind as K;
    for rotated in [false, true] {
        for (offset, value, kind) in [
            (8, 3, K::UnsupportedFormat),
            (40, 99, K::ForeignStore),
            (56, 9, K::ForeignEpoch),
            (64, 99, K::ForeignSegment),
            (96, 99, K::DigestLineage),
            (32, 127, K::SequenceGap),
            (32, 0, K::SequenceOverlap),
            (160, 99, K::UnsupportedFormat),
        ] {
            let (dir, report) = fixture(rotated);
            let mut content = std::fs::read(dir.path().join(&report.wal)).unwrap();
            let first_len = u64::from_le_bytes(content[16..24].try_into().unwrap()) as usize + 200;
            // Store/segment identities and the predecessor digest are random.
            // Assigning 99 can leave the valid fixture unchanged (1/256); a
            // bit flip guarantees the intended lineage mismatch instead.
            if matches!(offset, 40 | 64 | 96) {
                content[offset] ^= 1;
            } else {
                content[offset] = value;
            }
            repair_fixture_integrity(&mut content[..first_len], 160);
            std::fs::write(dir.path().join(&report.wal), content).unwrap();
            check(dir.path(), StoragePhase::Replay, kind, &report.wal, Some(0));
        }
        for cut in [1, 100, 170] {
            let (dir, report) = fixture(rotated);
            let content = std::fs::read(dir.path().join(&report.wal)).unwrap();
            std::fs::write(dir.path().join(&report.wal), &content[..cut]).unwrap();
            check(
                dir.path(),
                StoragePhase::Replay,
                K::IncompleteTail,
                &report.wal,
                Some(0),
            );
        }
        for which in [128, 160] {
            let (dir, report) = fixture(rotated);
            let mut content = std::fs::read(dir.path().join(&report.wal)).unwrap();
            content[which] ^= 1;
            std::fs::write(dir.path().join(&report.wal), content).unwrap();
            check(
                dir.path(),
                StoragePhase::Replay,
                K::Integrity,
                &report.wal,
                Some(0),
            );
        }
    }
}
