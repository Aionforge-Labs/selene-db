use super::{dbs, graph_type, rt};
use crate::*;

#[test]
fn change_postcard_round_trip() {
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
        Change::SchemaChanged {
            graph: GraphId::new(1),
            change: SchemaChange::GraphTypeCreated {
                graph_type: graph_type(),
            },
        },
        Change::NodesOfTypeTruncated {
            label: label.clone(),
        },
        Change::EdgesOfTypeTruncated { label },
        Change::GraphReset {},
    ];
    for change in changes {
        rt(&change);
    }
}

#[test]
fn graph_reset_postcard_round_trip() {
    // BRIEF-152: GraphReset is the empty (carries-nothing) factory-reset change.
    // It is the last appended variant, so its postcard tag is 12 (the removal of
    // the former IndexExtensionEvent variant at tag 7 re-tagged every later
    // variant down by one - a greenfield postcard break with no production WAL).
    // A unit-style variant encodes to its single tag byte with no payload.
    let change = Change::GraphReset {};
    let bytes = postcard::to_allocvec(&change).unwrap();
    assert_eq!(
        bytes,
        [12_u8],
        "GraphReset encodes to its bare tag byte (12)"
    );
    let decoded: Change = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, Change::GraphReset {});
}

#[test]
fn pre_147_node_updated_wire_blob_still_decodes() {
    let bytes = [1_u8, 1, 0, 0, 0, 0];
    let decoded: Change = postcard::from_bytes(&bytes).unwrap();

    assert_eq!(
        decoded,
        Change::NodeUpdated {
            id: NodeId::new(1),
            labels_diff: LabelDiff::new([], []).unwrap(),
            properties_diff: PropertyDiff::new([], []).unwrap(),
        }
    );
}
