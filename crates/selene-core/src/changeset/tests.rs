use smallvec::smallvec;

use super::*;
use crate::{CoreError, GraphTypeId, Value};

fn dbs(name: &str) -> DbString {
    crate::db_string(name).unwrap()
}

#[test]
fn node_created_round_trip() {
    let change = Change::NodeCreated {
        id: NodeId::new(1),
        labels: LabelSet::single(dbs("change.node")),
        properties: PropertyMap::from_pairs([(dbs("change.p"), Value::Int(1))]).unwrap(),
    };
    assert_eq!(change.clone(), change);
}

#[test]
fn node_updated_with_label_diff_and_property_diff() {
    let change = Change::NodeUpdated {
        id: NodeId::new(1),
        labels_diff: LabelDiff::new([dbs("change.add")], [dbs("change.remove")]).unwrap(),
        properties_diff: PropertyDiff::new([(dbs("change.set"), Value::Bool(true))], []).unwrap(),
    };
    assert_eq!(change.clone(), change);
}

#[test]
fn edge_lifecycle_create_update_delete() {
    let create = Change::EdgeCreated {
        directionality: crate::EdgeDirectionality::Directed,
        id: EdgeId::new(1),
        label: dbs("change.edge"),
        source: NodeId::new(1),
        target: NodeId::new(2),
        properties: PropertyMap::new(),
    };
    let update = Change::EdgeUpdated {
        id: EdgeId::new(1),
        properties_diff: PropertyDiff::new([], [dbs("change.removed")]).unwrap(),
    };
    let delete = Change::EdgeDeleted { id: EdgeId::new(1) };
    assert_ne!(create, update);
    assert_ne!(update, delete);
}

#[test]
fn schema_changed_carries_graph_id_and_change_kind() {
    let graph_type = GraphTypeId::new(1).unwrap();
    let change = Change::SchemaChanged {
        graph: GraphId::new(1),
        change: SchemaChange::GraphCreated {
            id: GraphId::new(2),
            name: dbs("change.graph"),
            graph_type: Some(graph_type),
        },
    };
    match change {
        Change::SchemaChanged { graph, .. } => assert_eq!(graph, GraphId::new(1)),
        _ => panic!("expected schema change"),
    }
}

#[test]
fn change_all_covers_every_variant() {
    assert_eq!(Change::VARIANT_COUNT, 13);
    let mut discriminants = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for factory in Change::ALL {
        let change = factory();
        assert!(
            discriminants.insert(std::mem::discriminant(&change)),
            "Change::ALL has duplicate variant: {}",
            change.variant_name()
        );
        let name = change.variant_name();
        assert!(!name.is_empty(), "Change::variant_name must not be empty");
        assert!(names.insert(name), "Change::variant_name collision: {name}");
    }
    assert_eq!(discriminants.len(), Change::ALL.len());
    assert_eq!(names.len(), Change::ALL.len());
}

#[test]
fn label_diff_added_and_removed_independent() {
    let added = dbs("change.label.added");
    let removed = dbs("change.label.removed");
    let diff = LabelDiff::new([added.clone()], [removed.clone()]).unwrap();
    assert_eq!(diff.added.as_slice(), &[added]);
    assert_eq!(diff.removed.as_slice(), &[removed]);
}

#[test]
fn property_diff_accepts_singleton_set_input() {
    let property = dbs("change.property.single.set");
    let single_property = PropertyDiff::new([(property.clone(), Value::Int(7))], []).unwrap();
    assert_eq!(single_property.set.as_slice(), &[(property, Value::Int(7))]);
    assert!(single_property.removed.is_empty());
}

#[test]
fn property_diff_accepts_canonical_set_input() {
    let first = dbs("change.property.canonical.a");
    let second = dbs("change.property.canonical.b");
    let diff = PropertyDiff::new(
        [
            (first.clone(), Value::Int(1)),
            (second.clone(), Value::Int(2)),
        ],
        [],
    )
    .unwrap();

    assert_eq!(
        diff.set.as_slice(),
        &[(first, Value::Int(1)), (second, Value::Int(2))]
    );
    assert!(diff.removed.is_empty());
}

#[test]
fn property_diff_set_includes_null_value() {
    let property = dbs("change.null");
    let diff = PropertyDiff::new([(property.clone(), Value::Null)], []).unwrap();
    assert_eq!(diff.set.as_slice(), &[(property, Value::Null)]);
}

#[test]
fn label_diff_rejects_overlapping_label() {
    let label = dbs("change.overlap.label");
    let err = LabelDiff::new([label.clone()], [label]).unwrap_err();
    assert!(matches!(
        err,
        CoreError::OverlappingDiff { kind: "label", .. }
    ));
}

#[test]
fn property_diff_rejects_overlapping_key() {
    let key = dbs("change.overlap.prop");
    let err = PropertyDiff::new([(key.clone(), Value::Int(1))], [key]).unwrap_err();
    assert!(matches!(
        err,
        CoreError::OverlappingDiff {
            kind: "property",
            ..
        }
    ));
}

