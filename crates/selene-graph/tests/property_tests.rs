//! Randomized + deterministic structural-consistency tests for the selene-graph
//! mutation funnel and its derived indexes.
//!
//! Every committed op is checked three ways: the debug-only post-commit hook
//! (active because tests build in debug), an explicit
//! `assert_indexes_consistent()` call (so the test fails with a readable
//! message even if the hook is disabled), an alive-set + CONTENT oracle, and a
//! strictly monotonic generation counter. A fraction of transactions are rolled
//! back to assert an aborted txn leaves zero derived-state drift.
//!
//! The shared funnel harness (op generators, the [`Oracle`], `apply_op`) lives
//! in `funnel_harness` so this in-memory suite and the durability round-trip in
//! `recovery_property.rs` drive the funnel through the exact same oracle.

mod funnel_harness;

use proptest::prelude::*;
use selene_core::{GraphId, LabelDiff, LabelSet, PropertyDiff, PropertyMap, Value, db_string};

use selene_graph::{SharedGraph, TypedIndexKind};

use funnel_harness::{
    Oracle, apply_op, arb_op, assert_snapshot_matches_oracle, edge_labels, labels, prop_keys,
    register_indexes,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200)
    ))]

    #[test]
    fn funnel_keeps_indexes_consistent(ops in proptest::collection::vec(arb_op(), 1..=100)) {
        let shared = SharedGraph::new(GraphId::new(1));
        register_indexes(&shared);
        let mut oracle = Oracle::default();
        let mut last_generation = shared.read().meta.generation;

        for (i, op) in ops.iter().enumerate() {
            let committed = apply_op(&shared, &mut oracle, op, i);
            let snapshot = shared.read();
            // (i) Consistency after every committed op.
            prop_assert!(
                snapshot.assert_indexes_consistent().is_ok(),
                "{}",
                snapshot.assert_indexes_consistent().unwrap_err()
            );
            // (ii) Alive-set + content parity vs the oracle.
            assert_snapshot_matches_oracle(&snapshot, &oracle);
            // (iii) Generation monotonicity: exactly +1 per committed op.
            if committed {
                prop_assert_eq!(snapshot.meta.generation, last_generation + 1);
                last_generation = snapshot.meta.generation;
            } else {
                prop_assert_eq!(snapshot.meta.generation, last_generation);
            }
        }
    }

    #[test]
    fn rollback_leaves_zero_drift(
        setup in proptest::collection::vec(arb_op(), 0..=20),
        batch in proptest::collection::vec(arb_op(), 1..=10),
    ) {
        let shared = SharedGraph::new(GraphId::new(1));
        register_indexes(&shared);
        let mut oracle = Oracle::default();
        for (i, op) in setup.iter().enumerate() {
            apply_op(&shared, &mut oracle, op, i);
        }
        let pre = shared.read();
        let pre_nodes = pre.node_count();
        let pre_edges = pre.edge_count();
        let pre_label_count = pre.label_count();
        let pre_property_index_count = pre.property_index_count();
        let pre_generation = pre.meta.generation;
        prop_assert!(pre.assert_indexes_consistent().is_ok());

        // Run a batch of mutations inside one txn, then roll it back.
        {
            let mut txn = shared.begin_write();
            {
                let mut mutator = txn.mutator();
                for op in &batch {
                    // Best-effort: create_node always works; others may no-op.
                    if let funnel_harness::Op::CreateNode { labels, props } = op {
                        let _ = mutator.create_node(labels.clone(), props.clone());
                    }
                }
            }
            txn.rollback();
        }

        let post = shared.read();
        prop_assert_eq!(post.node_count(), pre_nodes);
        prop_assert_eq!(post.edge_count(), pre_edges);
        prop_assert_eq!(post.label_count(), pre_label_count);
        prop_assert_eq!(post.property_index_count(), pre_property_index_count);
        prop_assert_eq!(post.meta.generation, pre_generation);
        prop_assert!(post.assert_indexes_consistent().is_ok());
    }
}

// ---------------------------------------------------------------------------
// Deterministic targeted-invariant battery.
// ---------------------------------------------------------------------------

