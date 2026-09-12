use std::{sync::Arc, thread};

use parking_lot::Mutex;
use selene_core::{Change, GraphId, LabelSet, NodeId, PropertyMap, PropertyValueType, Value};

use crate::{
    GraphTypeDef, IndexProvider, NodeTypeDef, PropertyTypeDef, ProviderError, ProviderTag,
    SharedGraph, ValidationMode,
};

fn db_string(value: &str) -> selene_core::DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn prop(name: &str, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string(name), value)]).expect("test property map is valid")
}

fn person_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("txn.person.graph"),
        node_types: vec![NodeTypeDef {
            name: db_string("txn.person"),
            key_labels: LabelSet::single(db_string("Person")),
            properties: vec![PropertyTypeDef {
                name: db_string("name"),
                value_type: PropertyValueType::String,
                list_element_type: None,
                required: true,
                default: None,
                immutable: false,
                unique: false,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                record_field_types: None,
            }],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    }
}

struct RecordingProvider {
    tag: ProviderTag,
    seen: Arc<Mutex<Vec<(ProviderTag, Change)>>>,
    fail: bool,
}

impl RecordingProvider {
    fn new(tag: ProviderTag, seen: Arc<Mutex<Vec<(ProviderTag, Change)>>>) -> Self {
        Self {
            tag,
            seen,
            fail: false,
        }
    }

    fn failing(tag: ProviderTag, seen: Arc<Mutex<Vec<(ProviderTag, Change)>>>) -> Self {
        Self {
            tag,
            seen,
            fail: true,
        }
    }
}

struct PanicOnSecondChangeProvider {
    tag: ProviderTag,
    seen: Arc<Mutex<Vec<(ProviderTag, Change)>>>,
    calls: Mutex<usize>,
}

impl PanicOnSecondChangeProvider {
    fn new(tag: ProviderTag, seen: Arc<Mutex<Vec<(ProviderTag, Change)>>>) -> Self {
        Self {
            tag,
            seen,
            calls: Mutex::new(0),
        }
    }
}

impl IndexProvider for PanicOnSecondChangeProvider {
    fn provider_tag(&self) -> ProviderTag {
        self.tag
    }

    fn on_change(&self, change: &Change) -> Result<(), ProviderError> {
        self.seen.lock().push((self.tag, change.clone()));
        let mut calls = self.calls.lock();
        *calls += 1;
        assert_ne!(*calls, 2, "synthetic provider panic on the second change");
        Ok(())
    }
}

impl IndexProvider for RecordingProvider {
    fn provider_tag(&self) -> ProviderTag {
        self.tag
    }

    fn on_change(&self, change: &Change) -> Result<(), ProviderError> {
        self.seen.lock().push((self.tag, change.clone()));
        if self.fail {
            Err(ProviderError::Inconsistent {
                reason: "synthetic provider failure".to_owned(),
            })
        } else {
            Ok(())
        }
    }
}

#[test]
fn commit_publishes_new_snapshot() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    let node_id = {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok")
    };
    let outcome = txn.commit().unwrap();
    assert_eq!(outcome.generation, 1);
    assert!(shared.read().is_node_alive(node_id));
}

#[test]
fn rollback_does_not_publish() {
    let shared = SharedGraph::new(GraphId::new(1));
    {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok");
        txn.rollback();
    }
    assert_eq!(shared.read().node_count(), 0);
}

#[test]
fn writer_panic_during_mutation_does_not_leak_partial_state() {
    let shared = Arc::new(SharedGraph::new(GraphId::new(1)));
    let pre_count = shared.read().node_count();

    let shared_for_writer = Arc::clone(&shared);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut txn = shared_for_writer.begin_write();
        {
            let mut mutator = txn.mutator();
            mutator
                .create_node(LabelSet::new(), PropertyMap::new())
                .unwrap();
        }
        panic!("synthetic panic mid-transaction");
    }));

    assert_eq!(shared.read().node_count(), pre_count);
    let txn = shared.begin_write();
    txn.rollback();
}

#[test]
fn pre_txn_arc_shares_with_snapshot_on_begin() {
    let shared = SharedGraph::new(GraphId::new(1));
    let snapshot = shared.read();
    let txn = shared.begin_write();

    assert_eq!(
        Arc::as_ptr(txn.pre_txn.as_ref().expect("pre_txn is stashed")),
        Arc::as_ptr(&snapshot)
    );

    txn.rollback();
}

