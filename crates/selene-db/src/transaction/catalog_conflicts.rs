//! Direct catalog publication and implicit writes use optimistic base validation.
//!
//! A simultaneous-start barrier cannot require both writers to succeed: catalog
//! publication between implicit staging and commit must instead cause `40000`.
//! Cover that interleaving and both non-overlapping orders explicitly.

use std::sync::{Arc, atomic::Ordering, mpsc};
use std::time::Duration;

use selene_core::{NodeId, Value};

use super::{fixture, ids};
use crate::{
    CreateOutcome, CreatePolicy, Database, ErrorKind, ExecutionOutcome, ObjectPath, SchemaPath,
    Session, TransactionSlotState, TransactionState, WriteSummary,
    database::{DatabaseState, ImplicitCommitPause},
};

const WRITE: &str = "INSERT (:Concurrent { value: 7 }) FINISH";

#[test]
fn catalog_publication_during_implicit_write_rolls_back_before_one_fresh_attempt() {
    let (database, _, graph_path) = fixture();
    let (_, graph_id) = ids(&database, &graph_path);
    let catalog = database.catalog();
    let inner = Arc::clone(&catalog.inner);
    let session = database.session(&graph_path).unwrap();
    let before = inner.state.load_full();
    let constructions = inner
        .replacement_graph_constructions
        .load(Ordering::Relaxed);
    let created_path = SchemaPath::regular("selene", "alongside_write").unwrap();

    std::thread::scope(|scope| {
        let (staged_tx, staged_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        *inner.before_implicit_commit.lock() = Some(ImplicitCommitPause {
            staged: staged_tx,
            resume: resume_rx,
        });
        let writer = scope.spawn(move || {
            // Ordinary execute must auto-start, stage and attempt implicit commit.
            let result = session.execute(WRITE);
            (session, result)
        });
        let staged = staged_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(staged.state(), TransactionState::Active);
        assert_eq!(staged.statement_count(), 1);
        assert_eq!(staged.staged_change_count(), 1);
        assert_eq!(staged.pinned_publication(), before.publication);
        assert_eq!(
            staged.pinned_catalog_generation().get(),
            before.catalog.generation().get()
        );
        assert_eq!(staged.selected_graph().get(), graph_id.get());
        assert_eq!(staged.pinned_graph_generation(), 0);
        assert!(Arc::ptr_eq(&before, &inner.state.load_full()));
        assert_empty_graph(&before, graph_id);

        // Join the winning writer before releasing the losing implicit commit.
        // Completion also proves detached execution does not hold the reservation.
        let created = scope
            .spawn(|| catalog.create_schema(&created_path, CreatePolicy::Strict))
            .join()
            .unwrap()
            .unwrap();
        let CreateOutcome::Created(created) = created else {
            panic!("the direct catalog writer must create exactly one schema");
        };
        let winner = inner.state.load_full();
        assert!(!Arc::ptr_eq(&before, &winner));
        assert_eq!(winner.publication, before.publication + 1);
        assert_eq!(
            winner.catalog.generation().get(),
            before.catalog.generation().get() + 1
        );
        assert_eq!(created.id.get(), before.high_water.schema + 1);
        assert_eq!(winner.high_water.schema, created.id.get());
        assert_eq!(winner.high_water.graph, before.high_water.graph);
        assert_eq!(winner.high_water.graph_type, before.high_water.graph_type);
        assert!(Arc::ptr_eq(
            &before.graphs[&graph_id],
            &winner.graphs[&graph_id]
        ));
        assert_empty_graph(&winner, graph_id);

        resume_tx.send(()).unwrap();
        let (session, result) = writer.join().unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TransactionRollback);
        assert_eq!(error.gqlstatus().unwrap().as_str(), "40000");
        let rolled_back = session.context().transaction().unwrap();
        assert_eq!(rolled_back.id(), staged.id());
        assert_eq!(rolled_back.state(), TransactionState::RolledBack);
        assert_eq!(rolled_back.statement_count(), 1);
        assert_eq!(rolled_back.staged_change_count(), 1);
        assert_eq!(
            session.context().transaction_slot(),
            TransactionSlotState::RolledBack
        );
        assert_eq!(
            session.context().transaction_retains_detached_state(),
            Some(false)
        );
        assert!(session.context().current_request().is_none());

        // Assert the intermediate rollback before allowing any fresh write: no
        // partial graph publication, ID consumption or lost winning catalog state.
        let after = inner.state.load_full();
        assert!(Arc::ptr_eq(&winner, &after));
        assert_eq!(after.high_water, winner.high_water);
        assert_empty_graph(&after, graph_id);
        assert_eq!(
            catalog.snapshot().resolve_schema(&created_path).unwrap(),
            created
        );
        assert_eq!(
            inner
                .replacement_graph_constructions
                .load(Ordering::Relaxed),
            constructions
        );
        assert_eq!(
            session
                .execute("MATCH (n:Concurrent) RETURN n")
                .unwrap()
                .row_count(),
            Some(0)
        );
        assert!(Arc::ptr_eq(&winner, &inner.state.load_full()));

        // Exactly one NEW transaction, not a retry that hides a failed test or
        // an automatic replay by the engine. Contention has already ended.
        let outcome = session.execute(WRITE).unwrap();
        assert_eq!(outcome.write_summary(), Some(WriteSummary::new(1, None)));
        let fresh = session.context().transaction().unwrap();
        assert_eq!(fresh.id().get(), staged.id().get() + 1);
        assert_eq!(fresh.pinned_publication(), winner.publication);
        assert_eq!(
            catalog.snapshot().resolve_schema(&created_path).unwrap(),
            created
        );
        assert_eq!(
            inner
                .replacement_graph_constructions
                .load(Ordering::Relaxed),
            constructions + 1
        );
        assert_both_effects_once(&database, &graph_path, &created_path, &session, &before);
    });
}

