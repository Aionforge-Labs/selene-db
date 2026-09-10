use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use selene_core::{
    Change, EdgeId, GraphId, GraphTypeId, HlcTimestamp, LabelSet, NodeId, Origin,
    PredefinedValueType, PropertyDef, PropertyMap, SchemaChange, Value, ValueType, db_string,
};
use selene_persist::{
    DEFAULT_WAL_FILE_NAME, PersistError, SectionCompression, SnapshotBuilder, SnapshotConfig,
    SyncPolicy, WalConfig, WalWriter, snapshot_path,
};
use smallvec::smallvec;

use crate::{
    CORE_PROVIDER_TAG, GraphError, GraphTypeDef, PropertyDefaultValue, PropertyElementType,
    PropertyTypeDef, ProviderTag, SharedGraph, ValidationMode,
};

#[path = "recover_tests/variant_tests.rs"]
mod variant_tests;

#[path = "recover_tests/build_vs_recover.rs"]
mod build_vs_recover;
#[path = "recover_tests/catalog_ddl.rs"]
mod catalog_ddl;

#[path = "recover_tests/alter_node_type.rs"]
mod alter_node_type;

#[path = "recover_tests/alter_edge_type.rs"]
mod alter_edge_type;

#[path = "recover_tests/oneof.rs"]
mod oneof;

#[path = "recover_tests/provider_replay.rs"]
mod provider_replay;

#[path = "recovery_attachment_tests.rs"]
mod recovery_attachment_tests;

#[path = "recover_tests/truncate_recovery.rs"]
mod truncate_recovery;

#[path = "recover_tests/nodeid_split_recovery.rs"]
mod nodeid_split_recovery;

#[path = "recover_tests/endpoint_recovery.rs"]
mod endpoint_recovery;

#[path = "recover_tests/record_recovery.rs"]
mod record_recovery;

#[path = "recover_tests/vector_recovery.rs"]
mod vector_recovery;

#[path = "recover_tests/json_recovery.rs"]
mod json_recovery;

#[cfg(unix)]
#[path = "recover_tests/path_anchor.rs"]
mod path_anchor;

#[path = "recover_tests/text_recovery.rs"]
mod text_recovery;

#[path = "recover_tests/serialization.rs"]
mod serialization;

#[path = "recover_tests/tail_repair_report.rs"]
mod tail_repair_report;

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-graph-recover-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    dir
}

fn prop(name: &str, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string(name).unwrap(), value)]).unwrap()
}

fn write_snapshot(dir: &Path, shared: &SharedGraph, sequence: u64) -> PathBuf {
    let outcome = shared
        .write_snapshot(SnapshotConfig {
            dir: dir.to_path_buf(),
            sequence,
            compression: SectionCompression::None,
            fsync: false,
        })
        .unwrap();
    assert_eq!(outcome.snapshot_seq, sequence);
    snapshot_path(dir, sequence)
}

fn append_wal(dir: &Path, snapshot_seq: u64, changes: &[Change]) {
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &path,
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1),
            snapshot_seq,
        },
    )
    .unwrap();
    writer
        .append(HlcTimestamp::zero(), Origin::Local, None, changes)
        .unwrap();
    writer.flush().unwrap();
}

fn recover_err(dir: &Path, graph_id: GraphId) -> GraphError {
    match SharedGraph::recover(dir, graph_id) {
        Ok(_) => panic!("recovery should have failed"),
        Err(error) => error,
    }
}

fn sample_shared_graph() -> SharedGraph {
    let shared = SharedGraph::builder(GraphId::new(7)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let mut ids = Vec::new();
        for index in 0..5 {
            ids.push(
                mutator
                    .create_node(
                        LabelSet::single(db_string("recover.node").unwrap()),
                        prop("recover.index", Value::Int(index)),
                    )
                    .unwrap(),
            );
        }
        mutator
            .create_edge(
                db_string("recover.edge").unwrap(),
                ids[0],
                ids[1],
                prop("recover.weight", Value::Int(9)),
            )
            .unwrap();
        mutator.delete_node(ids[4]).unwrap();
    }
    txn.commit().unwrap();
    shared
}

fn large_shared_graph() -> SharedGraph {
    let shared = SharedGraph::builder(GraphId::new(13)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let mut ids = Vec::with_capacity(100);
        for index in 0..100 {
            ids.push(
                mutator
                    .create_node(
                        LabelSet::single(db_string("recover.large.node").unwrap()),
                        prop("recover.large.index", Value::Int(index)),
                    )
                    .unwrap(),
            );
        }
        for index in 0..200 {
            let source = ids[10 + (index % 90)];
            let target = ids[10 + ((index * 7 + 1) % 90)];
            mutator
                .create_edge(
                    db_string("recover.large.edge").unwrap(),
                    source,
                    target,
                    prop("recover.large.weight", Value::Int(index as i64)),
                )
                .unwrap();
        }
        for id in ids.iter().take(10).copied() {
            mutator.delete_node(id).unwrap();
        }
    }
    txn.commit().unwrap();
    shared
}

