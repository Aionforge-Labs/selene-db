use super::codec::{decode_rkyv, encode_rkyv, ensure_section_within_cap, validate_sorted_unique};
use crate::graph::SeleneGraph;
use crate::graph_types::GraphTypeDef;

/// CORE/GTYP section format version.
///
/// Single version per the 2026-05-30 clean-break directive (greenfield, no consumers): the
/// legacy multi-magic V1/V2/V3 decoder is removed and the on-disk layout IS the live
/// [`GraphTypeDef`] rkyv archive.
///
/// Bumped `1 -> 2` by the typed-descriptor stream: the archived `PropertyTypeDef`
/// gained mid-struct `decimal_type` / `character_string_type` / `byte_string_type`
/// fields, and `PropertyElementType` / `RecordFieldType` gained descriptor
/// variants ahead of existing ones, so a version-1 archive must be rejected
/// rather than bytechecked against the wrong shape.
// Why: D14 (rkyv snapshot archive) / D19 (GG02 catalog persistence). The GTYP version byte
// is internal to the `b"GTYP"` section payload and is independent of the SLSN container's
// `SNAPSHOT_VERSION_MINOR` (which bumped 3 -> 4 for the same descriptor break).
const GTYP_VERSION: u8 = 2;

pub(in crate::core_provider) fn encode_graph_types(
    graph: &SeleneGraph,
) -> Result<Vec<u8>, crate::ProviderError> {
    let rows = graph
        .meta
        .bound_type
        .as_ref()
        .map(|type_def| vec![(0_u32, (**type_def).clone())])
        .unwrap_or_default();
    let mut payload = Vec::with_capacity(1);
    payload.push(GTYP_VERSION);
    payload.extend(encode_rkyv(&rows, "CORE/GTYP")?);
    ensure_section_within_cap("CORE/GTYP", payload.len())?;
    Ok(payload)
}

