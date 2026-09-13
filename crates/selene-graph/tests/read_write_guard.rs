//! Guard the sparse/many-label benchmark's observable correctness contract.

#[path = "../benches/read_write_guard/fixture.rs"]
mod fixture;

use selene_core::{LabelDiff, NodeId, PropertyDiff, Value, db_string};
use selene_graph::SharedGraph;

#[test]
fn sparse_many_label_reads_candidates_and_snapshot_mutations() {
    let graph = fixture::sparse(4_096);
    let ids = graph.live_node_candidates().unwrap();
    assert_eq!(ids.len(), 3_276);
    assert_eq!(graph.label_count(), fixture::LABELS);
    assert!(!ids.contains(NodeId::new(fixture::FIRST_ID)));
    let live = NodeId::new(fixture::FIRST_ID + 1);
    let key = db_string("value").unwrap();
    let label = fixture::label(1);
    assert_eq!(
        graph.node_property_eq_cardinality(&label, &key, &Value::Int(1)),
        Some(1)
    );
    let empty = graph.bind_node_candidates([]).unwrap();
    for _ in 0..8 {
        assert_eq!(
            graph
                .difference_candidates(&ids, &empty)
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            ids.iter().collect::<Vec<_>>()
        );
    }
    let shared = SharedGraph::from_graph(graph.clone());
    let old = shared.read();
    let old_candidates = old.live_node_candidates().unwrap();
    let mut tx = shared.begin_write();
    tx.mutator()
        .update_node(
            live,
            LabelDiff::new([], []).unwrap(),
            PropertyDiff::new([(key.clone(), Value::Int(-1))], []).unwrap(),
        )
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        old.node_properties(live).unwrap().get(&key),
        Some(&Value::Int(1))
    );
    assert_eq!(
        shared.read().node_properties(live).unwrap().get(&key),
        Some(&Value::Int(-1))
    );
    assert!(
        shared
            .read()
            .difference_candidates(&old_candidates, &old_candidates)
            .is_err()
    );
    let mut tx = shared.begin_write();
    tx.mutator().delete_node(live).unwrap();
    tx.commit().unwrap();
    assert!(shared.read().node_properties(live).is_none());
    assert!(old.node_properties(live).is_some());
    let survivors = shared
        .read()
        .live_node_candidates()
        .unwrap()
        .iter()
        .collect::<Vec<_>>();
    shared.compact().unwrap();
    assert_eq!(
        shared
            .read()
            .live_node_candidates()
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        survivors
    );
    shared.read().assert_indexes_consistent().unwrap();
}