#[test]
fn first_mutation_detaches_guard_arc() {
    let shared = SharedGraph::new(GraphId::new(1));
    let snapshot = shared.read();
    let mut txn = shared.begin_write();
    let before = Arc::as_ptr(&*txn.guard);

    assert_eq!(before, Arc::as_ptr(&snapshot));

    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node succeeds");
    }

    assert_ne!(Arc::as_ptr(&*txn.guard), before);
    txn.rollback();
}

#[test]
fn subsequent_mutations_do_not_clone_again() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();

    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("first create_node succeeds");
    }
    let after_first = Arc::as_ptr(&*txn.guard);

    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("second create_node succeeds");
    }

    assert_eq!(Arc::as_ptr(&*txn.guard), after_first);
    txn.rollback();
}

#[test]
fn rollback_via_drop_restores_pre_txn_arc() {
    let shared = SharedGraph::new(GraphId::new(1));
    let pre_count = shared.read().node_count();
    let pre_txn_ptr = {
        let mut txn = shared.begin_write();
        let pre_txn_ptr = Arc::as_ptr(txn.pre_txn.as_ref().expect("pre_txn is stashed"));
        {
            let mut mutator = txn.mutator();
            mutator
                .create_node(LabelSet::new(), PropertyMap::new())
                .expect("create_node succeeds");
        }
        pre_txn_ptr
    };

    assert_eq!(shared.read().node_count(), pre_count);
    assert_eq!(shared.locked_arc_ptr_for_test(), pre_txn_ptr);
    assert_eq!(Arc::as_ptr(&shared.read()), pre_txn_ptr);
}

#[test]
fn rollback_via_explicit_method() {
    let shared = SharedGraph::new(GraphId::new(1));
    let pre_count = shared.read().node_count();
    let pre_txn_ptr = {
        let mut txn = shared.begin_write();
        let pre_txn_ptr = Arc::as_ptr(txn.pre_txn.as_ref().expect("pre_txn is stashed"));
        {
            let mut mutator = txn.mutator();
            mutator
                .create_node(LabelSet::new(), PropertyMap::new())
                .expect("create_node succeeds");
        }
        txn.rollback();
        pre_txn_ptr
    };

    assert_eq!(shared.read().node_count(), pre_count);
    assert_eq!(shared.locked_arc_ptr_for_test(), pre_txn_ptr);
    assert_eq!(Arc::as_ptr(&shared.read()), pre_txn_ptr);
}

#[test]
fn rollback_after_panic_during_mutation() {
    let shared = Arc::new(SharedGraph::new(GraphId::new(1)));
    let pre_count = shared.read().node_count();

    let shared_for_writer = Arc::clone(&shared);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut txn = shared_for_writer.begin_write();
        {
            let mut mutator = txn.mutator();
            mutator
                .create_node(LabelSet::new(), PropertyMap::new())
                .expect("create_node succeeds");
        }
        panic!("synthetic panic during mutation");
    }));

    assert!(result.is_err());
    assert_eq!(shared.read().node_count(), pre_count);
    let txn = shared.begin_write();
    txn.rollback();
}

#[test]
fn rollback_after_counter_exhaustion_during_meta_bump() {
    let mut graph = crate::SeleneGraph::new(GraphId::new(1));
    graph.meta.generation = u64::MAX;
    let shared = Arc::new(SharedGraph::from_graph(graph));
    let pre_count = shared.read().node_count();

    let shared_for_writer = Arc::clone(&shared);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut txn = shared_for_writer.begin_write();
        {
            let mut mutator = txn.mutator();
            mutator
                .create_node(LabelSet::new(), PropertyMap::new())
                .expect("create_node succeeds");
        }
        txn.commit()
    }));

    assert!(matches!(
        result.unwrap(),
        Err(crate::GraphError::CounterExhausted {
            kind: "graph generation"
        })
    ));
    assert_eq!(shared.read().node_count(), pre_count);
    assert_eq!(shared.read().meta.generation, u64::MAX);
    let txn = shared.begin_write();
    txn.rollback();
}

