use super::*;
use crate::{
    CreatePolicy, Database, DurableCommitPhase, DurableCommitState, ErrorKind, ObjectPath,
    SchemaPath, TransactionAccessMode, TransactionState, catalog::FailurePoint,
};
use selene_core::{GraphId, logical::Limits};
use selene_graph::logical_transaction::ReplayState;
use selene_persist::{
    StoreDirectory,
    control::{CompatibilityIdentity, EmptyStoreControl},
    logical_stream::LogicalReader,
};

#[path = "named_tests.rs"]
mod named_tests;
#[path = "public_recovery_tests.rs"]
mod public_recovery_tests;

fn identity() -> CompatibilityIdentity {
    CompatibilityIdentity::new("facade-commit-fixture", 1, [5; 32], [17, 0, 0], "binary", 1)
        .unwrap()
}

fn fixture() -> (
    tempfile::TempDir,
    StoreDirectory,
    Database,
    ReplayState,
    ObjectPath,
) {
    let temp = tempfile::tempdir().unwrap();
    let dir = StoreDirectory::open(temp.path()).unwrap();
    let control = EmptyStoreControl::create_empty(&dir, identity()).unwrap();
    let wal = LogicalWal::create(control).unwrap();
    let database = Database {
        inner: Arc::new(DatabaseInner::with_wal(crate::DatabaseConfig::default(), wal).unwrap()),
    };
    let seed = ReplayState::seed(database.catalog().snapshot().logical_catalog().unwrap()).unwrap();
    let schema = SchemaPath::regular("selene", "durable").unwrap();
    let graph = ObjectPath::regular("selene", "durable", "data").unwrap();
    database
        .catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    database
        .catalog()
        .create_graph(&graph, None, CreatePolicy::Strict)
        .unwrap();
    (temp, dir, database, seed, graph)
}

fn replay(dir: &StoreDirectory, mut seed: ReplayState) -> (ReplayState, usize) {
    let mut reader = LogicalReader::open(dir, &identity(), Limits::default().bytes).unwrap();
    let mut count = 0;
    while let Some(body) = reader.next_body().unwrap() {
        seed = seed.apply_body(&body, Limits::default()).unwrap();
        count += 1;
    }
    assert!(!reader.incomplete_tail());
    (seed, count)
}

#[test]
fn durable_implicit_and_explicit_requests_use_one_outer_authority() {
    let (_temp, dir, database, seed, graph) = fixture();
    let session = database.session(&graph).unwrap();
    session.execute("INSERT (:Item {n: 1})").unwrap();
    let before = database.inner.state.load_full();
    session
        .start_transaction(TransactionAccessMode::ReadWrite)
        .unwrap();
    session.execute("INSERT (:Item {n: 2})").unwrap();
    session.execute("INSERT (:Item {n: 3})").unwrap();
    assert!(Arc::ptr_eq(&before, &database.inner.state.load_full()));
    session.commit_transaction().unwrap();
    assert_eq!(
        database.inner.state.load().publication,
        before.publication + 1
    );
    assert_eq!(
        session.execute("MATCH (n) RETURN n").unwrap().row_count(),
        Some(3)
    );
    let progress = database
        .inner
        .transactions
        .durable
        .lock()
        .as_ref()
        .unwrap()
        .progress();
    assert_eq!(progress.written, progress.synchronized);
    assert_eq!(progress.acknowledged, Some(progress.synchronized));
    assert_eq!(progress.published, progress.acknowledged);
    drop(session);
    drop(database);
    let (state, count) = replay(&dir, seed);
    assert_eq!(count, 4); // schema, graph, implicit, entire explicit transaction
    assert_eq!(state.graph_summary(GraphId::new(1)).unwrap().0, 3);
}