#[test]
fn diffs_report_lowest_sorted_overlap() {
    let beta = dbs("change.overlap.beta");
    let delta = dbs("change.overlap.delta");
    let gamma = dbs("change.overlap.gamma");

    let label_err = LabelDiff::new(
        [gamma.clone(), beta.clone()],
        [delta.clone(), beta.clone(), gamma.clone()],
    )
    .unwrap_err();
    assert!(matches!(
        label_err,
        CoreError::OverlappingDiff { key, .. } if key == beta
    ));

    let property_err = PropertyDiff::new(
        [
            (gamma.clone(), Value::Int(3)),
            (beta.clone(), Value::Int(2)),
        ],
        [delta, beta.clone(), gamma],
    )
    .unwrap_err();
    assert!(matches!(
        property_err,
        CoreError::OverlappingDiff { key, .. } if key == beta
    ));
}

#[test]
fn label_diff_deserialize_round_trip() {
    let added = dbs("change.deser.add");
    let removed = dbs("change.deser.remove");
    let diff = LabelDiff::new([added], [removed]).unwrap();
    let bytes = postcard::to_allocvec(&diff).unwrap();
    let round: LabelDiff = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(round, diff);
}

#[test]
fn label_diff_serialize_independent_of_construction_order() {
    // Wire-invariance proof: two diffs built from different input orders of
    // the same labels serialize to byte-identical (canonical) wire.
    let a = dbs("change.wire.alpha");
    let b = dbs("change.wire.beta");
    let c = dbs("change.wire.gamma");
    let forward = LabelDiff::new([c.clone(), a.clone(), b.clone()], []).unwrap();
    let reverse = LabelDiff::new([b, a, c], []).unwrap();
    assert_eq!(
        postcard::to_allocvec(&forward).unwrap(),
        postcard::to_allocvec(&reverse).unwrap(),
    );
}

#[test]
fn label_diff_serialize_canonicalizes_public_field_construction() {
    // `LabelDiff.added`/`removed` are PUBLIC fields, so a caller can build a
    // non-canonical diff without `LabelDiff::new`. Serialize canonicalizes
    // it so the wire round-trips through the strict (validate-no-resort)
    // decoder rather than being rejected as malformed.
    let zebra = dbs("change.noncanon.label.zebra");
    let apple = dbs("change.noncanon.label.apple");
    let non_canonical = LabelDiff {
        added: smallvec![zebra.clone(), apple.clone()],
        removed: SmallVec::new(),
    };
    let bytes = postcard::to_allocvec(&non_canonical).unwrap();
    let round: LabelDiff = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(
        round.added,
        SmallVec::<[DbString; 2]>::from_vec(vec![apple, zebra])
    );
}

#[test]
fn property_diff_format2_rejects_noncanonical_public_fields() {
    // `PropertyDiff.set`/`removed` are PUBLIC fields; serialize canonicalizes
    // a non-canonical diff so it round-trips through the strict decoder.
    let zebra = dbs("change.noncanon.prop.zebra");
    let apple = dbs("change.noncanon.prop.apple");
    let non_canonical = PropertyDiff {
        set: smallvec![
            (zebra.clone(), Value::Int(2)),
            (apple.clone(), Value::Int(1))
        ],
        removed: SmallVec::new(),
    };
    assert!(encode_property_diff(&non_canonical).is_err());
}

#[test]
fn label_diff_deserialize_round_trips_canonical_payload() {
    // A canonical (ascending) wire payload deserializes preserving order.
    // `DbString` Ord is lexicographic, so "apple" sorts before "zebra".
    let zebra = dbs("change.deser.label.zebra");
    let apple = dbs("change.deser.label.apple");
    let good = LabelDiffWireSer {
        added: smallvec![apple.clone(), zebra.clone()],
        removed: SmallVec::new(),
    };
    let bytes = postcard::to_allocvec(&good).unwrap();
    let round: LabelDiff = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(
        round.added,
        SmallVec::<[DbString; 2]>::from_vec(vec![apple, zebra])
    );
}

#[test]
fn label_diff_deserialize_rejects_non_canonical_payload() {
    // A non-ascending wire payload is rejected as malformed (the decoder
    // validates the canonical invariant, no longer resorts).
    let zebra = dbs("change.deser.label.noncanon.zebra");
    let apple = dbs("change.deser.label.noncanon.apple");
    let bad = LabelDiffWireSer {
        added: smallvec![zebra, apple],
        removed: SmallVec::new(),
    };
    let bytes = postcard::to_allocvec(&bad).unwrap();
    let result: Result<LabelDiff, _> = postcard::from_bytes(&bytes);
    assert!(result.is_err());
}

