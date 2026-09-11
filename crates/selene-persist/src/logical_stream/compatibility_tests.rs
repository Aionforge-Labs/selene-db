use super::*;
use crate::logical_snapshot::SnapshotContext;

fn pr05_two_record_prefix(
    adjust: impl FnOnce(&mut Position),
) -> (
    selene_testing::PersistenceTestPath,
    StoreDirectory,
    Position,
    Position,
) {
    let (_temp, dir, mut wal) = fixture();
    wal.checkpoint(b"initial", 0).unwrap();
    commit(&mut wal, &[b"one"]);
    let first = wal.progress.synchronized;
    commit(&mut wal, &[b"two"]);
    let covered = wal.progress.synchronized;
    let mut boundary = covered;
    adjust(&mut boundary);
    let epoch = ManifestEpochGuard::acquire(&wal.authority).unwrap();
    wal.selected = crate::control::logical::publish_checkpoint(
        &epoch,
        &wal.selected,
        b"two image",
        SnapshotContext {
            boundary,
            publication: 2,
        },
    )
    .unwrap();
    drop(epoch);
    drop(wal);
    (_temp, dir, first, covered)
}

#[rstest::rstest]
#[case::empty_wal(0)]
#[case::complete_record_end(1)]
#[case::incomplete_frame(2)]
fn snapshot_covered_pr05_prefix_is_required_not_an_unsealed_tail(#[case] cut_kind: u8) {
    let (_temp, dir, first, covered) = pr05_two_record_prefix(|_| {});
    let cut = match cut_kind {
        0 => 0,
        1 => first.offset,
        _ => first.offset + 10,
    };
    assert!(cut < covered.offset);
    dir.open_write(std::path::Path::new(crate::control::logical::LOG_NAME))
        .unwrap()
        .set_len(cut)
        .unwrap();
    let before = checkpoint::artifacts(&dir);
    let mut reader = RecoveryReader::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reader.snapshot_body().unwrap(), b"two image");
    let expected_offset = if cut_kind == 0 { 0 } else { first.offset };
    let expected_sequence = if cut_kind == 0 { 1 } else { 2 };
    assert!(
        matches!(reader.next_body(), Err(StreamError::Artifact { name, offset, expected_sequence: sequence, source })
        if name == crate::control::logical::LOG_NAME && offset == Some(expected_offset)
            && sequence == Some(expected_sequence)
            && matches!(*source, StreamError::Frame(logical_frame::FrameError::CorruptIncomplete)))
    );
    assert!(matches!(reader.next_body(), Err(StreamError::Terminated)));
    assert!(reader.finish().is_err());
    assert_eq!(checkpoint::artifacts(&dir), before);
}

#[rstest::rstest]
#[case::wrong_offset(true)]
#[case::wrong_digest(false)]
fn present_pr05_checkpoint_sequence_with_inconsistent_boundary_remains_lineage(
    #[case] offset: bool,
) {
    let (_temp, dir, _, covered) = pr05_two_record_prefix(|boundary| {
        if offset {
            boundary.offset += 1;
        } else {
            boundary.digest[0] ^= 1;
        }
    });
    let before = checkpoint::artifacts(&dir);
    let mut reader = RecoveryReader::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reader.snapshot_body().unwrap(), b"two image");
    assert!(
        matches!(reader.next_body(), Err(StreamError::Artifact { name, offset, expected_sequence, source })
        if name == crate::control::logical::LOG_NAME && offset == Some(covered.offset)
            && expected_sequence == Some(covered.sequence)
            && matches!(*source, StreamError::Frame(logical_frame::FrameError::SnapshotBoundary)))
    );
    assert!(matches!(reader.next_body(), Err(StreamError::Terminated)));
    assert!(reader.finish().is_err());
    assert_eq!(checkpoint::artifacts(&dir), before);
}

#[test]
fn empty_rotated_declared_base_is_complete_at_nonzero_sequence() {
    let (_temp, dir, mut wal) = fixture();
    wal.checkpoint(b"initial", 0).unwrap();
    commit(&mut wal, &[b"one"]);
    wal.checkpoint(b"one image", 1).unwrap();
    let base = wal.progress.synchronized;
    assert_eq!((base.sequence, base.offset), (1, 0));
    drop(wal);
    let before = checkpoint::artifacts(&dir);
    let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reopen.snapshot_body().unwrap(), b"one image");
    assert!(reopen.next_body().unwrap().is_none());
    assert_eq!(reopen.position(), base);
    assert_eq!(reopen.finish().unwrap().progress().synchronized, base);
    assert_eq!(checkpoint::artifacts(&dir), before);
}