#[test]
fn durable_phase_matrix_pins_status_live_replay_and_fencing() {
    for point in [
        FailurePoint::BeforeAuthorityPrepare,
        FailurePoint::BeforeAuthorityFlush,
        FailurePoint::BeforePublication,
        FailurePoint::AfterPublicationAcknowledgement,
        FailurePoint::AfterPublicationObserverPanic,
    ] {
        let (_temp, dir, database, seed, graph) = fixture();
        let session = database.session(&graph).unwrap();
        session
            .start_transaction(TransactionAccessMode::ReadWrite)
            .unwrap();
        session.execute("INSERT (:Item {n: 1})").unwrap();
        session.execute("INSERT (:Item {n: 2})").unwrap();
        let before = database.inner.state.load_full();
        *database.inner.failure.lock() = Some(point);
        let error = session.commit_transaction().unwrap_err();
        let outcome = error.durable_commit_outcome().unwrap();
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
            error.kind(),
            if synchronized {
                ErrorKind::DurableCommitUnacknowledged
            } else {
                ErrorKind::DurableCommitCanceled
            }
        );
        assert_eq!(
            error.gqlstatus().unwrap().as_str(),
            if synchronized { "40003" } else { "40N01" }
        );
        assert_eq!(
            outcome.phase,
            if published {
                DurableCommitPhase::Acknowledge
            } else if synchronized {
                DurableCommitPhase::Publish
            } else {
                DurableCommitPhase::Prepare
            }
        );
        assert_eq!(
            Arc::ptr_eq(&before, &database.inner.state.load_full()),
            !published
        );
        assert!(std::error::Error::source(&error).is_some());
        assert_eq!(
            session.context().transaction().unwrap().state(),
            if synchronized {
                TransactionState::Indeterminate
            } else {
                TransactionState::RolledBack
            }
        );
        assert_eq!(
            database
                .inner
                .transactions
                .durable
                .lock()
                .as_ref()
                .unwrap()
                .is_fenced(),
            synchronized
        );
        if synchronized {
            let error = session.execute("INSERT (:Item {n: 3})").unwrap_err();
            assert_eq!(error.kind(), ErrorKind::DurableCommitCanceled);
        }
        drop(session);
        drop(database);
        let (state, count) = replay(&dir, seed);
        assert_eq!(count, if synchronized { 3 } else { 2 });
        assert_eq!(
            state.graph_summary(GraphId::new(1)).unwrap().0,
            if synchronized { 2 } else { 0 }
        );
    }
}

#[test]
fn whole_explicit_failure_and_dropped_session_leave_no_replayable_statement() {
    let (_temp, dir, database, seed, graph) = fixture();
    let session = database.session(&graph).unwrap();
    session
        .start_transaction(TransactionAccessMode::ReadWrite)
        .unwrap();
    session.execute("INSERT (:Item {n: 1})").unwrap();
    assert!(session.execute("NOT GQL").is_err());
    session.rollback_transaction().unwrap();
    session
        .start_transaction(TransactionAccessMode::ReadWrite)
        .unwrap();
    session.execute("INSERT (:Item {n: 2})").unwrap();
    drop(session);
    drop(database);
    let (state, count) = replay(&dir, seed);
    assert_eq!(count, 2);
    assert_eq!(state.graph_summary(GraphId::new(1)).unwrap().0, 0);
}

#[test]
fn direct_catalog_mutation_obeys_durable_outcomes() {
    let (_temp, dir, database, seed, _graph) = fixture();
    *database.inner.failure.lock() = Some(FailurePoint::BeforePublication);
    let path = SchemaPath::regular("selene", "not_live").unwrap();
    let error = database
        .catalog()
        .create_schema(&path, CreatePolicy::Strict)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DurableCommitUnacknowledged);
    assert!(!error.durable_commit_outcome().unwrap().published);
    assert!(database.catalog().snapshot().resolve_schema(&path).is_err());
    drop(database);
    let (state, count) = replay(&dir, seed);
    assert_eq!(count, 3);
    assert!(
        state
            .catalog()
            .reconstruct()
            .unwrap()
            .schema(&selene_catalog::CatalogName::regular("not_live").unwrap())
            .is_some()
    );
}

