//! Independent archive construction exercises the production decode boundary.

use crate::graph_types::{
    GraphTypeDef, NodeTypeDef, PropertyDefaultRecordField, PropertyDefaultValue as D,
    PropertyTypeDef, ValidationMode,
};
use selene_core::{LabelSet, PropertyValueType, db_string};

fn default(depth: usize, record: bool) -> D {
    let mut value = D::Null;
    for _ in 1..depth {
        value = if record {
            D::Record(vec![PropertyDefaultRecordField {
                name: db_string("x").unwrap(),
                value: Box::new(value),
            }])
        } else {
            D::List(vec![Box::new(value)])
        };
    }
    value
}

fn schema(default: D, record: bool) -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("g").unwrap(),
        edge_types: vec![],
        node_types: vec![NodeTypeDef {
            name: db_string("n").unwrap(),
            key_labels: LabelSet::single(db_string("N").unwrap()),
            validation_mode: ValidationMode::Strict,
            properties: vec![PropertyTypeDef {
                name: db_string("p").unwrap(),
                value_type: if record {
                    PropertyValueType::RecordTyped
                } else {
                    PropertyValueType::List
                },
                list_element_type: None,
                required: false,
                default: Some(default),
                immutable: false,
                unique: false,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                record_field_types: None,
            }],
        }],
    }
}

#[test]
fn snapshot_default_maximum_is_admitted_and_maximum_plus_one_rejected() {
    for record in [false, true] {
        for depth in [
            selene_core::MAX_STORED_VALUE_DEPTH,
            selene_core::MAX_STORED_VALUE_DEPTH + 1,
        ] {
            // Bypass production admission and section serialization to construct
            // an independently archived legacy descriptor in the current format.
            let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&vec![(
                0_u32,
                schema(default(depth, record), record),
            )])
            .unwrap();
            let payload = [vec![2], bytes.to_vec()].concat();
            let decoded = super::super::gtyp::decode_graph_types(&payload);
            assert_eq!(
                decoded.is_ok(),
                depth == selene_core::MAX_STORED_VALUE_DEPTH,
                "record={record}, depth={depth}: {decoded:?}"
            );
        }
    }
}

#[test]
fn archive_bytecheck_rejects_deep_subtrees_before_deserialization() {
    // Fixture encoding owns a larger stack; the actual production decoder is
    // exercised separately with a small stack and must return an error.
    let bytes = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&default(4096, false)).unwrap();
            rkyv::access::<rkyv::Archived<D>, rkyv::rancor::Error>(&bytes)
                .expect("fixture is a valid archive without a depth limit");
            bytes.to_vec()
        })
        .unwrap()
        .join()
        .unwrap();
    std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let error = super::decode_rkyv::<D>(&bytes, "CORE/GTYP").unwrap_err();
            assert!(
                matches!(error, crate::ProviderError::InvalidPayload { .. }),
                "{error}"
            );
        })
        .unwrap()
        .join()
        .unwrap();
}
