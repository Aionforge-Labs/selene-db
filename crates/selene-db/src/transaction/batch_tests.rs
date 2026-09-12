//! Multi-batch GQL commits cross the same F02 authority cut-lines.

use super::*;

#[test]
fn batch_control_commit_phase_matrix_preserves_live_and_public_reopen_outcomes() {
    for point in [
        FailurePoint::BeforeAuthorityPrepare,
        FailurePoint::BeforeAuthorityFlush,
        FailurePoint::BeforePublication,
        FailurePoint::AfterPublicationAcknowledgement,
        FailurePoint::AfterPublicationObserverPanic,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::create(directory.path()).unwrap();
        let graph = ObjectPath::regular("selene", "batch", "data").unwrap();
        db.catalog()
            .create_schema(
                &SchemaPath::regular("selene", "batch").unwrap(),
                CreatePolicy::Strict,
            )
            .unwrap();
        db.catalog()
            .create_graph(&graph, None, CreatePolicy::Strict)
            .unwrap();
        let session = db.session(&graph).unwrap();
        session.execute("INSERT (:Seed {k: 0})").unwrap();
        for shift in 0..10 {
            session
                .execute(&format!(
                    "MATCH (s:Seed) INSERT (:Seed {{k: s.k + {}}})",
                    1 << shift,
                ))
                .unwrap();
        }
        session.execute("INSERT (:Seed {k: 1024})").unwrap();
        session.execute("START TRANSACTION").unwrap();
        let staged = session
            .execute("MATCH (s:Seed) INSERT (:Item {id: s.k})")
            .unwrap();
        assert_eq!(staged.write_summary().unwrap().change_count(), 1025);
        let before = db.inner.state.load_full();
        *db.inner.failure.lock() = Some(point);
        let error = session.execute("COMMIT").unwrap_err();
        let synchronized = matches!(
            point,
            FailurePoint::BeforePublication
                | FailurePoint::AfterPublicationAcknowledgement
                | FailurePoint::AfterPublicationObserverPanic
        );
        let published = matches!(
            point,
            FailurePoint::AfterPublicationAcknowledgement
                | FailurePoint::AfterPublicationObserverPanic
        );
        let outcome = error.durable_commit_outcome().unwrap();
        assert_eq!(outcome.published, published);
        assert_eq!(
            outcome.state,
            if synchronized {
                DurableCommitState::CommittedUnacknowledged
            } else {
                DurableCommitState::Canceled
            }
        );
        assert_eq!(
            error.gqlstatus().unwrap().as_str(),
            if synchronized { "40003" } else { "40N01" }
        );
        assert_eq!(
            session.context().transaction().unwrap().state(),
            if synchronized {
                TransactionState::Indeterminate
            } else {
                TransactionState::RolledBack
            }
        );
        assert_eq!(
            session.context().request_slot(),
            crate::RequestSlotState::Vacant
        );
        assert_eq!(
            Arc::ptr_eq(&before, &db.inner.state.load_full()),
            !published
        );
        assert_eq!(
            session
                .execute("MATCH (n:Item) RETURN n")
                .unwrap()
                .row_count(),
            Some(if published { 1025 } else { 0 })
        );
        // No retry after an unknown outcome: drop all owners and reconcile by open.
        drop(before);
        drop(session);
        drop(db);
        let db = Database::open(directory.path()).unwrap();
        assert_eq!(
            db.session(&graph)
                .unwrap()
                .execute("MATCH (n:Item) RETURN n")
                .unwrap()
                .row_count(),
            Some(if synchronized { 1025 } else { 0 })
        );
    }
}