#[test]
fn active_constraint_backing_is_ready_before_durable_acknowledgment() {
    let (_temp, dir, database, seed, graph) = fixture();
    let ty = ObjectPath::regular("selene", "durable", "closed").unwrap();
    crate::transaction::test_schema::bind_schema(
        &database,
        &graph,
        &ty,
        &["CREATE NODE TYPE :Sensor (serial :: STRING)"],
    );
    let session = database.session(&graph).unwrap();
    session.execute("DROP NODE TYPE :Sensor").unwrap();
    session
        .execute("CREATE NODE TYPE :Sensor (serial :: STRING UNIQUE)")
        .unwrap();
    session.execute("INSERT (:Sensor {serial: 'A'})").unwrap();
    let before = database.inner.state.load_full();
    let progress = database
        .inner
        .transactions
        .durable
        .lock()
        .as_ref()
        .unwrap()
        .progress();
    assert!(session.execute("INSERT (:Sensor {serial: 'A'})").is_err());
    assert!(Arc::ptr_eq(&before, &database.inner.state.load_full()));
    assert_eq!(
        database
            .inner
            .transactions
            .durable
            .lock()
            .as_ref()
            .unwrap()
            .progress(),
        progress
    );
    let graph_id = database
        .catalog()
        .snapshot()
        .resolve_graph(&graph)
        .unwrap()
        .id
        .get();
    let (state, count) = replay(&dir, seed);
    assert_eq!(count as u64, progress.synchronized.sequence);
    assert_eq!(state.graph_summary(GraphId::new(graph_id)).unwrap().0, 1);
    assert!(
        state
            .catalog()
            .descriptors()
            .iter()
            .any(|d| d.kind() == selene_catalog::CatalogObjectKind::Constraint)
    );
}

#[test]
fn real_io_failure_outcomes_preserve_sources_prefix_and_session_state() {
    use selene_persist::logical_stream::{CommitFailure, Fault};
    for fault in [
        Fault::PartialAppend,
        Fault::Synchronize,
        Fault::Truncate,
        Fault::CleanupSync,
    ] {
        let (_temp, dir, database, seed, graph) = fixture();
        let session = database.session(&graph).unwrap();
        session.execute("INSERT (:Item {n: 0})").unwrap();
        session
            .start_transaction(TransactionAccessMode::ReadWrite)
            .unwrap();
        session.execute("INSERT (:Item {n: 1})").unwrap();
        session.execute("INSERT (:Item {n: 2})").unwrap();
        let before = database.inner.state.load_full();
        database
            .inner
            .transactions
            .durable
            .lock()
            .as_mut()
            .unwrap()
            .wal
            .inject_fault(fault);
        let error = session.commit_transaction().unwrap_err();
        let uncertain = matches!(fault, Fault::Truncate | Fault::CleanupSync);
        assert_eq!(
            error.gqlstatus().unwrap().as_str(),
            if uncertain { "40003" } else { "40N01" }
        );
        assert_eq!(
            error.durable_commit_outcome().unwrap().state,
            if uncertain {
                DurableCommitState::Uncertain
            } else {
                DurableCommitState::Canceled
            }
        );
        assert!(!error.durable_commit_outcome().unwrap().published);
        let failure = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<CommitFailure>()
            .unwrap();
        assert_eq!(failure.cleanup.is_some(), uncertain);
        assert!(std::error::Error::source(failure).is_some());
        assert!(Arc::ptr_eq(&before, &database.inner.state.load_full()));
        assert_eq!(
            session.context().transaction().unwrap().state(),
            if uncertain {
                TransactionState::Indeterminate
            } else {
                TransactionState::RolledBack
            }
        );
        assert!(session.execute("INSERT (:Item {n: 3})").is_err());
        drop(session);
        drop(database);
        let mut reader = LogicalReader::open(&dir, &identity(), Limits::default().bytes).unwrap();
        let mut state = seed;
        let mut count = 0;
        while let Some(body) = reader.next_body().unwrap() {
            state = state.apply_body(&body, Limits::default()).unwrap();
            count += 1;
        }
        assert_eq!(count, 3);
        assert_eq!(state.graph_summary(GraphId::new(1)).unwrap().0, 1);
    }
}