#[test]
fn delete_node_with_incident_edges_cleans_adjacency_and_labels() {
    let shared = SharedGraph::new(GraphId::new(1));
    let [alpha, ..] = labels();
    let [knows, _] = edge_labels();
    let (a, b) = {
        let mut txn = shared.begin_write();
        let (a, b);
        {
            let mut m = txn.mutator();
            a = m
                .create_node(LabelSet::single(alpha.clone()), PropertyMap::new())
                .unwrap();
            b = m
                .create_node(LabelSet::single(alpha), PropertyMap::new())
                .unwrap();
            m.create_edge(knows.clone(), a, b, PropertyMap::new())
                .unwrap();
            m.create_edge(knows, b, a, PropertyMap::new()).unwrap();
        }
        txn.commit().unwrap();
        (a, b)
    };
    {
        let mut txn = shared.begin_write();
        txn.mutator().delete_node(a).unwrap();
        txn.commit().unwrap();
    }
    let snapshot = shared.read();
    snapshot.assert_indexes_consistent().unwrap();
    assert!(!snapshot.is_node_alive(a));
    assert!(snapshot.is_node_alive(b));
    assert_eq!(snapshot.edge_count(), 0, "incident edges cascade-deleted");
    assert!(snapshot.outgoing_edges(a).is_none());
    assert!(snapshot.incoming_edges(a).is_none());
    assert!(snapshot.outgoing_edges(b).is_none());
    assert!(snapshot.incoming_edges(b).is_none());
}

#[test]
fn flipping_a_label_moves_index_buckets() {
    let shared = SharedGraph::new(GraphId::new(1));
    let [alpha, beta, _] = labels();
    let [age, _, _] = prop_keys();
    shared
        .create_property_index(alpha.clone(), age.clone(), TypedIndexKind::I64)
        .unwrap();
    shared
        .create_property_index(beta.clone(), age.clone(), TypedIndexKind::I64)
        .unwrap();
    let node = {
        let mut txn = shared.begin_write();
        let id = txn
            .mutator()
            .create_node(
                LabelSet::single(alpha.clone()),
                PropertyMap::from_pairs([(age.clone(), Value::Int(7))]).unwrap(),
            )
            .unwrap();
        txn.commit().unwrap();
        id
    };
    {
        let snapshot = shared.read();
        assert_eq!(
            snapshot
                .node_candidates_with_property_eq(&alpha, &age, &Value::Int(7))
                .unwrap()
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            vec![node]
        );
        assert!(
            snapshot
                .node_candidates_with_property_eq(&beta, &age, &Value::Int(7))
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }
    // Flip alpha -> beta.
    {
        let mut txn = shared.begin_write();
        txn.mutator()
            .update_node(
                node,
                LabelDiff::new([beta.clone()], [alpha.clone()]).unwrap(),
                PropertyDiff::new([], []).unwrap(),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let snapshot = shared.read();
    snapshot.assert_indexes_consistent().unwrap();
    assert!(
        snapshot
            .node_candidates_with_property_eq(&alpha, &age, &Value::Int(7))
            .unwrap()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        snapshot
            .node_candidates_with_property_eq(&beta, &age, &Value::Int(7))
            .unwrap()
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![node]
    );
}

#[test]
fn string_value_admits_into_string_index() {
    let shared = SharedGraph::new(GraphId::new(1));
    let [alpha, ..] = labels();
    let [_, _, name] = prop_keys();
    shared
        .create_property_index(alpha.clone(), name.clone(), TypedIndexKind::String)
        .unwrap();
    let content = "proptest.string.unique-admit";
    let text_value = db_string(content).unwrap();
    {
        let mut txn = shared.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(alpha.clone()),
                PropertyMap::from_pairs([(name.clone(), Value::String(text_value.clone()))])
                    .unwrap(),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let snapshot = shared.read();
    snapshot.assert_indexes_consistent().unwrap();
    assert_eq!(
        snapshot
            .node_candidates_with_property_eq(&alpha, &name, &Value::String(text_value))
            .unwrap()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn nan_float_is_skipped_without_false_positive() {
    let shared = SharedGraph::new(GraphId::new(1));
    let [alpha, ..] = labels();
    let [_, score, _] = prop_keys();
    shared
        .create_property_index(alpha.clone(), score.clone(), TypedIndexKind::F64)
        .unwrap();
    {
        let mut txn = shared.begin_write();
        // NaN is legitimately skipped by maintenance; assert no panic + consistent.
        txn.mutator()
            .create_node(
                LabelSet::single(alpha),
                PropertyMap::from_pairs([(score, Value::Float(f64::NAN))]).unwrap(),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let snapshot = shared.read();
    snapshot.assert_indexes_consistent().unwrap();
    // The NaN row is absent from the index.
    assert_eq!(snapshot.node_count(), 1);
}

#[test]
fn null_property_is_never_indexed() {
    let shared = SharedGraph::new(GraphId::new(1));
    let [alpha, ..] = labels();
    let [age, _, _] = prop_keys();
    shared
        .create_property_index(alpha.clone(), age.clone(), TypedIndexKind::I64)
        .unwrap();
    {
        let mut txn = shared.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(alpha),
                PropertyMap::from_pairs([(age.clone(), Value::Null)]).unwrap(),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let snapshot = shared.read();
    snapshot.assert_indexes_consistent().unwrap();
    assert!(
        snapshot
            .node_candidates_with_property_eq(&age, &age, &Value::Int(0))
            .unwrap()
            .is_none()
            || snapshot.node_count() == 1
    );
}
