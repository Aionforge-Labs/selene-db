use super::{dbs, decode_changes, encode_changes};
use crate::*;

#[test]
fn data_change_format2_round_trip() {
    let label = dbs("serde.change.label");
    let property = dbs("serde.change.property");
    let changes = vec![
        Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(label.clone()),
            properties: PropertyMap::from_pairs([(property.clone(), Value::Int(1))]).unwrap(),
        },
        Change::NodeUpdated {
            id: NodeId::new(1),
            labels_diff: LabelDiff::new([dbs("serde.change.add")], [dbs("serde.change.remove")])
                .unwrap(),
            properties_diff: PropertyDiff::new([(property.clone(), Value::Null)], []).unwrap(),
        },
        Change::NodeDeleted { id: NodeId::new(1) },
        Change::NodePropertyRemoved {
            id: NodeId::new(1),
            property: property.clone(),
        },
        Change::NodeLabelRemoved {
            id: NodeId::new(1),
            label: label.clone(),
        },
        Change::EdgeCreated {
            directionality: EdgeDirectionality::Directed,
            id: EdgeId::new(1),
            label: label.clone(),
            source: NodeId::new(1),
            target: NodeId::new(2),
            properties: PropertyMap::new(),
        },
        Change::EdgeUpdated {
            id: EdgeId::new(1),
            properties_diff: PropertyDiff::new([(property.clone(), Value::Bool(true))], [])
                .unwrap(),
        },
        Change::EdgeDeleted { id: EdgeId::new(1) },
        Change::EdgePropertyRemoved {
            id: EdgeId::new(1),
            property,
        },
        Change::NodesOfTypeTruncated {
            label: label.clone(),
        },
        Change::EdgesOfTypeTruncated { label },
        Change::GraphReset {},
    ];
    for change in changes {
        assert_eq!(
            decode_changes(&encode_changes(vec![change.clone()]).unwrap()).unwrap(),
            vec![change]
        );
    }
}

#[test]
fn graph_reset_format2_tag_and_round_trip() {
    let change = Change::GraphReset {};
    let mut encoder = logical::Encoder::new(Default::default()).unwrap();
    encoder.graph_change(&change).unwrap();
    let bytes = encoder.finish();
    assert_eq!(
        bytes,
        [12_u8],
        "GraphReset encodes to its bare tag byte (12)"
    );
    assert_eq!(
        decode_changes(&encode_changes(vec![change]).unwrap()).unwrap(),
        vec![Change::GraphReset {}]
    );
}

#[test]
fn retired_postcard_node_update_is_not_a_format2_graph() {
    let bytes = [1_u8, 1, 0, 0, 0, 0];
    assert!(decode_changes(&bytes).is_err());
}
