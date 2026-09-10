//! Named LIST/RECORD values survive legacy snapshot, recovery and WAL paths.
//! Query-only and positional-record rejection is covered by stored_value_boundary.

use selene_core::Record;
use smallvec::smallvec;

use super::*;

/// Fold a `Value::List` around a leaf `depth` times to build a deeply nested
/// list, within the explicit semantic nesting cap.
fn deeply_nested_list(depth: usize) -> Value {
    let mut value = Value::Int(99);
    for _ in 0..depth {
        value = Value::List(vec![value]);
    }
    value
}

/// Property map exercising every composite shape + edge case in one bag:
/// flat list, empty list, nested list-of-list, open record, empty record,
/// record holding both a list field and a nested record field, named null fields,
/// and an 8-deep nested list.
fn composite_property_map(prefix: &str) -> PropertyMap {
    PropertyMap::from_pairs([
        (
            db_string(&format!("{prefix}.list")).unwrap(),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        ),
        (
            db_string(&format!("{prefix}.empty_list")).unwrap(),
            Value::List(vec![]),
        ),
        (
            db_string(&format!("{prefix}.nested_list")).unwrap(),
            Value::List(vec![
                Value::List(vec![Value::Int(1)]),
                Value::List(vec![Value::Int(2), Value::Int(3)]),
            ]),
        ),
        (
            db_string(&format!("{prefix}.record")).unwrap(),
            Value::Record(Box::new(Record::Open(smallvec![
                (db_string("id").unwrap(), Value::Int(7)),
                (
                    db_string("name").unwrap(),
                    Value::String(db_string("ada").unwrap()),
                ),
            ]))),
        ),
        (
            db_string(&format!("{prefix}.empty_record")).unwrap(),
            Value::Record(Box::new(Record::Open(smallvec![]))),
        ),
        (
            db_string(&format!("{prefix}.nested_record")).unwrap(),
            Value::Record(Box::new(Record::Open(smallvec![
                (
                    db_string("tags").unwrap(),
                    Value::List(vec![
                        Value::String(db_string("a").unwrap()),
                        Value::String(db_string("b").unwrap()),
                    ]),
                ),
                (
                    db_string("inner").unwrap(),
                    Value::Record(Box::new(Record::Open(smallvec![(
                        db_string("leaf").unwrap(),
                        Value::Int(42),
                    )]))),
                ),
            ]))),
        ),
        (
            db_string(&format!("{prefix}.record_nulls")).unwrap(),
            Value::Record(Box::new(Record::Open(smallvec![
                (db_string("number").unwrap(), Value::Int(1)),
                (db_string("optional").unwrap(), Value::Null),
                (
                    db_string("text").unwrap(),
                    Value::String(db_string("x").unwrap())
                ),
            ]))),
        ),
        (
            db_string(&format!("{prefix}.deep")).unwrap(),
            deeply_nested_list(8),
        ),
    ])
    .unwrap()
}

/// Commit a single node carrying `props`; return the published graph snapshot.
fn graph_with_node_props(props: PropertyMap) -> SeleneGraph {
    let shared = SharedGraph::builder(GraphId::new(1)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let id = mutator
            .create_node(
                LabelSet::single(db_string("composite.node").unwrap()),
                props,
            )
            .unwrap();
        assert_eq!(id, NodeId::new(1));
    }
    txn.commit().unwrap();
    shared.read().as_ref().clone()
}

/// Commit two endpoint nodes and one edge carrying `edge_props`; return the
/// published graph snapshot. The edge is `EdgeId::new(1)`.
fn graph_with_edge_props(edge_props: PropertyMap) -> SeleneGraph {
    let shared = SharedGraph::builder(GraphId::new(1)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let a = mutator
            .create_node(
                LabelSet::single(db_string("composite.a").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
        let b = mutator
            .create_node(
                LabelSet::single(db_string("composite.b").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
        let edge = mutator
            .create_edge(db_string("composite.edge").unwrap(), a, b, edge_props)
            .unwrap();
        assert_eq!(edge, EdgeId::new(1));
    }
    txn.commit().unwrap();
    shared.read().as_ref().clone()
}

#[test]
fn section_blob_round_trips_composite_node_properties() {
    let expected = composite_property_map("node.section");
    let graph = graph_with_node_props(expected.clone());

    let rows = decode_nodes(&encode_nodes(&graph).unwrap()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.properties, expected);
}

#[test]
fn section_blob_round_trips_composite_edge_properties() {
    let expected = composite_property_map("edge.section");
    let graph = graph_with_edge_props(expected.clone());

    let rows = decode_edges(&encode_edges(&graph).unwrap()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.properties, expected);
}

#[test]
fn recovery_round_trips_composite_node_properties() {
    let expected = composite_property_map("node.recovery");
    let graph = graph_with_node_props(expected.clone());

    let provider = CoreProvider::new_for_recovery();
    IndexProvider::read_section(
        provider.as_ref(),
        SubTag(CORE_NODE_SUB),
        &encode_nodes(&graph).unwrap(),
    )
    .unwrap();
    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();

    assert_eq!(recovered.node_properties(NodeId::new(1)), Some(&expected));
}

#[test]
fn recovery_round_trips_composite_edge_properties() {
    let expected = composite_property_map("edge.recovery");
    let graph = graph_with_edge_props(expected.clone());

    // Recovery reads sections in declared order (NODE before EDGE) so the
    // edge's endpoints exist before the edge section is applied.
    let provider = CoreProvider::new_for_recovery();
    IndexProvider::read_section(
        provider.as_ref(),
        SubTag(CORE_NODE_SUB),
        &encode_nodes(&graph).unwrap(),
    )
    .unwrap();
    IndexProvider::read_section(
        provider.as_ref(),
        SubTag(CORE_EDGE_SUB),
        &encode_edges(&graph).unwrap(),
    )
    .unwrap();
    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();

    assert_eq!(recovered.edge_properties(EdgeId::new(1)), Some(&expected));
}

#[test]
fn wal_round_trips_composite_node_properties() {
    let path = temp_wal_path("composite-node");
    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    let snapshot = Arc::new(ArcSwap::from_pointee(SeleneGraph::new(GraphId::new(1))));
    let provider = CoreProvider::new_for_live_with_wal(snapshot, Some(DurableState::new(writer)));

    let expected = composite_property_map("node.wal");
    let changes = vec![Change::NodeCreated {
        id: NodeId::new(1),
        labels: LabelSet::single(db_string("composite.wal").unwrap()),
        properties: expected.clone(),
    }];
    let timestamp = DurableProvider::next_timestamp(provider.as_ref());
    DurableProvider::write_commit(provider.as_ref(), None, &changes, timestamp).unwrap();
    DurableProvider::flush(provider.as_ref()).unwrap();
    drop(provider);

    let entries = wal_entries(&path);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].changes.len(), 1);
    let recovered = match &entries[0].changes[0] {
        Change::NodeCreated { properties, .. } => properties.clone(),
        other => panic!("expected NodeCreated, got {other:?}"),
    };
    assert_eq!(recovered, expected);
}

#[test]
fn wal_replay_reconstructs_composite_node_properties() {
    let expected = composite_property_map("node.replay");
    let provider = CoreProvider::new_for_recovery();
    IndexProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(db_string("composite.replay").unwrap()),
            properties: expected.clone(),
        },
    )
    .unwrap();
    let recovered = provider.finish_recovery(GraphId::new(1), None).unwrap();

    assert_eq!(recovered.node_properties(NodeId::new(1)), Some(&expected));
}
