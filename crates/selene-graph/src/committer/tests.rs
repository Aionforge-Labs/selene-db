//! Memory ordering, cancellation and failure ports; no synthetic durable voters.
use crate::{GraphError, SharedGraph};
use selene_core::{GraphId, LabelSet, PropertyMap};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn sealed_node(shared: &SharedGraph) -> crate::write_txn::SealedCommit {
    let mut txn = shared.begin_write();
    txn.mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    txn.seal(None, None).unwrap()
}
fn failing_node(shared: &SharedGraph) -> crate::write_txn::SealedCommit {
    let mut sealed = sealed_node(shared);
    sealed.fail_publish = true;
    sealed
}

#[test]
fn cancel_cutline_in_seal_rolls_back_with_no_burned_state() {
    let shared = SharedGraph::new(GraphId::new(91_001));
    let mut txn = shared.begin_write();
    let burned = txn
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let flag = Arc::new(AtomicBool::new(true));
    let error = txn.seal(None, Some(&flag)).err().unwrap();
    assert!(matches!(error, GraphError::Cancelled));
    assert_eq!(error.gqlstatus(), "5GQL2");
    assert_eq!(shared.read().node_count(), 0);
    assert_eq!(shared.read().meta.generation, 0);
    assert_eq!(shared.locked_generation_for_test(), 0);
    assert_eq!(
        shared.locked_arc_ptr_for_test(),
        Arc::as_ptr(&shared.read())
    );
    let mut txn = shared.begin_write();
    let fresh = txn
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    assert!(fresh > burned, "detached allocation burns remain consumed");
    assert_eq!(txn.commit().unwrap().generation, 1);
}
#[test]
fn cancel_token_unset_at_seal_proceeds_and_is_irrevocable() {
    let shared = SharedGraph::new(GraphId::new(91_002));
    let mut txn = shared.begin_write();
    let id = txn
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let sealed = txn.seal(None, Some(&flag)).unwrap();
    let outcome = shared.submit_sealed_for_test(sealed).unwrap();
    flag.store(true, Ordering::Release);
    assert_eq!(outcome.generation, 1);
    assert!(shared.read().is_node_alive(id));
}
#[test]
fn committer_panic_poisons_and_fails_all_waiters_without_hanging() {
    let shared = SharedGraph::new(GraphId::new(91_004));
    let first = shared
        .submit_sealed_for_test(failing_node(&shared))
        .unwrap_err();
    assert!(matches!(first, GraphError::IndeterminateOutcome { .. }));
    assert_eq!(first.gqlstatus(), "40003");
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(shared.submit_sealed_for_test(sealed_node(&shared)).is_err());
    assert!(shared.compact().is_err());
    assert!(Instant::now() < deadline);
    assert_eq!(shared.read().node_count(), 0);
    assert_eq!(shared.schema_version(), 0);
}
#[test]
fn concurrent_panicking_commits_all_err_without_hanging() {
    let shared = Arc::new(SharedGraph::new(GraphId::new(91_005)));
    let deadline = Instant::now() + Duration::from_secs(10);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || shared.submit_sealed_for_test(failing_node(&shared)))
        })
        .collect();
    for handle in handles {
        assert!(handle.join().unwrap().is_err());
    }
    assert!(Instant::now() < deadline);
    assert_eq!(shared.read().node_count(), 0);
}
#[test]
fn failed_publication_poisons_so_failed_commit_never_leaks() {
    let shared = SharedGraph::new(GraphId::new(91_006));
    let failed = failing_node(&shared);
    let later = sealed_node(&shared);
    // Later state already includes the first mutation, but cannot publish it.
    let waiter = shared.submit_sealed_async_for_test(later).unwrap();
    assert!(shared.submit_sealed_for_test(failed).is_err());
    match waiter.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => assert!(result.is_err()),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("queued waiter hung after publication failure")
        }
    }
    assert_eq!(shared.read().node_count(), 0);
    assert_eq!(shared.read().meta.generation, 0);
}
#[test]
fn returned_publication_error_is_classified_and_fences() {
    let poisoned = Arc::new(AtomicBool::new(false));
    let result = super::unwrap_protected::<()>(
        Ok(Err(GraphError::Inconsistent {
            reason: "failure".into(),
        })),
        &poisoned,
    );
    assert!(matches!(
        result,
        Err(GraphError::IndeterminateOutcome { .. })
    ));
    assert!(poisoned.load(Ordering::Acquire));
}
#[test]
fn reorder_buffer_publishes_in_seal_order_not_arrival_order() {
    let shared = SharedGraph::new(GraphId::new(91_007));
    let a = sealed_node(&shared);
    let b = sealed_node(&shared);
    let b_waiter = shared.submit_sealed_async_for_test(b).unwrap();
    assert!(b_waiter.try_recv().is_err());
    assert_eq!(shared.submit_sealed_for_test(a).unwrap().generation, 1);
    assert_eq!(
        b_waiter
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .generation,
        2
    );
    assert_eq!(shared.read().node_count(), 2);
    assert_eq!(shared.read().meta.generation, 2);
}
#[test]
fn compact_cannot_clobber_an_earlier_sealed_commit() {
    let shared = Arc::new(SharedGraph::new(GraphId::new(91_008)));
    let mut txn = shared.begin_write();
    let ids: Vec<_> = (0..20)
        .map(|_| {
            txn.mutator()
                .create_node(LabelSet::new(), PropertyMap::new())
                .unwrap()
        })
        .collect();
    txn.commit().unwrap();
    let mut txn = shared.begin_write();
    for id in ids {
        txn.mutator().delete_node(id).unwrap();
    }
    txn.commit().unwrap();
    let sealed = sealed_node(&shared);
    let worker_graph = Arc::clone(&shared);
    let worker = std::thread::spawn(move || worker_graph.compact().unwrap());
    for _ in 0..1000 {
        std::thread::yield_now();
    }
    assert_eq!(shared.submit_sealed_for_test(sealed).unwrap().generation, 3);
    assert!(worker.join().unwrap().reclaimed_nodes >= 20);
    assert_eq!(shared.read().node_count(), 1);
    assert_eq!(shared.read().node_store.len(), 1);
    shared.read().assert_indexes_consistent().unwrap();
}