#[test]
fn storage_full_during_snapshot_write_preserves_acknowledged_prefix() {
    let (_temp, dir, mut wal) = fixture();
    wal.checkpoint(b"initial", 0).unwrap();
    commit(&mut wal, &[b"one"]);
    dir.fail_with("snapshot.partial_write", std::io::ErrorKind::StorageFull);
    assert!(wal.checkpoint(b"one image", 1).is_err());
    assert!(wal.is_fenced());
    drop(wal);
    let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reopen.snapshot_body().unwrap(), b"initial");
    assert_eq!(reopen.next_body().unwrap().unwrap(), b"one");
    assert!(reopen.next_body().unwrap().is_none());
}

#[test]
fn complete_unselected_rotation_is_not_history_and_explicit_prune_allows_new_checkpoint() {
    let (_temp, dir, mut wal) = fixture();
    wal.checkpoint(b"initial", 0).unwrap();
    commit(&mut wal, &[b"one"]);
    dir.fail_at("current.replace");
    assert!(wal.checkpoint(b"unselected", 1).is_err());
    drop(wal);
    let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reopen.snapshot_body().unwrap(), b"initial");
    assert_eq!(reopen.next_body().unwrap().unwrap(), b"one");
    assert!(reopen.next_body().unwrap().is_none());
    let mut wal = reopen.finish().unwrap();
    assert!(
        wal.prune()
            .unwrap()
            .removed
            .iter()
            .any(|a| a.name == "SNAPSHOT-00000000000000000004.logical")
    );
    wal.checkpoint(b"selected explicitly", 1).unwrap();
}

#[test]
fn pr05_single_segment_fixture_is_unchanged_on_open_and_only_explicit_checkpoint_upgrades() {
    let (_temp, dir, mut wal) = fixture();
    wal.checkpoint(b"initial", 0).unwrap();
    commit(&mut wal, &[b"one"]);
    // Exercise the preserved PR05 writer and exact SLDM layout, not a rotating
    // file relabeled as legacy. The read path must keep its full-prefix contract.
    let epoch = ManifestEpochGuard::acquire(&wal.authority).unwrap();
    wal.selected = crate::control::logical::publish_checkpoint(
        &epoch,
        &wal.selected,
        b"one image",
        SnapshotContext {
            boundary: wal.progress.synchronized,
            publication: 1,
        },
    )
    .unwrap();
    drop(epoch);
    commit(&mut wal, &[b"two"]);
    let original_segment = wal.progress.synchronized.segment;
    drop(wal);
    let before = checkpoint::artifacts(&dir);
    let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reopen.snapshot_body().unwrap(), b"one image");
    assert_eq!(reopen.next_body().unwrap().unwrap(), b"two");
    assert!(reopen.next_body().unwrap().is_none());
    assert_eq!((reopen.prefix_records(), reopen.suffix_records()), (1, 1));
    let mut wal = reopen.finish().unwrap();
    assert_eq!(checkpoint::artifacts(&dir), before);
    assert_eq!(wal.progress.synchronized.segment, original_segment);
    wal.checkpoint(b"two image", 2).unwrap();
    assert_ne!(wal.progress.synchronized.segment, original_segment);
    wal.checkpoint(b"two image again", 2).unwrap();
    let report = wal.prune().unwrap();
    assert!(report.cleanup_error.is_none());
    assert!(!dir.contains(crate::control::logical::LOG_NAME).unwrap());
}

#[test]
fn staged_orphan_bytes_are_debt_never_authority_and_unknown_control_stops_prune() {
    let (_temp, dir, mut wal) = fixture();
    wal.checkpoint(b"initial", 0).unwrap();
    wal.checkpoint(b"second", 0).unwrap();
    wal.checkpoint(b"third", 0).unwrap();
    let name = "SNAPSHOT-00000000000000009999.logical";
    dir.create_new(std::path::Path::new(name))
        .unwrap()
        .write_all(b"not a checkpoint")
        .unwrap();
    let report = wal.prune().unwrap();
    assert!(
        report
            .retained
            .iter()
            .any(|a| a.artifact.name == name && a.reason == RetentionReason::Deferred)
    );
    let future = "MANIFEST-00000000000000009999.control";
    dir.create_new(std::path::Path::new(future))
        .unwrap()
        .write_all(b"SLRM future unsupported")
        .unwrap();
    let before = checkpoint::artifacts(&dir);
    assert!(wal.prune().is_err());
    assert_eq!(checkpoint::artifacts(&dir), before);
    drop(wal);
    let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reopen.snapshot_body().unwrap(), b"third");
    assert!(reopen.next_body().unwrap().is_none());
}