#[test]
fn catalog_before_implicit_write_preserves_both_effects() {
    non_overlapping_writers(true);
}

#[test]
fn implicit_write_before_catalog_preserves_both_effects() {
    non_overlapping_writers(false);
}

fn non_overlapping_writers(catalog_first: bool) {
    let (database, _, graph_path) = fixture();
    let catalog = database.catalog();
    let before = catalog.inner.state.load_full();
    let session = database.session(&graph_path).unwrap();
    let created_path = SchemaPath::regular("selene", "alongside_write").unwrap();
    let create = || {
        catalog
            .create_schema(&created_path, CreatePolicy::Strict)
            .unwrap()
    };
    let write = || {
        let outcome = session.execute(WRITE).unwrap();
        assert_eq!(outcome.write_summary(), Some(WriteSummary::new(1, None)));
    };
    let created = if catalog_first {
        let created = create();
        write();
        created
    } else {
        write();
        create()
    };
    let CreateOutcome::Created(created) = created else {
        panic!("the direct catalog writer must create exactly one schema");
    };
    assert_eq!(
        catalog.snapshot().resolve_schema(&created_path).unwrap(),
        created
    );
    assert_both_effects_once(&database, &graph_path, &created_path, &session, &before);
}

fn assert_empty_graph(state: &DatabaseState, graph_id: selene_catalog::GraphId) {
    let graph = state.graphs[&graph_id].graph.read();
    assert_eq!(graph.graph_id().get(), graph_id.get());
    assert_eq!(graph.node_count(), 0);
    assert_eq!(graph.edge_count(), 0);
    assert_eq!(graph.meta.generation, 0);
    assert_eq!(graph.meta.next_node_id, 1);
    assert_eq!(graph.meta.next_edge_id, 1);
}

fn assert_both_effects_once(
    database: &Database,
    graph_path: &ObjectPath,
    created_path: &SchemaPath,
    session: &Session,
    before: &DatabaseState,
) {
    let (_, graph_id) = ids(database, graph_path);
    let catalog = database.catalog();
    let state = catalog.inner.state.load_full();
    assert_eq!(state.publication, before.publication + 2);
    assert_eq!(
        state.catalog.generation().get(),
        before.catalog.generation().get() + 1
    );
    assert_eq!(state.high_water.schema, before.high_water.schema + 1);
    assert_eq!(state.high_water.graph, before.high_water.graph);
    assert_eq!(state.high_water.graph_type, before.high_water.graph_type);
    assert_eq!(
        catalog
            .snapshot()
            .resolve_schema(created_path)
            .unwrap()
            .id
            .get(),
        state.high_water.schema
    );
    let graph = state.graphs[&graph_id].graph.read();
    assert_eq!(graph.graph_id().get(), graph_id.get());
    assert_eq!(graph.node_count(), 1);
    assert_eq!(graph.edge_count(), 0);
    assert_eq!(graph.meta.generation, 1);
    assert_eq!(graph.meta.next_node_id, 2);
    assert_eq!(graph.meta.next_edge_id, 1);
    assert!(graph.is_node_alive(NodeId::new(1)));
    assert_eq!(
        session.context().transaction_slot(),
        TransactionSlotState::Committed
    );
    assert_eq!(
        session.context().transaction_retains_detached_state(),
        Some(false)
    );
    assert!(session.context().current_request().is_none());
    let committed = session.context().transaction().unwrap();
    assert_eq!(committed.statement_count(), 1);
    assert_eq!(committed.staged_change_count(), 1);
    let rows = session
        .execute("MATCH (n:Concurrent) RETURN n, n.value")
        .unwrap();
    let ExecutionOutcome::Rows { result, .. } = rows else {
        panic!("expected the committed node and property");
    };
    assert_eq!(result.rows().len(), 1);
    assert_eq!(
        result.rows()[0].values(),
        &[Value::NodeRef(NodeId::new(1)), Value::Int(7)]
    );
    assert!(Arc::ptr_eq(&state, &catalog.inner.state.load_full()));
}
