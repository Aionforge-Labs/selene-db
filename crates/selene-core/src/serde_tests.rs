use std::fmt::Debug;

use serde::{Deserialize, Serialize};
use smallvec::smallvec;

use crate::*;

mod changes;
mod schema;
mod values;

fn rt<T>(value: &T)
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + Debug,
{
    let bytes = postcard::to_allocvec(value)
        .unwrap_or_else(|error| panic!("postcard encode failed for {value:?}: {error:?}"));
    let decoded: T = postcard::from_bytes(&bytes)
        .unwrap_or_else(|error| panic!("postcard decode failed for {value:?}: {error:?}"));
    assert_eq!(&decoded, value);
}

pub(crate) fn encode_changes(changes: Vec<Change>) -> logical::CodecResult<Vec<u8>> {
    let delta = logical::GraphDelta {
        id: GraphId::new(1),
        previous: None,
        generation: 1,
        next_node_id: 100,
        next_edge_id: 100,
        definition: None,
        backing_indexes: vec![],
        changes,
    };
    let mut encoder = logical::Encoder::new(Default::default())?;
    delta.encode(&mut encoder)?;
    Ok(encoder.finish())
}
pub(crate) fn decode_changes(bytes: &[u8]) -> logical::CodecResult<Vec<Change>> {
    let mut budget = logical::Budget::new(Default::default())?;
    let mut decoder = logical::Decoder::new(bytes, &mut budget)?;
    let delta = logical::GraphDelta::decode(&mut decoder)?;
    decoder.finish()?;
    Ok(delta.changes)
}
pub(crate) fn encode_map(map: &PropertyMap) -> logical::CodecResult<Vec<u8>> {
    encode_changes(vec![Change::NodeCreated {
        id: NodeId::new(1),
        labels: LabelSet::new(),
        properties: map.clone(),
    }])
}
pub(crate) fn decode_map(bytes: &[u8]) -> logical::CodecResult<PropertyMap> {
    match decode_changes(bytes)?.remove(0) {
        Change::NodeCreated { properties, .. } => Ok(properties),
        _ => Err(logical::CodecError::Semantic),
    }
}
fn rt_value(value: &Value) {
    let stored = StoredValue::try_from(value.clone()).unwrap();
    let bytes = logical::encode_value(&stored, Default::default()).unwrap();
    assert_eq!(
        logical::decode_value(&bytes, Default::default())
            .unwrap()
            .as_value(),
        value
    );
}
fn rt_property(value: &PropertyDef) {
    let definition = logical::GraphDefinition {
        name: dbs("type"),
        nodes: vec![(
            dbs("N"),
            NodeTypeDef {
                labels: LabelSet::single(dbs("N")),
                properties: smallvec![value.clone()],
                key: None,
                validation_mode: ValidationMode::Strict,
            },
        )],
        edges: vec![],
    };
    let mut encoder = logical::Encoder::new(Default::default()).unwrap();
    encoder.graph_definition(&definition).unwrap();
    let bytes = encoder.finish();
    let mut budget = logical::Budget::new(Default::default()).unwrap();
    let mut decoder = logical::Decoder::new(&bytes, &mut budget).unwrap();
    assert_eq!(decoder.graph_definition().unwrap(), definition);
    decoder.finish().unwrap();
}

fn dbs(value: &str) -> DbString {
    crate::db_string(value).unwrap()
}

fn property_def(name: &str) -> PropertyDef {
    PropertyDef {
        name: dbs(name),
        value_type: ValueType::predefined(PredefinedValueType::String),
        nullable: false,
        default: None,
        immutable: false,
        unique: false,
        record_fields: None,
    }
}