#[test]
fn validation_failure_rolls_back_and_does_not_bump_generation() {
    let shared = SharedGraph::builder(GraphId::new(1))
        .bound_to(person_graph_type())
        .expect("graph type is valid")
        .build()
        .expect("shared graph builds");
    let pre_count = shared.read().node_count();
    let pre_generation = shared.read().meta.generation;
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(db_string("Person")),
                prop("name", Value::Int(7)),
            )
            .expect("mutation accepts before commit-time validation");
    }

    let error = txn.commit().expect_err("commit rejects invalid type");

    assert!(matches!(
        error,
        crate::GraphError::TypeViolation(crate::TypeViolation::PropertyTypeMismatch { .. })
    ));
    assert_eq!(shared.read().node_count(), pre_count);
    assert_eq!(shared.read().meta.generation, pre_generation);
}

#[test]
fn commit_publishes_arc_shared_with_lock_after_publish() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node succeeds");
    }
    txn.commit().expect("commit succeeds");

    assert_eq!(
        shared.locked_arc_ptr_for_test(),
        Arc::as_ptr(&shared.read())
    );
}

#[test]
fn aborted_tx_ids_become_permanent_holes() {
    let shared = SharedGraph::new(GraphId::new(1));
    {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        assert_eq!(
            mutator
                .create_node(LabelSet::new(), PropertyMap::new())
                .expect("create_node ok"),
            NodeId::new(1)
        );
    }
    {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        assert_eq!(
            mutator
                .create_node(LabelSet::new(), PropertyMap::new())
                .expect("create_node ok"),
            NodeId::new(2)
        );
        txn.commit().unwrap();
    }
    let snapshot = shared.read();
    assert!(!snapshot.is_node_alive(NodeId::new(1)));
    assert!(snapshot.is_node_alive(NodeId::new(2)));
    assert_eq!(snapshot.meta.next_node_id, 3);
}

#[test]
fn commit_with_principal_carries_principal_to_outcome() {
    let shared = SharedGraph::new(GraphId::new(1));
    let principal = Arc::from([1_u8, 2, 3]);
    let outcome = shared
        .begin_write()
        .commit_with_principal(Some(Arc::clone(&principal)))
        .unwrap();
    assert_eq!(outcome.principal.as_deref(), Some(&principal[..]));
}

#[test]
fn commit_returns_changes_in_order() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    let label = db_string("txn.node");
    {
        let mut mutator = txn.mutator();
        let id = mutator
            .create_node(LabelSet::single(label), PropertyMap::new())
            .expect("create_node ok");
        let prop = db_string("txn.prop");
        let diff = selene_core::PropertyDiff::new([(prop, Value::Int(1))], []).unwrap();
        mutator
            .update_node(id, selene_core::LabelDiff::new([], []).unwrap(), diff)
            .unwrap();
    }
    let outcome = txn.commit().unwrap();
    assert!(matches!(outcome.changes[0], Change::NodeCreated { .. }));
    assert!(matches!(outcome.changes[1], Change::NodeUpdated { .. }));
}

#[test]
fn commit_calls_on_change_for_every_change_in_order() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = SharedGraph::builder(GraphId::new(1))
        .with_provider(Arc::new(RecordingProvider::new(
            ProviderTag(*b"RECD"),
            Arc::clone(&seen),
        )))
        .build()
        .unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let id = mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok");
        mutator.delete_node(id).unwrap();
    }
    txn.commit().unwrap();
    let seen = seen.lock();
    assert!(matches!(seen[0].1, Change::NodeCreated { .. }));
    assert!(matches!(seen[1].1, Change::NodeDeleted { .. }));
}

#[test]
fn commit_calls_each_provider_in_registration_order() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = SharedGraph::builder(GraphId::new(1))
        .with_provider(Arc::new(RecordingProvider::new(
            ProviderTag(*b"ONE1"),
            Arc::clone(&seen),
        )))
        .with_provider(Arc::new(RecordingProvider::new(
            ProviderTag(*b"TWO2"),
            Arc::clone(&seen),
        )))
        .build()
        .unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok");
    }
    txn.commit().unwrap();
    let tags = seen.lock().iter().map(|(tag, _)| *tag).collect::<Vec<_>>();
    assert_eq!(tags, vec![ProviderTag(*b"ONE1"), ProviderTag(*b"TWO2")]);
}

