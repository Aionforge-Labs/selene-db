use super::*;
use crate::{DurableCommitState, catalog::FailurePoint};

#[test]
fn fenced_commit_never_checkpoints_stale_preflight_and_recovers_whole_record() {
    for point in [
        FailurePoint::BeforePublication,
        FailurePoint::AfterPublicationAcknowledgement,
        FailurePoint::AfterPublicationObserverPanic,
    ] {
        let (dir, db, path) = fixture();
        let current = std::fs::read(dir.path().join("CURRENT")).unwrap();
        let s = db.session(&path).unwrap();
        *db.inner.failure.lock() = Some(point);
        let error = s.execute("INSERT (:Item {n: 7})").unwrap_err();
        assert_eq!(
            error.durable_commit_outcome().unwrap().state,
            DurableCommitState::CommittedUnacknowledged
        );
        assert_eq!(db.checkpoint().unwrap_err().kind, StorageErrorKind::Fenced);
        assert_eq!(std::fs::read(dir.path().join("CURRENT")).unwrap(), current);
        drop(s);
        drop(db);
        let reopened = Database::open(dir.path()).unwrap();
        assert_eq!(
            reopened
                .session(&path)
                .unwrap()
                .execute("MATCH (n) WHERE n.n = 7 RETURN n")
                .unwrap()
                .row_count(),
            Some(1)
        );
        reopened.checkpoint().unwrap();
    }
}

#[test]
fn snapshot_publication_error_releases_reservation_but_fences_owner() {
    let (dir, db, path) = fixture();
    // Immutable collision is a real-file failure before any CURRENT update.
    std::fs::write(
        dir.path().join("SNAPSHOT-00000000000000000004.logical"),
        b"orphan",
    )
    .unwrap();
    assert!(db.checkpoint().is_err());
    assert!(db.durable_status().unwrap().fenced);
    assert_eq!(db.checkpoint().unwrap_err().kind, StorageErrorKind::Fenced);
    assert_eq!(
        db.session(&path)
            .unwrap()
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .row_count(),
        Some(0)
    );
    drop(db);
    let reopened = Database::open(dir.path()).unwrap();
    reopened
        .session(&path)
        .unwrap()
        .execute("INSERT (:Preserved)")
        .unwrap();
}

#[test]
fn persisted_native_registration_tampering_fails_before_returning_a_database() {
    use selene_catalog::{CatalogDescriptor, CatalogLogicalRecords, CatalogPayload, NativeBinding};
    let directory = tempfile::tempdir().unwrap();
    let memory = Database::builder().build();
    let records = memory.catalog().snapshot().logical_catalog().unwrap();
    let mut descriptors = records.descriptors().to_vec();
    let d = descriptors
        .iter_mut()
        .find(|d| matches!(d.payload(), CatalogPayload::Procedure(_)))
        .unwrap();
    let mut payload = d.payload().clone();
    let CatalogPayload::Procedure(native) = &mut payload else {
        unreachable!()
    };
    let NativeBinding::Procedure(procedure) = &mut native.binding else {
        unreachable!()
    };
    procedure.description.push_str(" tampered");
    *d = CatalogDescriptor::new(
        d.id(),
        d.kind(),
        d.name().clone(),
        d.parent(),
        d.generation(),
        d.creation().clone(),
        payload,
    )
    .unwrap();
    let records = CatalogLogicalRecords::new(
        records.reconstruct().unwrap().generation(),
        records.high_water().clone(),
        descriptors,
    )
    .unwrap();
    let bytes = encode_checkpoint(&records, &Default::default(), &[], Limits::default()).unwrap();
    let dir = StoreDirectory::open(directory.path()).unwrap();
    let mut wal = LogicalWal::create(
        EmptyStoreControl::create_empty(&dir, compatibility().unwrap()).unwrap(),
    )
    .unwrap();
    wal.checkpoint(&bytes, 0).unwrap();
    drop(wal);
    let before = std::fs::read(directory.path().join("CURRENT")).unwrap();
    let error = Database::open(directory.path()).err().unwrap();
    assert_eq!(error.phase, StoragePhase::Rebuild);
    assert_eq!(error.kind, StorageErrorKind::NativeAdmission);
    let verified = Database::verify(directory.path()).unwrap_err();
    assert_eq!(verified.kind, error.kind);
    assert_eq!(verified.phase, error.phase);
    assert_eq!(verified.artifact, error.artifact);
    assert_eq!(
        std::fs::read(directory.path().join("CURRENT")).unwrap(),
        before
    );
    drop(selene_persist::StoreWriter::acquire_existing(&dir).unwrap());
}
