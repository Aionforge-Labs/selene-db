//! Format-2 semantic replay/snapshot property ports. These exercise real codecs
//! and graph apply, not filesystem durability; facade phase/crash tests own I/O.
mod format2_support;
mod funnel_harness;
use funnel_harness::{Oracle, apply_op, arb_op, assert_snapshot_matches_oracle};
use proptest::prelude::*;
use selene_core::{Change, DbString, GraphId, LabelSet, PropertyMap, Record, Value, db_string};
use selene_graph::{IndexProvider, ProviderError, ProviderTag, SharedGraph};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recorder(Mutex<Vec<Change>>);
impl IndexProvider for Recorder {
    fn provider_tag(&self) -> ProviderTag {
        ProviderTag(*b"TEST")
    }
    fn on_change(&self, change: &Change) -> Result<(), ProviderError> {
        self.0.lock().unwrap().push(change.clone());
        Ok(())
    }
}
fn memory_graph(id: GraphId, recorder: &Arc<Recorder>) -> SharedGraph {
    SharedGraph::builder(id)
        .with_provider(recorder.clone())
        .build()
        .unwrap()
}
proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn format2_replay_and_snapshot_round_trip_arbitrary_op_sequence(
        first in proptest::collection::vec(arb_op(), 1..=40),
        second in proptest::collection::vec(arb_op(), 0..=20),
    ) {
        let id = GraphId::new(909);
        let recorder = Arc::new(Recorder::default());
        let base = memory_graph(id, &recorder);
        let mut oracle = Oracle::default();
        for (i, op) in first.iter().enumerate() { apply_op(&base, &mut oracle, op, i); }
        let image = format2_support::snapshot(&base.read()).unwrap();
        assert_snapshot_matches_oracle(&image.read(), &oracle);
        prop_assert_eq!(image.read().meta.next_node_id, base.read().meta.next_node_id);
        prop_assert_eq!(image.read().meta.next_edge_id, base.read().meta.next_edge_id);
        let live = SharedGraph::from_graph_with_providers(image.read().as_ref().clone(), vec![recorder.clone()]).unwrap();
        for (i, op) in second.iter().enumerate() { apply_op(&live, &mut oracle, op, first.len() + i); }
        let replay = format2_support::replay(id, recorder.0.lock().unwrap().clone()).unwrap();
        assert_snapshot_matches_oracle(&replay.read(), &oracle);
        replay.read().assert_indexes_consistent().unwrap();
        let image = format2_support::snapshot(&live.read()).unwrap();
        assert_snapshot_matches_oracle(&image.read(), &oracle);
        image.read().assert_indexes_consistent().unwrap();
    }
}
fn payload_key() -> DbString {
    db_string("recover.heavy.payload").unwrap()
}
fn indexed_key() -> DbString {
    db_string("recover.heavy.age").unwrap()
}
fn heavy_label() -> DbString {
    db_string("recover.heavy.node").unwrap()
}
fn heavy_edge_label() -> DbString {
    db_string("recover.heavy.edge").unwrap()
}
fn heavy_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int),
        any::<u64>().prop_map(Value::Uint),
        any::<i128>().prop_map(Value::Int128),
        any::<u128>().prop_map(Value::Uint128),
        any::<f64>().prop_map(Value::Float),
        any::<f32>().prop_map(Value::Float32),
        any::<i64>().prop_map(|m| Value::Decimal(rust_decimal::Decimal::new(m, 2))),
        "[a-zA-Z0-9 ]{0,16}".prop_map(|s| Value::String(db_string(&s).unwrap())),
        proptest::collection::vec(any::<u8>(), 0..20).prop_map(|b| Value::Bytes(b.into())),
        Just(Value::Date("2024-06-15".parse().unwrap())),
        Just(Value::LocalDateTime("2024-06-15T12:30:00".parse().unwrap())),
        Just(Value::LocalTime("12:30:00".parse().unwrap())),
        Just(Value::Duration(Box::new("PT3H15M".parse().unwrap()))),
        any::<u128>().prop_map(|n| Value::Uuid(uuid::Uuid::from_u128(n))),
        Just(Value::Null),
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..3).prop_map(Value::List),
            proptest::collection::btree_map("[a-z]{1,5}", inner, 0..2).prop_map(|fields| {
                Value::Record(Box::new(Record::Open(
                    fields
                        .into_iter()
                        .map(|(k, v)| (db_string(&k).unwrap(), v))
                        .collect(),
                )))
            }),
        ]
    })
}
fn heavy_props() -> impl Strategy<Value = PropertyMap> {
    (0i64..50, heavy_value()).prop_map(|(age, payload)| {
        PropertyMap::from_pairs([(indexed_key(), Value::Int(age)), (payload_key(), payload)])
            .unwrap()
    })
}
proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    #[test]
    fn format2_replays_heavy_properties_and_mixed_edges(
        node_props in proptest::collection::vec(heavy_props(), 1..=16),
        edge_props in proptest::collection::vec(heavy_props(), 0..=8),
    ) {
        let id = GraphId::new(911);
        let recorder = Arc::new(Recorder::default());
        let shared = memory_graph(id, &recorder);
        let mut expected_nodes = Vec::new();
        for props in &node_props {
            let mut txn = shared.begin_write();
            let node = txn.mutator().create_node(LabelSet::single(heavy_label()), props.clone()).unwrap();
            txn.commit().unwrap();
            expected_nodes.push(node);
        }
        let mut expected_edges = Vec::new();
        for (i, props) in edge_props.iter().enumerate() {
            let mut txn = shared.begin_write();
            let edge = txn.mutator().create_mixed_edge(heavy_edge_label(), expected_nodes[i % expected_nodes.len()],
                expected_nodes[(i + 1) % expected_nodes.len()],
                if i % 2 == 0 { selene_core::EdgeDirectionality::Directed } else { selene_core::EdgeDirectionality::Undirected }, props.clone()).unwrap();
            txn.commit().unwrap();
            expected_edges.push(edge);
        }
        let replay = format2_support::replay(id, recorder.0.lock().unwrap().clone()).unwrap();
        let image = format2_support::snapshot(&shared.read()).unwrap();
        for graph in [&replay, &image] {
            let view = graph.read();
            for (id, props) in expected_nodes.iter().zip(&node_props) { prop_assert_eq!(view.node_properties(*id), Some(props)); }
            for (id, props) in expected_edges.iter().zip(&edge_props) {
                prop_assert_eq!(view.edge_properties(*id), Some(props));
                prop_assert_eq!(view.edge_record(*id), shared.read().edge_record(*id));
            }
            // Catalog-backed index reconstruction is covered by the joined
            // logical_transaction runtime and facade all-index fixtures.
            graph.read().assert_indexes_consistent().unwrap();
        }
    }
}