#[test]
fn label_diff_deserialize_rejects_duplicate_added() {
    let label = dbs("change.deser.label.dup");
    let bad = LabelDiffWireSer {
        added: smallvec![label.clone(), label],
        removed: SmallVec::new(),
    };
    let bytes = postcard::to_allocvec(&bad).unwrap();
    let result: Result<LabelDiff, _> = postcard::from_bytes(&bytes);
    assert!(result.is_err());
}

#[test]
fn label_diff_deserialize_rejects_overlap() {
    let label = dbs("change.deser.bad");
    let mut added = SmallVec::<[DbString; 2]>::new();
    added.push(label.clone());
    let mut removed = SmallVec::<[DbString; 2]>::new();
    removed.push(label);
    let bad = LabelDiffWireSer { added, removed };
    let bytes = postcard::to_allocvec(&bad).unwrap();
    let result: Result<LabelDiff, _> = postcard::from_bytes(&bytes);
    assert!(result.is_err());
}

#[test]
fn property_diff_format2_round_trips_canonical_payload() {
    // A canonical (ascending key) property-set wire payload deserializes
    // preserving order.
    let zebra = dbs("change.deser.prop.zebra");
    let apple = dbs("change.deser.prop.apple");
    let good = PropertyDiff {
        set: smallvec![
            (apple.clone(), Value::Int(1)),
            (zebra.clone(), Value::Int(2))
        ],
        removed: SmallVec::new(),
    };
    let bytes = encode_property_diff(&good).unwrap();
    let Change::NodeUpdated {
        properties_diff: round,
        ..
    } = crate::serde_tests::decode_changes(&bytes)
        .unwrap()
        .remove(0)
    else {
        panic!("node update")
    };
    assert_eq!(
        round.set,
        SmallVec::<[(DbString, Value); 4]>::from_vec(vec![
            (apple, Value::Int(1)),
            (zebra, Value::Int(2)),
        ])
    );
}

#[test]
fn property_diff_format2_rejects_non_canonical_payload() {
    // A non-ascending property-set key list is rejected as malformed.
    let zebra = dbs("change.deser.prop.noncanon.zebra");
    let apple = dbs("change.deser.prop.noncanon.apple");
    let bad = PropertyDiff {
        set: smallvec![(zebra, Value::Int(2)), (apple, Value::Int(1))],
        removed: SmallVec::new(),
    };
    let result = encode_property_diff(&bad);
    assert!(result.is_err());
}

#[test]
fn property_diff_format2_rejects_duplicate_set_key() {
    let key = dbs("change.deser.prop.dup");
    let bad = PropertyDiff {
        set: smallvec![(key.clone(), Value::Int(1)), (key, Value::Int(2))],
        removed: SmallVec::new(),
    };
    let result = encode_property_diff(&bad);
    assert!(result.is_err());
}

#[test]
fn property_diff_format2_rejects_overlap() {
    let key = dbs("change.deser.prop");
    let mut set = SmallVec::<[(DbString, Value); 4]>::new();
    set.push((key.clone(), Value::Int(1)));
    let mut removed = SmallVec::<[DbString; 2]>::new();
    removed.push(key);
    let bad = PropertyDiff { set, removed };
    let result = encode_property_diff(&bad);
    assert!(result.is_err());
}

#[derive(serde::Serialize)]
struct LabelDiffWireSer {
    added: SmallVec<[DbString; 2]>,
    removed: SmallVec<[DbString; 2]>,
}

fn encode_property_diff(diff: &PropertyDiff) -> crate::logical::CodecResult<Vec<u8>> {
    crate::serde_tests::encode_changes(vec![Change::NodeUpdated {
        id: NodeId::new(1),
        labels_diff: LabelDiff::new([], []).unwrap(),
        properties_diff: diff.clone(),
    }])
}

#[test]
fn empty_diffs_are_valid() {
    assert!(LabelDiff::new([], []).unwrap().is_empty());
    assert!(PropertyDiff::new([], []).unwrap().is_empty());
}

#[test]
fn schema_change_variants_construct() {
    let variants: Vec<_> = SchemaChange::ALL.iter().map(|factory| factory()).collect();
    assert_eq!(variants.len(), SchemaChange::VARIANT_COUNT);
    assert_eq!(SchemaChange::VARIANT_COUNT, 24);
}

#[test]
fn schema_change_all_covers_every_variant() {
    assert_eq!(SchemaChange::VARIANT_COUNT, 24);
    let mut discriminants = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for factory in SchemaChange::ALL {
        let change = factory();
        assert!(
            discriminants.insert(std::mem::discriminant(&change)),
            "SchemaChange::ALL has duplicate variant: {}",
            change.variant_name()
        );
        let name = change.variant_name();
        assert!(
            !name.is_empty(),
            "SchemaChange::variant_name must not be empty"
        );
        assert!(
            names.insert(name),
            "SchemaChange::variant_name collision: {name}"
        );
    }
    assert_eq!(discriminants.len(), SchemaChange::ALL.len());
    assert_eq!(names.len(), SchemaChange::ALL.len());
}
