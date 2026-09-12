//! Wire-format canonical-order checks for DbString-keyed containers.
//!
//! `DbString` is an owned string newtype with lexicographic `Ord`.
//! These assertions prove two different insertion orders of the same keys
//! produce byte-identical canonical wire after `PropertyMap` canonicalizes to
//! lexicographic key order at serialize time.

use selene_core::{
    Change, GraphId, LabelSet, NodeId, PropertyMap, Value, db_string,
    logical::{Budget, Decoder, Encoder, GraphDelta},
};

fn encode(map: &PropertyMap) -> Vec<u8> {
    let delta = GraphDelta {
        id: GraphId::new(1),
        previous: None,
        generation: 1,
        next_node_id: 2,
        next_edge_id: 1,
        definition: None,
        backing_indexes: vec![],
        changes: vec![Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::new(),
            properties: map.clone(),
        }],
    };
    let mut encoder = Encoder::new(Default::default()).unwrap();
    delta.encode(&mut encoder).unwrap();
    encoder.finish()
}
fn decode(bytes: &[u8]) -> PropertyMap {
    let mut budget = Budget::new(Default::default()).unwrap();
    let mut decoder = Decoder::new(bytes, &mut budget).unwrap();
    let mut delta = GraphDelta::decode(&mut decoder).unwrap();
    decoder.finish().unwrap();
    let Change::NodeCreated { properties, .. } = delta.changes.remove(0) else {
        panic!("node creation")
    };
    properties
}

/// Two `PropertyMap`s built from different insertion orders of the same keys
/// serialize to byte-identical (canonical lexicographic) postcard wire.
#[test]
fn property_map_wire_bytes_are_independent_of_insertion_order() {
    let apple = db_string("wire.apple").unwrap();
    let banana = db_string("wire.banana").unwrap();
    let zebra = db_string("wire.zebra").unwrap();

    let first = PropertyMap::from_pairs([
        (zebra.clone(), Value::Int(3)),
        (apple.clone(), Value::Int(1)),
        (banana.clone(), Value::Int(2)),
    ])
    .expect("first map builds");
    let second = PropertyMap::from_pairs([
        (banana, Value::Int(2)),
        (zebra, Value::Int(3)),
        (apple, Value::Int(1)),
    ])
    .expect("second map builds");

    let first_bytes = encode(&first);
    let second_bytes = encode(&second);
    assert_eq!(
        first_bytes, second_bytes,
        "canonical wire order must not depend on PropertyMap insertion order"
    );

    // And both round-trip back to the same map.
    let round = decode(&first_bytes);
    assert_eq!(round, first);
    assert_eq!(round, second);
}

/// The same canonicalization holds for a Compact-shaped map: a fixed key set
/// supplied in two different orders serializes identically.
#[test]
fn compact_property_map_wire_bytes_are_independent_of_key_order() {
    let alpha = db_string("wire.compact.alpha").unwrap();
    let mid = db_string("wire.compact.mid").unwrap();
    let omega = db_string("wire.compact.omega").unwrap();

    let forward = PropertyMap::compact(
        [alpha.clone(), mid.clone(), omega.clone()],
        [
            Some(Value::Int(1)),
            Some(Value::Int(2)),
            Some(Value::Int(3)),
        ],
    )
    .expect("forward compact builds");
    let reverse = PropertyMap::compact(
        [omega, mid, alpha],
        [
            Some(Value::Int(3)),
            Some(Value::Int(2)),
            Some(Value::Int(1)),
        ],
    )
    .expect("reverse compact builds");

    assert_eq!(
        encode(&forward),
        encode(&reverse),
        "compact canonical wire must not depend on supplied key order"
    );
}