#[test]
fn multi_provider_commit_ordering() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = SharedGraph::builder(GraphId::new(1))
        .with_provider(Arc::new(RecordingProvider::new(
            ProviderTag(*b"ONE1"),
            Arc::clone(&seen),
        )))
        .with_provider(Arc::new(RecordingProvider::new(
            ProviderTag(*b"TWO2"),
            Arc::clone(&seen),
        )))
        .build()
        .unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let id = mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok");
        mutator.delete_node(id).expect("delete_node ok");
    }
    txn.commit().unwrap();

    let seen = seen.lock();
    let provider_one = seen
        .iter()
        .filter(|(tag, _)| *tag == ProviderTag(*b"ONE1"))
        .map(|(_, change)| change)
        .collect::<Vec<_>>();
    let provider_two = seen
        .iter()
        .filter(|(tag, _)| *tag == ProviderTag(*b"TWO2"))
        .map(|(_, change)| change)
        .collect::<Vec<_>>();
    assert_eq!(provider_one.len(), 2);
    assert_eq!(provider_two.len(), 2);
    assert!(matches!(provider_one[0], Change::NodeCreated { .. }));
    assert!(matches!(provider_one[1], Change::NodeDeleted { .. }));
    assert!(matches!(provider_two[0], Change::NodeCreated { .. }));
    assert!(matches!(provider_two[1], Change::NodeDeleted { .. }));
}

#[test]
fn provider_panic_isolation() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = SharedGraph::builder(GraphId::new(1))
        .with_provider(Arc::new(PanicOnSecondChangeProvider::new(
            ProviderTag(*b"PAN1"),
            Arc::clone(&seen),
        )))
        .with_provider(Arc::new(RecordingProvider::new(
            ProviderTag(*b"OKAY"),
            Arc::clone(&seen),
        )))
        .build()
        .unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let id = mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok");
        let prop = db_string("txn.panic.prop");
        let diff = selene_core::PropertyDiff::new([(prop, Value::Int(1))], []).unwrap();
        mutator
            .update_node(id, selene_core::LabelDiff::new([], []).unwrap(), diff)
            .expect("update_node ok");
        mutator.delete_node(id).expect("delete_node ok");
    }
    let outcome = txn.commit().unwrap();
    assert_eq!(outcome.changes.len(), 3);

    let seen = seen.lock();
    let panicking_count = seen
        .iter()
        .filter(|(tag, _)| *tag == ProviderTag(*b"PAN1"))
        .count();
    let other_count = seen
        .iter()
        .filter(|(tag, _)| *tag == ProviderTag(*b"OKAY"))
        .count();
    assert_eq!(panicking_count, 3);
    assert_eq!(other_count, 3);
}

#[test]
fn provider_error_does_not_fail_commit() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = SharedGraph::builder(GraphId::new(1))
        .with_provider(Arc::new(RecordingProvider::failing(
            ProviderTag(*b"FAIL"),
            Arc::clone(&seen),
        )))
        .build()
        .unwrap();
    let mut txn = shared.begin_write();
    let id = {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok")
    };
    let outcome = txn.commit().unwrap();
    assert!(shared.read().is_node_alive(id));
    assert_eq!(outcome.changes.len(), 1);
    assert_eq!(seen.lock().len(), 1);
}

#[test]
fn rollback_does_not_call_on_change() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = SharedGraph::builder(GraphId::new(1))
        .with_provider(Arc::new(RecordingProvider::new(
            ProviderTag(*b"ROLL"),
            Arc::clone(&seen),
        )))
        .build()
        .unwrap();
    {
        let mut txn = shared.begin_write();
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok");
        txn.rollback();
    }
    assert!(seen.lock().is_empty());
}

#[test]
fn concurrent_writers_serialize() {
    let shared = Arc::new(SharedGraph::new(GraphId::new(1)));
    thread::scope(|scope| {
        for _ in 0..4 {
            let shared = Arc::clone(&shared);
            scope.spawn(move || {
                let mut txn = shared.begin_write();
                {
                    let mut mutator = txn.mutator();
                    let _ = mutator.create_node(LabelSet::new(), PropertyMap::new());
                }
                txn.commit().unwrap();
            });
        }
    });
    assert_eq!(shared.read().node_count(), 4);
}

/// A provider whose `on_change` tries to start a nested write via
/// `SharedGraph::begin_write()`. With the v1.0 contract this is misuse
/// and must panic - caught by `notify_providers` so the outer commit
/// completes regardless. The `chained_count` increments only if the
/// nested write completes; it must stay at 0.
mod provider_tests;
mod unpublished;
