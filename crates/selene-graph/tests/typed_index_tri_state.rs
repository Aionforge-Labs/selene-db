//! Regression tests for property-index equality lookup tri-state semantics.

use selene_core::{GraphId, LabelSet, PropertyMap, Value, db_string};
use selene_graph::{SharedGraph, TypedIndexKind};

#[test]
fn typed_index_tri_state_pin() {
    let shared = graph_with_age_index();
    let person = db_string("tri.person").unwrap();
    let age = db_string("tri.age").unwrap();
    let absent = db_string("tri.absent").unwrap();

    let snapshot = shared.read();
    let hit = snapshot
        .node_candidates_with_property_eq(&person, &age, &Value::Int(30))
        .unwrap()
        .expect("registered index hit returns candidates");
    assert_eq!(hit.len(), 1);

    let missing_key = snapshot
        .node_candidates_with_property_eq(&person, &age, &Value::Int(999))
        .unwrap()
        .expect("registered index miss returns Some(empty), not None");
    assert!(missing_key.is_empty());

    assert!(
        snapshot
            .node_candidates_with_property_eq(&person, &absent, &Value::Int(999))
            .unwrap()
            .is_none(),
        "unregistered index remains None so callers can fall back to scan"
    );
}

fn graph_with_age_index() -> SharedGraph {
    let shared = SharedGraph::new(GraphId::new(88));
    let person = db_string("tri.person").unwrap();
    let age = db_string("tri.age").unwrap();
    {
        let mut txn = shared.begin_write();
        txn.mutator()
            .create_node(
                LabelSet::single(person.clone()),
                PropertyMap::from_pairs([(age.clone(), Value::Int(30))]).unwrap(),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    shared
        .create_property_index(person, age, TypedIndexKind::I64)
        .unwrap();
    shared
}
