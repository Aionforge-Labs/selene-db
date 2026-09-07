use std::sync::Arc;

use super::*;

#[test]
fn read_within_tx_sees_own_writes() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    let mut mutator = txn.mutator();
    let id = empty_node(&mut mutator);
    assert!(mutator.read().is_node_alive(id));
}

#[test]
fn read_within_tx_sees_label_index_updates() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    let label = db_string("node.index.tx-read").unwrap();
    let mut mutator = txn.mutator();
    let id = mutator
        .create_node(LabelSet::single(label.clone()), PropertyMap::new())
        .expect("create_node ok");
    let row = mutator
        .read()
        .node_row_for_id(id)
        .expect("created node is mapped")
        .get();
    assert!(
        mutator
            .read()
            .nodes_with_label(&label)
            .unwrap()
            .contains(row)
    );
}

#[test]
fn multi_step_tx_emits_changes_in_order() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    let id = {
        let mut mutator = txn.mutator();
        let id = empty_node(&mut mutator);
        mutator
            .update_node(
                id,
                LabelDiff::new([db_string("node.updated").unwrap()], []).unwrap(),
                PropertyDiff::new([], []).unwrap(),
            )
            .unwrap();
        mutator.delete_node(id).unwrap();
        id
    };
    let outcome = txn.commit().unwrap();
    assert!(matches!(outcome.changes[0], Change::NodeCreated { .. }));
    assert!(matches!(outcome.changes[1], Change::NodeUpdated { .. }));
    assert_eq!(outcome.changes[2], Change::NodeDeleted { id });
}

/// The emitted record must be stamped with the live graph's id, not with the
/// id named anywhere in the payload. `GraphDropped { id: 2 }` on a graph whose
/// id is 77 is the discriminating shape: a `graph` field that echoed the
/// payload, or that a caller could supply, would read 2 here and leave a
/// durable record that recovery refuses as cross-wired.
#[test]
fn schema_change_stamps_the_live_graph_id() {
    let shared = SharedGraph::new(GraphId::new(77));
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator.schema_change(SchemaChange::GraphDropped {
            id: GraphId::new(2),
        });
    }
    let outcome = txn.commit().unwrap();
    let Change::SchemaChanged { graph, .. } = &outcome.changes[0] else {
        panic!(
            "expected a SchemaChanged record, got {:?}",
            outcome.changes[0]
        );
    };
    assert_eq!(*graph, GraphId::new(77));
}

#[test]
#[cfg(not(miri))]
fn four_writer_stress_no_double_allocation() {
    let shared = Arc::new(SharedGraph::new(GraphId::new(1)));
    let nodes_per_thread = 64;
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let shared = Arc::clone(&shared);
            scope.spawn(move || {
                let mut txn = shared.begin_write();
                {
                    let mut mutator = txn.mutator();
                    for _ in 0..nodes_per_thread {
                        mutator
                            .create_node(LabelSet::new(), PropertyMap::new())
                            .expect("create_node ok");
                    }
                }
                txn.commit().unwrap();
            });
        }
    });
    let snapshot = shared.read();
    assert_eq!(snapshot.node_count(), 4 * nodes_per_thread);
    assert_eq!(
        snapshot.meta.next_node_id,
        (4 * nodes_per_thread + 1) as u64
    );
}

#[test]
fn value_type_import_smoke_keeps_schema_deferred() {
    let value_type = ValueType::predefined(PredefinedValueType::String);
    assert_eq!(value_type.predefined, Some(PredefinedValueType::String));
}