pub(in crate::core_provider) fn decode_graph_types(
    bytes: &[u8],
) -> Result<Vec<(u32, GraphTypeDef)>, crate::ProviderError> {
    // Single-version clean break (USER DIRECTIVE 2026-05-30): the on-disk layout IS the
    // live `GraphTypeDef` archive; there are no legacy V1/V2/V3 decoders. A mismatched or
    // absent version byte is a hard decode error, never a silent legacy fall-through.
    let Some((&version, rest)) = bytes.split_first() else {
        return Err(crate::ProviderError::InvalidPayload {
            reason: "CORE/GTYP section is empty".to_owned(),
        });
    };
    if version != GTYP_VERSION {
        return Err(crate::ProviderError::InvalidPayload {
            reason: format!(
                "CORE/GTYP section version {version} is unsupported (expected {GTYP_VERSION})"
            ),
        });
    }
    let rows: Vec<(u32, GraphTypeDef)> = decode_rkyv(rest, "CORE/GTYP")?;
    validate_sorted_unique(&rows, "CORE/GTYP")?;
    for (_, descriptor) in &rows {
        descriptor.validate_ref().map_err(|error| {
            crate::core_provider::invalid_payload(format!("CORE/GTYP invalid graph type: {error}"))
        })?;
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use selene_core::{
        ByteStringType, CharacterStringType, DecimalType, GraphId, LabelSet, PropertyValueType,
        db_string,
    };

    use super::*;
    use crate::SharedGraph;
    use crate::graph_types::{
        EdgeEndpointDef, EdgeTypeDef, NodeTypeDef, PropertyTypeDef, RecordFieldType,
        RecordFieldTypeDef, RecordFieldTypes, ValidationMode,
    };

    #[test]
    fn encode_graph_types_writes_version_byte() {
        let person = db_string("VersionPerson").unwrap();
        let graph_type = GraphTypeDef {
            name: db_string("version.graph").unwrap(),
            node_types: vec![NodeTypeDef {
                name: person.clone(),
                key_labels: LabelSet::single(person),
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: Vec::new(),
        };
        let graph = SharedGraph::builder(GraphId::new(211))
            .bound_to(graph_type)
            .unwrap()
            .build()
            .unwrap()
            .read()
            .as_ref()
            .clone();

        let bytes = encode_graph_types(&graph).unwrap();

        assert_eq!(bytes.first(), Some(&GTYP_VERSION));
    }

    #[test]
    fn gtyp_round_trips_oneof_endpoint() {
        // BRIEF-131e OneOf shape under the single-version (post-collapse) layout: encode +
        // decode a GraphTypeDef with an OneOf edge endpoint and assert structural equality
        // including OneOf payload sort order, so a future variant-reorder regression
        // surfaces here rather than as silent on-disk corruption.
        let person = db_string("V3OneOfPerson").unwrap();
        let company = db_string("V3OneOfCompany").unwrap();
        let school = db_string("V3OneOfSchool").unwrap();
        let affiliated = db_string("V3_AFFILIATED").unwrap();
        let graph_type = GraphTypeDef {
            name: db_string("v3.oneof.graph").unwrap(),
            node_types: vec![
                NodeTypeDef {
                    name: person.clone(),
                    key_labels: LabelSet::single(person),
                    properties: Vec::new(),
                    validation_mode: ValidationMode::Strict,
                },
                NodeTypeDef {
                    name: company.clone(),
                    key_labels: LabelSet::single(company),
                    properties: Vec::new(),
                    validation_mode: ValidationMode::Strict,
                },
                NodeTypeDef {
                    name: school.clone(),
                    key_labels: LabelSet::single(school),
                    properties: Vec::new(),
                    validation_mode: ValidationMode::Strict,
                },
            ],
            edge_types: vec![EdgeTypeDef {
                name: affiliated.clone(),
                label: affiliated,
                source_node_type: EdgeEndpointDef::NodeType(0),
                target_node_type: EdgeEndpointDef::one_of([1, 2]),
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
        };
        let graph = SharedGraph::builder(GraphId::new(212))
            .bound_to(graph_type.clone())
            .unwrap()
            .build()
            .unwrap()
            .read()
            .as_ref()
            .clone();

        let bytes = encode_graph_types(&graph).unwrap();
        assert_eq!(bytes.first(), Some(&GTYP_VERSION));

        let decoded = decode_graph_types(&bytes).unwrap();
        assert_eq!(decoded.len(), 1);
        let decoded_graph_type = &decoded[0].1;
        decoded_graph_type.validate_ref().unwrap();
        let edge_type = &decoded_graph_type.edge_types[0];
        assert_eq!(
            edge_type.target_node_type,
            EdgeEndpointDef::OneOf(vec![1, 2])
        );
        assert_eq!(edge_type.source_node_type, EdgeEndpointDef::NodeType(0));
    }

    #[test]
    fn gtyp_round_trips_record_field_types() {
        // A closed/typed RECORD property descriptor must survive the GTYP rkyv archive,
        // including nested LIST/RECORD/NOT NULL field types (exercises the
        // bytecheck recursion bounds on RecordFieldType across the archive).
        let person = db_string("RecordPerson").unwrap();
        let config = db_string("config").unwrap();
        let host = db_string("host").unwrap();
        let ports = db_string("ports").unwrap();
        let nested = db_string("nested").unwrap();
        let open_nested = db_string("open_nested").unwrap();
        let flag = db_string("flag").unwrap();
        let amount = db_string("amount").unwrap();
        let title = db_string("title").unwrap();
        let digest = db_string("digest").unwrap();
        let code = db_string("code").unwrap();
        let history = db_string("history").unwrap();
        let payloads = db_string("payloads").unwrap();
        let aliases = db_string("aliases").unwrap();
        let decimal = DecimalType::new(5, 2).unwrap();
        let history_decimal = DecimalType::new(4, 1).unwrap();
        let nested_decimal = DecimalType::new(6, 3).unwrap();
        let character_string = CharacterStringType::new(2, 4).unwrap();
        let history_character_string = CharacterStringType::new(1, 2).unwrap();
        let nested_character_string = CharacterStringType::new(4, 4).unwrap();
        let byte_string = ByteStringType::new(2, 4).unwrap();
        let history_byte_string = ByteStringType::new(1, 2).unwrap();
        let nested_byte_string = ByteStringType::new(4, 4).unwrap();
        let record_field_types = RecordFieldTypes(vec![
            RecordFieldTypeDef {
                name: host,
                field_type: RecordFieldType::Scalar(PropertyValueType::String),
                required: true,
            },
            RecordFieldTypeDef {
                name: amount.clone(),
                field_type: RecordFieldType::Decimal(nested_decimal),
                required: false,
            },
            RecordFieldTypeDef {
                name: code.clone(),
                field_type: RecordFieldType::CharacterString(nested_character_string),
                required: false,
            },
            RecordFieldTypeDef {
                name: digest.clone(),
                field_type: RecordFieldType::ByteString(nested_byte_string),
                required: false,
            },
            RecordFieldTypeDef {
                name: ports,
                field_type: RecordFieldType::List(Box::new(RecordFieldType::NotNull(Box::new(
                    RecordFieldType::Scalar(PropertyValueType::Int),
                )))),
                required: false,
            },
            RecordFieldTypeDef {
                name: open_nested,
                field_type: RecordFieldType::OpenRecord,
                required: false,
            },
            RecordFieldTypeDef {
                name: nested,
                field_type: RecordFieldType::Record(Box::new(RecordFieldTypes(vec![
                    RecordFieldTypeDef {
                        name: flag,
                        field_type: RecordFieldType::Scalar(PropertyValueType::Bool),
                        required: true,
                    },
                ]))),
                required: false,
            },
        ]);
        let graph_type = GraphTypeDef {
            name: db_string("record.graph").unwrap(),
            node_types: vec![NodeTypeDef {
                name: person.clone(),
                key_labels: LabelSet::single(person),
                properties: vec![
                    PropertyTypeDef {
                        name: config,
                        value_type: PropertyValueType::RecordTyped,
                        list_element_type: None,
                        required: false,
                        default: None,
                        immutable: false,
                        unique: false,
                        decimal_type: None,
                        character_string_type: None,
                        byte_string_type: None,
                        record_field_types: Some(record_field_types.clone()),
                    },
                    PropertyTypeDef {
                        name: amount.clone(),
                        value_type: PropertyValueType::Decimal,
                        list_element_type: None,
                        required: false,
                        default: None,
                        immutable: false,
                        unique: false,
                        decimal_type: Some(decimal),
                        character_string_type: None,
                        byte_string_type: None,
                        record_field_types: None,
                    },
                    PropertyTypeDef {
                        name: title,
                        value_type: PropertyValueType::String,
                        list_element_type: None,
                        required: false,
                        default: None,
                        immutable: false,
                        unique: false,
                        decimal_type: None,
                        character_string_type: Some(character_string),
                        byte_string_type: None,
                        record_field_types: None,
                    },
                    PropertyTypeDef {
                        name: digest,
                        value_type: PropertyValueType::Bytes,
                        list_element_type: None,
                        required: false,
                        default: None,
                        immutable: false,
                        unique: false,
                        decimal_type: None,
                        character_string_type: None,
                        byte_string_type: Some(byte_string),
                        record_field_types: None,
                    },
                    PropertyTypeDef {
                        name: history,
                        value_type: PropertyValueType::List,
                        list_element_type: Some(crate::graph_types::PropertyElementType::Decimal(
                            history_decimal,
                        )),
                        required: false,
                        default: None,
                        immutable: false,
                        unique: false,
                        decimal_type: None,
                        character_string_type: None,
                        byte_string_type: None,
                        record_field_types: None,
                    },
                    PropertyTypeDef {
                        name: payloads,
                        value_type: PropertyValueType::List,
                        list_element_type: Some(
                            crate::graph_types::PropertyElementType::ByteString(
                                history_byte_string,
                            ),
                        ),
                        required: false,
                        default: None,
                        immutable: false,
                        unique: false,
                        decimal_type: None,
                        character_string_type: None,
                        byte_string_type: None,
                        record_field_types: None,
                    },
                    PropertyTypeDef {
                        name: aliases,
                        value_type: PropertyValueType::List,
                        list_element_type: Some(
                            crate::graph_types::PropertyElementType::CharacterString(
                                history_character_string,
                            ),
                        ),
                        required: false,
                        default: None,
                        immutable: false,
                        unique: false,
                        decimal_type: None,
                        character_string_type: None,
                        byte_string_type: None,
                        record_field_types: None,
                    },
                ],
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: Vec::new(),
        };
        let graph = SharedGraph::builder(GraphId::new(213))
            .bound_to(graph_type)
            .unwrap()
            .build()
            .unwrap()
            .read()
            .as_ref()
            .clone();

        let bytes = encode_graph_types(&graph).unwrap();
        assert_eq!(bytes.first(), Some(&GTYP_VERSION));

        let decoded = decode_graph_types(&bytes).unwrap();
        assert_eq!(decoded.len(), 1);
        let decoded_graph_type = &decoded[0].1;
        decoded_graph_type.validate_ref().unwrap();
        let property = &decoded_graph_type.node_types[0].properties[0];
        assert_eq!(property.value_type, PropertyValueType::RecordTyped);
        assert_eq!(property.record_field_types, Some(record_field_types));
        let property = &decoded_graph_type.node_types[0].properties[1];
        assert_eq!(property.value_type, PropertyValueType::Decimal);
        assert_eq!(property.decimal_type, Some(decimal));
        let property = &decoded_graph_type.node_types[0].properties[2];
        assert_eq!(property.value_type, PropertyValueType::String);
        assert_eq!(property.character_string_type, Some(character_string));
        let property = &decoded_graph_type.node_types[0].properties[3];
        assert_eq!(property.value_type, PropertyValueType::Bytes);
        assert_eq!(property.byte_string_type, Some(byte_string));
        let property = &decoded_graph_type.node_types[0].properties[4];
        assert_eq!(
            property.list_element_type,
            Some(crate::graph_types::PropertyElementType::Decimal(
                history_decimal
            ))
        );
        let property = &decoded_graph_type.node_types[0].properties[5];
        assert_eq!(
            property.list_element_type,
            Some(crate::graph_types::PropertyElementType::ByteString(
                history_byte_string
            ))
        );
        let property = &decoded_graph_type.node_types[0].properties[6];
        assert_eq!(
            property.list_element_type,
            Some(crate::graph_types::PropertyElementType::CharacterString(
                history_character_string
            ))
        );
    }

    #[test]
    fn decode_rejects_unknown_or_empty_version() {
        // The clean break rejects any non-current version byte (e.g. the retired 0xB7 V3
        // magic) and an empty section, rather than silently falling through to a legacy
        // decoder or treating it as zero rows.
        let person = db_string("UnknownVersionPerson").unwrap();
        let graph_type = GraphTypeDef {
            name: db_string("unknown.version.graph").unwrap(),
            node_types: vec![NodeTypeDef {
                name: person.clone(),
                key_labels: LabelSet::single(person),
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: Vec::new(),
        };
        let graph = SharedGraph::builder(GraphId::new(214))
            .bound_to(graph_type)
            .unwrap()
            .build()
            .unwrap()
            .read()
            .as_ref()
            .clone();
        let mut bytes = encode_graph_types(&graph).unwrap();
        bytes[0] = 0xB7; // retired V3 magic
        assert!(matches!(
            decode_graph_types(&bytes),
            Err(crate::ProviderError::InvalidPayload { .. })
        ));
        assert!(matches!(
            decode_graph_types(&[]),
            Err(crate::ProviderError::InvalidPayload { .. })
        ));
    }

    #[test]
    fn decode_rejects_pre_descriptor_version_one() {
        // Typed-descriptor clean break: a version-1 GTYP payload archives
        // `PropertyTypeDef` WITHOUT the decimal/character-string/byte-string
        // descriptor fields, so the version gate must reject it with a clean
        // versioned error instead of bytechecking the body against the wrong
        // archived shape.
        let person = db_string("PreDescriptorPerson").unwrap();
        let graph_type = GraphTypeDef {
            name: db_string("pre.descriptor.graph").unwrap(),
            node_types: vec![NodeTypeDef {
                name: person.clone(),
                key_labels: LabelSet::single(person),
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: Vec::new(),
        };
        let graph = SharedGraph::builder(GraphId::new(215))
            .bound_to(graph_type)
            .unwrap()
            .build()
            .unwrap()
            .read()
            .as_ref()
            .clone();
        let mut bytes = encode_graph_types(&graph).unwrap();
        bytes[0] = 1; // pre-descriptor GTYP version
        assert!(matches!(
            decode_graph_types(&bytes),
            Err(crate::ProviderError::InvalidPayload { reason })
                if reason.contains("version 1 is unsupported")
        ));
    }
}