fn node_created(id: u64) -> Change {
    Change::NodeCreated {
        id: NodeId::new(id),
        labels: LabelSet::single(db_string("recover.wal.node").unwrap()),
        properties: prop("recover.id", Value::Int(id as i64)),
    }
}

fn expect_prop<'a>(map: &'a PropertyMap, key: &str, expected: &Value) -> &'a Value {
    let key = db_string(key).unwrap();
    let actual = map.get(&key).expect("expected property present");
    assert_eq!(actual, expected);
    actual
}

fn empty_closed_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("recover.closed.graph").unwrap(),
        node_types: Vec::new(),
        edge_types: Vec::new(),
    }
}

#[test]
fn recover_from_empty_dir_returns_empty_graph() {
    let dir = temp_dir("empty");
    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    assert_eq!(recovered.read().node_count(), 0);
    assert_eq!(recovered.read().edge_count(), 0);
    assert!(
        recovered
            .index_provider_by_tag(ProviderTag(CORE_PROVIDER_TAG))
            .is_some()
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_from_snapshot_only_round_trips_nodes_and_edges() {
    let dir = temp_dir("snapshot");
    let shared = sample_shared_graph();
    write_snapshot(&dir, &shared, 1);

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    let snapshot = recovered.read();
    assert_eq!(snapshot.node_count(), 4);
    assert_eq!(snapshot.edge_count(), 1);
    assert!(snapshot.is_node_alive(NodeId::new(1)));
    assert!(!snapshot.is_node_alive(NodeId::new(5)));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_from_wal_only_replays_changes_to_state() {
    let dir = temp_dir("wal-only");
    let edge_label = db_string("recover.wal.edge").unwrap();
    let changes = vec![
        node_created(1),
        node_created(2),
        Change::EdgeCreated {
            directionality: selene_core::EdgeDirectionality::Directed,
            id: EdgeId::new(1),
            label: edge_label.clone(),
            source: NodeId::new(1),
            target: NodeId::new(2),
            properties: prop("recover.wal.weight", Value::Int(5)),
        },
    ];
    append_wal(&dir, 0, &changes);

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    let snapshot = recovered.read();
    assert_eq!(snapshot.node_count(), 2);
    assert_eq!(snapshot.edge_count(), 1);
    assert!(snapshot.outgoing_edges(NodeId::new(1)).is_some());
    let expected_labels = LabelSet::single(db_string("recover.wal.node").unwrap());
    assert_eq!(snapshot.node_labels(NodeId::new(1)), Some(&expected_labels));
    assert_eq!(snapshot.node_labels(NodeId::new(2)), Some(&expected_labels));
    expect_prop(
        snapshot.node_properties(NodeId::new(1)).unwrap(),
        "recover.id",
        &Value::Int(1),
    );
    expect_prop(
        snapshot.node_properties(NodeId::new(2)).unwrap(),
        "recover.id",
        &Value::Int(2),
    );
    assert_eq!(snapshot.edge_label(EdgeId::new(1)), Some(&edge_label));
    assert_eq!(
        snapshot.edge_endpoints(EdgeId::new(1)),
        Some((NodeId::new(1), NodeId::new(2)))
    );
    expect_prop(
        snapshot.edge_properties(EdgeId::new(1)).unwrap(),
        "recover.wal.weight",
        &Value::Int(5),
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_closed_wal_only_preserves_typed_list_property() {
    let dir = temp_dir("closed-schema-list-wal-only");
    let graph_id = GraphId::new(21);
    let base = empty_closed_graph_type();
    let shared = SharedGraph::builder(graph_id)
        .bound_to(base.clone())
        .unwrap()
        .build()
        .unwrap();
    let sensor = db_string("ListSensor").unwrap();
    let readings = db_string("readings").unwrap();
    let element_type = PropertyElementType::List(Box::new(PropertyElementType::NotNull(Box::new(
        PropertyElementType::Scalar(selene_core::PropertyValueType::Int),
    ))));
    let changes = {
        let mut txn = shared.begin_write();
        txn.mutator()
            .create_node_type(
                sensor.clone(),
                LabelSet::single(sensor),
                vec![PropertyTypeDef {
                    name: readings.clone(),
                    value_type: selene_core::PropertyValueType::List,
                    list_element_type: Some(element_type.clone()),
                    required: false,
                    default: None,
                    immutable: false,
                    unique: false,
                    decimal_type: None,
                    character_string_type: None,
                    byte_string_type: None,
                    record_field_types: None,
                }],
                ValidationMode::Strict,
            )
            .unwrap();
        txn.commit().unwrap().changes
    };
    append_wal(&dir, 0, &changes);

    let recovered = SharedGraph::recover_closed(&dir, graph_id, base).unwrap();
    let graph_type = recovered.graph_type().unwrap();
    let property = &graph_type.node_types[0].properties[0];
    assert_eq!(property.name, readings);
    assert_eq!(property.value_type, selene_core::PropertyValueType::List);
    assert_eq!(property.list_element_type, Some(element_type));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_closed_rejects_overdeep_typed_list_property() {
    let dir = temp_dir("closed-schema-list-depth");
    let graph_id = GraphId::new(22);
    let base = empty_closed_graph_type();
    let graph_type = GraphTypeId::new(1).unwrap();
    let sensor = db_string("DeepListSensor").unwrap();
    let mut value_type = ValueType::predefined(PredefinedValueType::Int);
    for _ in 0..=64 {
        value_type = ValueType::list_of(value_type);
    }
    append_wal(
        &dir,
        0,
        &[Change::SchemaChanged {
            graph: graph_id,
            change: SchemaChange::NodeTypeAddedV2 {
                graph_type,
                label: sensor.clone(),
                def: selene_core::NodeTypeDef {
                    labels: LabelSet::single(sensor),
                    properties: smallvec![PropertyDef {
                        name: db_string("too_deep").unwrap(),
                        value_type,
                        nullable: true,
                        default: None,
                        immutable: false,
                        unique: false,
                        record_fields: None,
                    }],
                    key: None,
                    validation_mode: selene_core::ValidationMode::Strict,
                },
            },
        }],
    );

    let err = match SharedGraph::recover_closed(&dir, graph_id, base) {
        Ok(_) => panic!("overdeep LIST property recovery should fail"),
        Err(err) => err,
    };
    assert!(format!("{err}").contains("LIST nesting limit"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_from_snapshot_and_wal_streams_only_post_snapshot_changes() {
    let dir = temp_dir("snapshot-wal");
    let shared = sample_shared_graph();
    write_snapshot(&dir, &shared, 3);
    append_wal(&dir, 3, &[node_created(6)]);

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    let snapshot = recovered.read();
    assert!(snapshot.is_node_alive(NodeId::new(1)));
    assert!(snapshot.is_node_alive(NodeId::new(6)));
    assert!(!snapshot.is_node_alive(NodeId::new(5)));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_then_commit_continues_id_allocation_above_recovered_floor() {
    let dir = temp_dir("allocation");
    append_wal(&dir, 0, &[node_created(49)]);

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    let mut txn = recovered.begin_write();
    let id = {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .unwrap()
    };
    assert_eq!(id, NodeId::new(50));
    txn.commit().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_with_corrupt_snapshot_returns_persist_error() {
    let dir = temp_dir("corrupt");
    let shared = sample_shared_graph();
    let path = write_snapshot(&dir, &shared, 1);
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.last_mut().unwrap();
    *last ^= 0xAA;
    fs::write(&path, bytes).unwrap();

    let err = recover_err(&dir, GraphId::new(7));
    assert!(matches!(
        err,
        GraphError::Persist(PersistError::BodyHashMismatch { .. })
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_returns_persist_error_for_unknown_provider_in_section_table() {
    let dir = temp_dir("unknown-provider");
    let mut builder = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.clone(),
        sequence: 1,
        compression: SectionCompression::None,
        fsync: false,
    })
    .unwrap();
    builder
        .add_section(*b"MISS", *b"BODY", vec![1_u8, 2, 3])
        .unwrap();
    builder.finalize().unwrap();

    let err = recover_err(&dir, GraphId::new(7));
    assert!(matches!(
        err,
        GraphError::Persist(PersistError::UnknownProvider { provider, sub })
            if provider == *b"MISS" && sub == *b"BODY"
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn live_mutate_snapshot_recover_matches_state() {
    let dir = temp_dir("e2e");
    let shared = large_shared_graph();
    let expected = shared.read();
    write_snapshot(&dir, &shared, expected.meta.generation);

    let recovered = SharedGraph::recover(&dir, GraphId::new(13)).unwrap();
    let snapshot = recovered.read();
    assert_eq!(snapshot.node_count(), expected.node_count());
    assert_eq!(snapshot.edge_count(), expected.edge_count());
    assert_eq!(snapshot.meta.next_node_id, expected.meta.next_node_id);
    assert_eq!(snapshot.meta.next_edge_id, expected.meta.next_edge_id);
    assert!(!snapshot.is_node_alive(NodeId::new(1)));
    assert!(!snapshot.is_node_alive(NodeId::new(10)));
    assert!(snapshot.is_node_alive(NodeId::new(11)));
    assert!(snapshot.is_edge_alive(EdgeId::new(1)));
    assert_eq!(
        snapshot.edge_endpoints(EdgeId::new(1)),
        expected.edge_endpoints(EdgeId::new(1))
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_adds_logical_commit_count_when_wal_sequence_differs() {
    // Physical WAL sequence is deliberately far above graph generation. One
    // replayed commit must advance generation exactly once, not jump to the WAL
    // sequence and not remain at the snapshot generation.
    let dir = temp_dir("generation");
    let shared = sample_shared_graph();
    let snapshot_generation = shared.read().meta.generation;
    write_snapshot(&dir, &shared, 100);
    append_wal(&dir, 100, &[node_created(60)]);

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    let recovered_generation = recovered.read().meta.generation;
    assert_eq!(recovered_generation, snapshot_generation + 1);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_counts_unflagged_empty_wal_commit_as_generation() {
    let dir = temp_dir("empty-commit-generation");
    let graph_id = GraphId::new(71);
    let shared = SharedGraph::builder(graph_id)
        .with_wal(dir.join(DEFAULT_WAL_FILE_NAME), WalConfig::default())
        .unwrap()
        .build()
        .unwrap();

    let committed = shared.begin_write().commit().unwrap();
    assert!(committed.changes.is_empty());
    assert_eq!(shared.read().meta.generation, 1);
    drop(shared);

    let recovered = SharedGraph::recover(&dir, graph_id).unwrap();
    assert_eq!(recovered.read().meta.generation, 1);
    drop(recovered);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_rejects_graph_id_disagreeing_with_snapshot_meta() {
    // META carries the canonical graph identity; recover() callers must agree.
    // Regression guard for the bug where a hardcoded GraphId(1) fallback
    // silently reconstructed under the wrong identity.
    let dir = temp_dir("identity-mismatch");
    let shared = sample_shared_graph(); // uses GraphId::new(7)
    write_snapshot(&dir, &shared, 1);

    let err = recover_err(&dir, GraphId::new(99));
    assert!(matches!(
        err,
        GraphError::Provider(crate::ProviderError::Inconsistent { reason })
            if reason.contains("CORE/META declares") && reason.contains("caller asserted")
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_rejects_wal_recreating_tombstoned_id() {
    // WAL streams must never recreate an id that has already been seen,
    // even when the prior row is tombstoned. Regression guard for D11
    // (no ID reuse).
    let dir = temp_dir("tombstone-reuse");
    let changes = vec![
        node_created(1),
        Change::NodeDeleted { id: NodeId::new(1) },
        node_created(1), // illegal: id 1 was already allocated and tombstoned
    ];
    append_wal(&dir, 0, &changes);

    let err = recover_err(&dir, GraphId::new(7));
    let GraphError::Persist(PersistError::ProviderFailed { source, .. }) = &err else {
        panic!("expected PersistError::ProviderFailed, got {err:?}");
    };
    let message = format!("{source}");
    assert!(
        message.contains("recreate") && message.contains("D11"),
        "expected D11 reuse rejection, got: {message}",
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_round_trips_property_index_registrations() {
    // Property index registrations live in CORE/SCMA; restart must surface
    // the same registrations or indexed lookups regress to None. Regression
    // guard for the empty-SCMA bug.
    use crate::TypedIndexKind;

    let dir = temp_dir("scma");
    let shared = sample_shared_graph();
    let label = db_string("recover.node").unwrap();
    let property = db_string("recover.index").unwrap();
    shared
        .create_property_index(label.clone(), property.clone(), TypedIndexKind::I64)
        .unwrap();
    write_snapshot(&dir, &shared, shared.read().meta.generation);

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    assert!(
        recovered
            .read()
            .property_index_for(&label, &property)
            .is_some(),
        "registered property index must round-trip across recover()"
    );
    assert_eq!(recovered.read().property_index_count(), 1);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn recover_round_trips_named_property_index_registrations() {
    use crate::TypedIndexKind;

    let dir = temp_dir("scma-named");
    let shared = sample_shared_graph();
    let label = db_string("recover.named.node").unwrap();
    let property = db_string("recover.named.index").unwrap();
    let name = db_string("recover_named_index").unwrap();
    shared
        .create_property_index_named(
            label.clone(),
            property.clone(),
            TypedIndexKind::I64,
            Some(name.clone()),
        )
        .unwrap();
    write_snapshot(&dir, &shared, shared.read().meta.generation);

    let recovered = SharedGraph::recover(&dir, GraphId::new(7)).unwrap();
    let entries = recovered
        .read()
        .iter_property_index_entries()
        .collect::<Vec<_>>();
    assert_eq!(
        entries,
        vec![(label, property, TypedIndexKind::I64, Some(name))]
    );
    let _ = fs::remove_dir_all(dir);
}
