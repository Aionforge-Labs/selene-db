//! Mixed-edge identity and incidence regression tests.

use selene_core::{
    EdgeDirectionality::{Directed, Undirected},
    GraphId, LabelSet, PropertyMap, db_string,
};
use selene_graph::SharedGraph;

#[test]
fn mixed_parallel_edges_and_loops_keep_one_identity_each() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    let (a, b, ids) = {
        let mut m = txn.mutator();
        let a = m.create_node(LabelSet::new(), PropertyMap::new()).unwrap();
        let b = m.create_node(LabelSet::new(), PropertyMap::new()).unwrap();
        let ids = [
            (a, b, Directed),
            (b, a, Directed),
            (a, b, Undirected),
            (b, a, Undirected),
            (a, a, Directed),
            (a, a, Undirected),
        ]
        .map(|(first, second, kind)| {
            m.create_mixed_edge(
                db_string("E").unwrap(),
                first,
                second,
                kind,
                PropertyMap::new(),
            )
            .unwrap()
        });
        (a, b, ids)
    };
    txn.commit().unwrap();
    let before = shared.read();
    assert_eq!(before.edge_count(), 6);
    assert_eq!(
        ids.into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        6
    );
    assert_eq!(before.edge_endpoints(ids[0]), Some((a, b)));
    assert_eq!(before.edge_endpoints(ids[1]), Some((b, a)));
    for id in [ids[2], ids[3]] {
        let record = before.edge_record(id).unwrap();
        assert_eq!(
            (record.first, record.second, record.directionality),
            (a, b, Undirected)
        );
    }
    assert_eq!(before.undirected_edges(a).unwrap().len(), 3);
    assert_eq!(before.undirected_edges(b).unwrap().len(), 2);
    let mut txn = shared.begin_write();
    txn.mutator().delete_edge(ids[2]).unwrap();
    txn.commit().unwrap();
    assert!(shared.read().is_edge_alive(ids[3]));
    assert_eq!(shared.read().undirected_edges(b).unwrap().len(), 1);
    assert_eq!(
        before.undirected_edges(b).unwrap().len(),
        2,
        "COW snapshot is isolated"
    );
    let mut txn = shared.begin_write();
    txn.mutator().delete_node(a).unwrap();
    let deleted = txn.commit().unwrap();
    let tombstones = deleted
        .changes
        .iter()
        .filter_map(|change| match change {
            selene_core::Change::EdgeDeleted { id } => Some(*id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tombstones.len(), 5);
    assert_eq!(
        tombstones
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        5
    );
    let after = shared.read();
    assert_eq!(after.edge_count(), 0);
    assert!(!after.node_has_incident_edges(b));
    assert!(after.is_node_alive(b));
}

#[test]
fn abort_leaves_no_undirected_incidence_and_does_not_reuse_identity() {
    let shared = SharedGraph::new(GraphId::new(1));
    let mut txn = shared.begin_write();
    let a = txn
        .mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .unwrap();
    txn.commit().unwrap();
    let abandoned = {
        let mut txn = shared.begin_write();
        txn.mutator()
            .create_mixed_edge(
                db_string("E").unwrap(),
                a,
                a,
                Undirected,
                PropertyMap::new(),
            )
            .unwrap()
    };
    assert_eq!(shared.read().edge_count(), 0);
    assert!(shared.read().undirected_edges(a).is_none());
    let mut txn = shared.begin_write();
    let next = txn
        .mutator()
        .create_mixed_edge(
            db_string("E").unwrap(),
            a,
            a,
            Undirected,
            PropertyMap::new(),
        )
        .unwrap();
    assert_ne!(abandoned, next);
    txn.commit().unwrap();
}

#[test]
fn logical_changes_and_compaction_preserve_independent_edge_records() {
    use selene_core::{Change, EdgeRecordV1, NodeId, PropertyDiff, Value};
    use selene_graph::CoreProvider;
    use selene_persist::RecoveryProvider;

    let shared = SharedGraph::new(GraphId::new(2));
    let mut txn = shared.begin_write();
    let (a, b, dead, edge) = {
        let mut m = txn.mutator();
        let a = m.create_node(LabelSet::new(), PropertyMap::new()).unwrap();
        let b = m.create_node(LabelSet::new(), PropertyMap::new()).unwrap();
        let dead = m
            .create_edge(db_string("discard").unwrap(), a, b, PropertyMap::new())
            .unwrap();
        let edge = m
            .create_mixed_edge(
                db_string("E").unwrap(),
                b,
                a,
                Undirected,
                PropertyMap::new(),
            )
            .unwrap();
        m.update_edge(
            edge,
            PropertyDiff::new([(db_string("weight").unwrap(), Value::Int(7))], []).unwrap(),
        )
        .unwrap();
        m.delete_edge(dead).unwrap();
        (a, b, dead, edge)
    };
    let outcome = txn.commit().unwrap();
    let expected = EdgeRecordV1 {
        id: edge,
        label: db_string("E").unwrap(),
        directionality: Undirected,
        first: a,
        second: b,
        properties: PropertyMap::from_pairs([(db_string("weight").unwrap(), Value::Int(7))])
            .unwrap(),
    };
    assert!(outcome.changes.iter().any(|change| matches!(change,
        Change::EdgeCreated { id, source, target, directionality: Undirected, .. }
        if *id == edge && *source == a && *target == b)));
    let provider = CoreProvider::new_for_recovery();
    for change in &outcome.changes {
        RecoveryProvider::on_change(provider.as_ref(), change).unwrap();
    }
    let replayed =
        SharedGraph::try_from_graph(provider.finish_recovery(GraphId::new(2), None).unwrap())
            .unwrap();
    assert_eq!(replayed.read().edge_record(edge), Some(expected.clone()));
    assert_eq!(replayed.read().undirected_edges(b).unwrap().len(), 1);
    assert!(!replayed.read().is_edge_alive(dead));
    assert_eq!(shared.compact().unwrap().reclaimed_edges, 1);
    assert_eq!(shared.read().edge_record(edge), Some(expected.clone()));

    // Independently authored semantic inputs, not a serializer round trip.
    let independent = CoreProvider::new_for_recovery();
    for id in [a, b] {
        RecoveryProvider::on_change(
            independent.as_ref(),
            &Change::NodeCreated {
                id,
                labels: LabelSet::new(),
                properties: PropertyMap::new(),
            },
        )
        .unwrap();
    }
    RecoveryProvider::on_change(independent.as_ref(), &Change::from(expected.clone())).unwrap();
    let mut invalid = expected.clone();
    invalid.id = selene_core::EdgeId::new(123);
    invalid.first = NodeId::new(999);
    assert!(RecoveryProvider::on_change(independent.as_ref(), &Change::from(invalid)).is_err());
    let rebuilt =
        SharedGraph::try_from_graph(independent.finish_recovery(GraphId::new(2), None).unwrap())
            .unwrap();
    assert_eq!(rebuilt.read().edge_record(edge), Some(expected));
}

#[test]
fn closed_unordered_endpoints_revalidate_labels_and_reject_without_incidence() {
    use selene_core::{LabelDiff, NodeId, PropertyDiff};
    use selene_graph::{
        EdgeEndpointDef, EdgeTypeDef, GraphError, GraphTypeDef, NodeTypeDef, ValidationMode,
    };
    let graph_type = GraphTypeDef {
        name: db_string("G").unwrap(),
        node_types: ["A", "B", "C"]
            .map(|name| NodeTypeDef {
                name: db_string(name).unwrap(),
                key_labels: LabelSet::single(db_string(name).unwrap()),
                properties: vec![],
                validation_mode: ValidationMode::Strict,
            })
            .to_vec(),
        edge_types: vec![EdgeTypeDef {
            name: db_string("E").unwrap(),
            label: db_string("E").unwrap(),
            source_node_type: EdgeEndpointDef::NodeType(0),
            target_node_type: EdgeEndpointDef::NodeType(1),
            properties: vec![],
            validation_mode: ValidationMode::Strict,
        }],
    };
    let shared = SharedGraph::builder(GraphId::new(3))
        .bound_to(graph_type)
        .unwrap()
        .build()
        .unwrap();
    let mut txn = shared.begin_write();
    let (b, a, c, edge) = {
        let mut m = txn.mutator();
        // Canonical IDs have the reverse order from the declared endpoint types.
        let b = m
            .create_node(
                LabelSet::single(db_string("B").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
        let a = m
            .create_node(
                LabelSet::single(db_string("A").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
        let c = m
            .create_node(
                LabelSet::single(db_string("C").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
        let edge = m
            .create_mixed_edge(
                db_string("E").unwrap(),
                a,
                b,
                Undirected,
                PropertyMap::new(),
            )
            .unwrap();
        let mismatch = m
            .create_mixed_edge(
                db_string("E").unwrap(),
                a,
                c,
                Undirected,
                PropertyMap::new(),
            )
            .unwrap_err();
        assert!(matches!(mismatch, GraphError::TypeViolation(_)));
        assert!(!m.read().node_has_incident_edges(c));
        assert_eq!(m.read().edge_count(), 1);
        let missing = m
            .create_mixed_edge(
                db_string("E").unwrap(),
                a,
                NodeId::new(999),
                Undirected,
                PropertyMap::new(),
            )
            .unwrap_err();
        assert!(matches!(missing, GraphError::NodeNotFound { .. }));
        (b, a, c, edge)
    };
    txn.commit().unwrap();
    assert_eq!(shared.read().edge_endpoints(edge), Some((b, a)));
    let mut txn = shared.begin_write();
    txn.mutator()
        .update_node(
            b,
            LabelDiff::new([db_string("C").unwrap()], [db_string("B").unwrap()]).unwrap(),
            PropertyDiff::new([], []).unwrap(),
        )
        .unwrap();
    assert!(matches!(txn.commit(), Err(GraphError::TypeViolation(_))));
    assert_eq!(
        shared.read().node_labels(b),
        Some(&LabelSet::single(db_string("B").unwrap()))
    );
    assert!(!shared.read().node_has_incident_edges(c));
    assert_eq!(shared.read().edge_directionality(edge), Some(Undirected));
}

#[test]
fn snapshot_reconstruction_preserves_both_loop_kinds_and_parallel_edges() {
    use selene_persist::{SectionCompression, SnapshotConfig};
    let dir = std::env::temp_dir().join(format!(
        "selene-mixed-snapshot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let shared = SharedGraph::new(GraphId::new(4));
    let mut txn = shared.begin_write();
    let (a, b, ids) = {
        let mut m = txn.mutator();
        let a = m.create_node(LabelSet::new(), PropertyMap::new()).unwrap();
        let b = m.create_node(LabelSet::new(), PropertyMap::new()).unwrap();
        let ids = [
            (a, b, Directed),
            (b, a, Directed),
            (b, a, Undirected),
            (a, b, Undirected),
            (a, a, Directed),
            (a, a, Undirected),
        ]
        .map(|(first, second, kind)| {
            m.create_mixed_edge(
                db_string("E").unwrap(),
                first,
                second,
                kind,
                PropertyMap::new(),
            )
            .unwrap()
        });
        (a, b, ids)
    };
    txn.commit().unwrap();
    shared
        .write_snapshot(SnapshotConfig {
            dir: dir.clone(),
            sequence: 0,
            compression: SectionCompression::None,
            fsync: false,
        })
        .unwrap();
    let recovered = SharedGraph::recover(&dir, GraphId::new(4)).unwrap();
    for id in ids {
        assert_eq!(
            recovered.read().edge_record(id),
            shared.read().edge_record(id)
        );
    }
    assert_eq!(recovered.read().undirected_edges(a).unwrap().len(), 3);
    assert_eq!(recovered.read().undirected_edges(b).unwrap().len(), 2);
    drop(recovered);
    drop(shared);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn property_index_updates_removal_and_abort_preserve_undirected_identity() {
    use selene_core::{PropertyDiff, Value};
    use selene_graph::TypedIndexKind;
    let shared = SharedGraph::new(GraphId::new(5));
    let label = db_string("E").unwrap();
    let key = db_string("weight").unwrap();
    shared
        .create_edge_property_index(label.clone(), key.clone(), TypedIndexKind::I64)
        .unwrap();
    let mut txn = shared.begin_write();
    let edge = {
        let mut m = txn.mutator();
        let a = m.create_node(LabelSet::new(), PropertyMap::new()).unwrap();
        m.create_mixed_edge(
            label.clone(),
            a,
            a,
            Undirected,
            PropertyMap::from_pairs([(key.clone(), Value::Int(1))]).unwrap(),
        )
        .unwrap()
    };
    txn.commit().unwrap();
    let before = shared.read();
    {
        let mut txn = shared.begin_write();
        txn.mutator()
            .update_edge(
                edge,
                PropertyDiff::new([(key.clone(), Value::Int(2))], []).unwrap(),
            )
            .unwrap();
    }
    assert_eq!(shared.read().edge_record(edge), before.edge_record(edge));
    assert_eq!(
        shared
            .read()
            .edge_property_eq_cardinality(&label, &key, &Value::Int(1)),
        Some(1)
    );
    let mut txn = shared.begin_write();
    txn.mutator()
        .update_edge(
            edge,
            PropertyDiff::new([(key.clone(), Value::Int(2))], []).unwrap(),
        )
        .unwrap();
    txn.commit().unwrap();
    assert_eq!(
        shared
            .read()
            .edge_property_eq_cardinality(&label, &key, &Value::Int(1)),
        Some(0)
    );
    assert_eq!(
        shared
            .read()
            .edge_property_eq_cardinality(&label, &key, &Value::Int(2)),
        Some(1)
    );
    let mut txn = shared.begin_write();
    txn.mutator()
        .remove_edge_property(edge, key.clone())
        .unwrap();
    txn.commit().unwrap();
    assert_eq!(
        shared
            .read()
            .edge_property_eq_cardinality(&label, &key, &Value::Int(2)),
        Some(0)
    );
    assert_eq!(shared.read().edge_directionality(edge), Some(Undirected));
    assert_eq!(
        before.edge_properties(edge).unwrap().get(&key),
        Some(&Value::Int(1))
    );
}
