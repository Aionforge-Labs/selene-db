use super::*;

#[test]
fn checkpoint_cannot_cover_up_corruption_in_its_old_selected_recovery_root() {
    for snapshot_damage in [false, true] {
        let (_temp, dir, mut wal) = fixture();
        let checkpoint = wal.checkpoint(b"initial", 0).unwrap();
        commit(&mut wal, &[b"one"]);
        let name = if snapshot_damage {
            checkpoint.name
        } else {
            wal.selected.log_name()
        };
        let mut file = dir.open_write(std::path::Path::new(&name)).unwrap();
        file.write_all(b"!").unwrap();
        file.sync_all().unwrap();
        let before = checkpoint::artifacts(&dir);
        assert!(
            wal.checkpoint(b"valid memory image", 1).is_err(),
            "corrupt root was replaced"
        );
        assert_eq!(
            checkpoint::artifacts(&dir),
            before,
            "corruption must be rejected before new artifacts"
        );
        assert!(wal.is_fenced());
    }
}

#[test]
fn maintenance_inventory_limit_precedes_control_reads_and_preserves_selected_state() {
    let (_temp, dir, mut wal) = fixture();
    wal.checkpoint(b"initial", 0).unwrap();
    for n in 10000..14100 {
        dir.create_new(std::path::Path::new(&format!("SNAPSHOT-{n:020}.logical")))
            .unwrap();
    }
    dir.reset_control_payload_opens();
    assert!(matches!(
        wal.prune(),
        Err(StreamError::Persist(crate::PersistError::Control(
            crate::ControlError::TooLarge
        )))
    ));
    assert_eq!(
        dir.control_payload_opens(),
        0,
        "inventory admission must precede payload work"
    );
    assert!(!wal.is_fenced());
}

#[test]
fn prune_keeps_latest_two_complete_roots_and_actual_leased_dependencies() {
    let (_temp, dir, mut wal) = fixture();
    let first = wal.checkpoint(b"initial image", 0).unwrap();
    commit(&mut wal, &[b"selected prefix"]);
    let first_log = wal.selected.log_name();
    let first_manifest = wal.selected.manifest_name().to_owned();
    let mut reader = LogicalReader::open(&dir, &identity(), 1024).unwrap();
    let second = wal.checkpoint(b"second image", 1).unwrap();
    let second_log = wal.selected.log_name();
    commit(&mut wal, &[b"second suffix"]);
    let third = wal.checkpoint(b"third image", 2).unwrap();
    let third_log = wal.selected.log_name();
    let held = wal.prune().unwrap();
    assert!(held.cleanup_error.is_none());
    for name in [&first.name, &first_log, &first_manifest] {
        assert!(dir.contains(name).unwrap());
        assert!(
            held.retained.iter().any(|a| &a.artifact.name == name
                && a.artifact.bytes > 0
                && a.reason == RetentionReason::Reader),
            "{name}: {held:?}"
        );
    }
    assert_eq!(reader.snapshot_body().unwrap(), b"initial image");
    assert_eq!(reader.next_body().unwrap().unwrap(), b"selected prefix");
    assert!(reader.next_body().unwrap().is_none());
    drop(reader);
    let released = wal.prune().unwrap();
    for name in [&first.name, &first_log, &first_manifest] {
        assert!(!dir.contains(name).unwrap(), "{name}: {released:?}");
        assert!(released.removed.iter().any(|a| &a.name == name));
    }
    for name in [&second.name, &second_log, &third.name, &third_log] {
        assert!(dir.contains(name).unwrap(), "required {name}");
    }
    drop(wal);
    let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
    assert_eq!(reopen.snapshot_body().unwrap(), b"third image");
    assert!(reopen.next_body().unwrap().is_none());
    assert_eq!(reopen.prefix_records(), 0);
    assert_eq!(reopen.finish().unwrap().progress().synchronized.sequence, 2);
}

#[test]
fn uncertain_current_reopen_requires_control_durability_before_any_prune_unlink() {
    for point in ["prune.establish_file_sync", "prune.establish_dir_sync"] {
        let (_temp, dir, mut wal) = fixture();
        wal.checkpoint(b"initial", 0).unwrap();
        commit(&mut wal, &[b"one"]);
        wal.checkpoint(b"one image", 1).unwrap();
        commit(&mut wal, &[b"two"]);
        dir.fail_at("current.dir_sync");
        assert!(wal.checkpoint(b"two image", 2).is_err());
        drop(wal);
        let mut reopen = ReopeningWal::open(&dir, &identity(), 1024).unwrap();
        assert_eq!(reopen.snapshot_body().unwrap(), b"two image");
        assert!(reopen.next_body().unwrap().is_none());
        let mut wal = reopen.finish().unwrap();
        let before = checkpoint::artifacts(&dir);
        dir.fail_at(point);
        assert!(wal.prune().is_err(), "{point}");
        assert_eq!(
            before,
            checkpoint::artifacts(&dir),
            "unlink before durability"
        );
        assert!(!wal.is_fenced());
        assert!(!wal.prune().unwrap().removed.is_empty());
        commit(&mut wal, &[b"three"]);
    }
}

#[test]
fn explicit_cleanup_debt_is_resumable_and_does_not_fence_commits() {
    for point in ["prune.unlink", "prune.dir_sync"] {
        let (_temp, dir, mut wal) = fixture();
        wal.checkpoint(b"initial", 0).unwrap();
        commit(&mut wal, &[b"one"]);
        wal.checkpoint(b"one image", 1).unwrap();
        wal.checkpoint(b"same image", 1).unwrap();
        dir.fail_at(point);
        let report = wal.prune().unwrap();
        assert!(report.cleanup_error.is_some());
        assert!(
            report.removed.is_empty(),
            "must not claim unsynchronized reclamation"
        );
        assert!(
            report
                .retained
                .iter()
                .any(|a| a.reason == RetentionReason::Deferred)
        );
        assert!(!wal.is_fenced());
        commit(&mut wal, &[b"two"]);
        let report = wal.prune().unwrap();
        assert!(report.cleanup_error.is_none());
        assert!(!report.removed.is_empty());
        assert!(
            !report
                .retained
                .iter()
                .any(|a| a.reason == RetentionReason::Deferred)
        );
    }
}

#[test]
fn corrupt_current_or_sealed_history_never_authorizes_deletion() {
    use std::path::Path;
    for damage in [
        "current",
        "sealed-truncate",
        "sealed-trailing",
        "sealed-checksum",
    ] {
        let (_temp, dir, mut wal) = fixture();
        wal.checkpoint(b"initial", 0).unwrap();
        commit(&mut wal, &[b"one"]);
        wal.checkpoint(b"one image", 1).unwrap();
        commit(&mut wal, &[b"two"]);
        let sealed = wal.selected.log_name();
        wal.checkpoint(b"two image", 2).unwrap();
        let name = if damage == "current" {
            "CURRENT"
        } else {
            &sealed
        };
        let mut file = dir.open_write(Path::new(name)).unwrap();
        match damage {
            "sealed-truncate" => file.set_len(file.metadata().unwrap().len() - 1).unwrap(),
            "sealed-trailing" => {
                file.seek(SeekFrom::End(0)).unwrap();
                file.write_all(b"SLTX").unwrap();
            }
            _ => {
                file.write_all(b"!").unwrap();
            }
        }
        file.sync_all().unwrap();
        let before = checkpoint::artifacts(&dir);
        assert!(wal.prune().is_err(), "{damage}");
        assert_eq!(before, checkpoint::artifacts(&dir));
    }
}
