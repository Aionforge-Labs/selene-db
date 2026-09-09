use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use selene_core::{
    Change, EdgeId, GraphId, HlcTimestamp, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap,
    PropertyValueType, Value, VectorValue, db_string,
};
use selene_persist::{WalConfig, WalReader, WalWriter};

use super::sections::{
    SCMA_VERSION, SchemaEntityKind, SchemaEntry, SchemaKey, decode_edges, decode_graph_types,
    decode_meta, decode_nodes, decode_schemas, encode_edges, encode_graph_types, encode_meta,
    encode_nodes, ensure_section_within_cap,
};
use super::*;
use crate::graph::PropertyIndexEntry;
use crate::typed_index::TypedIndex;
use crate::{DurableProvider, GraphError, SeleneGraph, SharedGraph, TypedIndexKind};

#[path = "tests/checkpoint_support.rs"]
mod checkpoint_support;
#[path = "tests/codec_symmetry.rs"]
mod codec_symmetry;
#[path = "tests/composites.rs"]
mod composites;
#[path = "tests/cpix.rs"]
mod cpix;
#[path = "tests/durable_state.rs"]
mod durable_state;
#[path = "tests/gtyp.rs"]
mod gtyp;
#[path = "tests/recovery_mode.rs"]
mod recovery_mode;
#[path = "tests/tidx.rs"]
mod tidx;

fn prop(name: &str, value: Value) -> PropertyMap {
    PropertyMap::from_pairs([(db_string(name).unwrap(), value)]).unwrap()
}

fn temp_wal_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "selene-core-provider-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    dir.join(selene_persist::DEFAULT_WAL_FILE_NAME)
}

fn wal_entries(path: &Path) -> Vec<selene_persist::WalEntry> {
    let reader = WalReader::open(path).unwrap();
    reader
        .iterate(|_| true)
        .unwrap()
        .map(|entry| entry.unwrap().into_entry().unwrap())
        .collect()
}

fn full_value_property_map(prefix: &str) -> PropertyMap {
    PropertyMap::from_pairs([
        (
            db_string(&format!("{prefix}.bool")).unwrap(),
            Value::Bool(true),
        ),
        (db_string(&format!("{prefix}.int")).unwrap(), Value::Int(-7)),
        (
            db_string(&format!("{prefix}.float")).unwrap(),
            Value::Float(1.25),
        ),
        (
            db_string(&format!("{prefix}.string")).unwrap(),
            Value::String(db_string("core.values.string").unwrap()),
        ),
        (
            db_string(&format!("{prefix}.decimal")).unwrap(),
            Value::Decimal("123.45".parse().unwrap()),
        ),
        (
            db_string(&format!("{prefix}.bytes")).unwrap(),
            Value::Bytes(Arc::from([1_u8, 2, 3, 4])),
        ),
        (
            db_string(&format!("{prefix}.uuid")).unwrap(),
            Value::Uuid(uuid::Uuid::from_u128(42)),
        ),
        (
            db_string(&format!("{prefix}.vector")).unwrap(),
            Value::Vector(VectorValue::new(vec![1.0, 2.0, 3.0]).unwrap()),
        ),
        (
            db_string(&format!("{prefix}.zoned_datetime")).unwrap(),
            Value::ZonedDateTime(Box::new(
                "2026-05-07T12:34:56-04:00[America/New_York]"
                    .parse()
                    .unwrap(),
            )),
        ),
        (
            db_string(&format!("{prefix}.date")).unwrap(),
            Value::Date("2026-05-07".parse().unwrap()),
        ),
        (
            db_string(&format!("{prefix}.local_datetime")).unwrap(),
            Value::LocalDateTime("2026-05-07T12:34:56".parse().unwrap()),
        ),
        (
            db_string(&format!("{prefix}.local_time")).unwrap(),
            Value::LocalTime("12:34:56".parse().unwrap()),
        ),
        (
            db_string(&format!("{prefix}.duration")).unwrap(),
            Value::Duration(Box::new("PT1H2S".parse().unwrap())),
        ),
    ])
    .unwrap()
}

fn graph_with_node() -> SeleneGraph {
    let shared = SharedGraph::builder(GraphId::new(1)).build().unwrap();
    let mut txn = shared.begin_write();
    let id = {
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(db_string("core.node").unwrap()),
                prop("core.name", Value::Int(7)),
            )
            .unwrap()
    };
    assert_eq!(id, NodeId::new(1));
    txn.commit().unwrap();
    shared.read().as_ref().clone()
}

fn graph_with_edge() -> SeleneGraph {
    let shared = SharedGraph::builder(GraphId::new(1)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        let a = mutator
            .create_node(
                LabelSet::single(db_string("core.a").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
        let b = mutator
            .create_node(
                LabelSet::single(db_string("core.b").unwrap()),
                PropertyMap::new(),
            )
            .unwrap();
        mutator
            .create_edge(
                db_string("core.edge").unwrap(),
                a,
                b,
                prop("core.weight", Value::Int(3)),
            )
            .unwrap();
    }
    txn.commit().unwrap();
    shared.read().as_ref().clone()
}

fn graph_type() -> crate::GraphTypeDef {
    crate::GraphTypeDef {
        name: db_string("core.gtyp").unwrap(),
        node_types: vec![crate::NodeTypeDef {
            name: db_string("core.gtyp.node").unwrap(),
            key_labels: LabelSet::single(db_string("CoreTypedNode").unwrap()),
            properties: vec![crate::PropertyTypeDef {
                name: db_string("core.gtyp.name").unwrap(),
                value_type: PropertyValueType::String,
                list_element_type: None,
                required: true,
                default: None,
                immutable: false,
                unique: false,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                record_field_types: None,
            }],
            validation_mode: crate::ValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    }
}

fn typed_graph() -> SeleneGraph {
    let shared = SharedGraph::builder(GraphId::new(8))
        .bound_to(graph_type())
        .unwrap()
        .build()
        .unwrap();
    shared.read().as_ref().clone()
}

#[test]
fn new_for_live_holds_snapshot_pointer() {
    let snapshot = Arc::new(ArcSwap::from_pointee(SeleneGraph::new(GraphId::new(1))));
    let provider = CoreProvider::new_for_live(Arc::clone(&snapshot));
    let inner = provider.inner.lock();
    match &*inner {
        CoreInner::Live {
            snapshot: observed, ..
        } => assert!(Arc::ptr_eq(observed, &snapshot)),
        CoreInner::Recovery { .. } => panic!("expected live provider"),
    }
}

#[test]
fn new_for_recovery_starts_empty() {
    let provider = CoreProvider::new_for_recovery();
    let graph = provider.finish_recovery(GraphId::new(1), None).unwrap();
    assert_eq!(graph.node_count(), 0);
    assert_eq!(graph.edge_count(), 0);
}

#[test]
fn finish_recovery_on_live_mode_is_error() {
    let snapshot = Arc::new(ArcSwap::from_pointee(SeleneGraph::new(GraphId::new(1))));
    let provider = CoreProvider::new_for_live(snapshot);
    assert!(matches!(
        provider.finish_recovery(GraphId::new(1), None),
        Err(GraphError::Provider(ProviderError::Inconsistent { reason }))
            if reason.contains("finish_recovery called on live-mode")
    ));
}

#[test]
fn encode_decode_round_trip_meta() {
    let graph = graph_with_node();
    let bytes = encode_meta(&graph.meta, 9).unwrap();
    let payload = decode_meta(&bytes).unwrap();
    assert_eq!(payload.graph_id, graph.meta.graph_id);
    assert_eq!(payload.generation, graph.meta.generation);
    assert_eq!(payload.next_node_id, graph.meta.next_node_id);
    assert_eq!(payload.next_edge_id, graph.meta.next_edge_id);
    assert_eq!(payload.bound_type_index, None);
    assert_eq!(payload.sequence, 9);
}

#[test]
fn encode_decode_round_trip_empty_graph_types() {
    let graph = graph_with_node();
    let rows = decode_graph_types(&encode_graph_types(&graph).unwrap()).unwrap();
    assert!(rows.is_empty());
}

#[test]
fn scma_decode_resorts_rows_lexicographically() {
    let zebra = db_string("core.scma.zebra").unwrap();
    let apple = db_string("core.scma.apple").unwrap();
    let zebra_prop = db_string("core.scma.zebra.prop").unwrap();
    let apple_prop = db_string("core.scma.apple.prop").unwrap();
    let zebra_key = SchemaKey {
        entity: SchemaEntityKind::Node,
        label: zebra,
        property: zebra_prop,
    };
    let apple_key = SchemaKey {
        entity: SchemaEntityKind::Node,
        label: apple,
        property: apple_prop,
    };
    // Out-of-(label, property)-order rows under the current (versioned) layout.
    // `decode_schemas` must re-sort by the (label, property) key regardless of
    // input order; `SchemaKey` Ord is lexicographic through `DbString`, so the
    // "apple" row sorts ahead of the "zebra" row on decode.
    let rows = vec![
        (
            zebra_key.clone(),
            SchemaEntry {
                kind: TypedIndexKind::String,
                name: None,
            },
        ),
        (
            apple_key.clone(),
            SchemaEntry {
                kind: TypedIndexKind::I64,
                name: None,
            },
        ),
    ];
    let mut bytes = vec![SCMA_VERSION];
    bytes.extend(
        rkyv::to_bytes::<rkyv::rancor::Error>(&rows)
            .unwrap()
            .into_vec(),
    );

    let decoded = decode_schemas(&bytes).unwrap();

    assert_eq!(
        decoded,
        vec![
            (
                apple_key,
                SchemaEntry {
                    kind: TypedIndexKind::I64,
                    name: None,
                }
            ),
            (
                zebra_key,
                SchemaEntry {
                    kind: TypedIndexKind::String,
                    name: None,
                }
            ),
        ]
    );
}

#[test]
fn scma_v3_round_trip_preserves_property_index_names() {
    let label = db_string("core.scma.named.label").unwrap();
    let edge_label = db_string("core.scma.named.edge").unwrap();
    let property = db_string("core.scma.named.property").unwrap();
    let name = db_string("core.scma.named.index").unwrap();
    let edge_name = db_string("core.scma.named.edge.index").unwrap();
    let mut graph = SeleneGraph::new(GraphId::new(9991));
    graph.property_index.insert(
        (label.clone(), property.clone()),
        PropertyIndexEntry::new(TypedIndex::new(TypedIndexKind::String), Some(name.clone())),
    );
    graph.edge_property_index.insert(
        (edge_label.clone(), property.clone()),
        PropertyIndexEntry::new(
            TypedIndex::new(TypedIndexKind::I64),
            Some(edge_name.clone()),
        ),
    );

    let decoded = decode_schemas(&encode_schemas(&graph).unwrap()).unwrap();

    assert_eq!(
        decoded,
        vec![
            (
                SchemaKey {
                    entity: SchemaEntityKind::Node,
                    label,
                    property: property.clone(),
                },
                SchemaEntry {
                    kind: TypedIndexKind::String,
                    name: Some(name),
                }
            ),
            (
                SchemaKey {
                    entity: SchemaEntityKind::Edge,
                    label: edge_label,
                    property,
                },
                SchemaEntry {
                    kind: TypedIndexKind::I64,
                    name: Some(edge_name),
                }
            )
        ]
    );
}

#[test]
fn scma_decode_rejects_duplicate_keys_after_resort() {
    let label = db_string("core.scma.dup.label").unwrap();
    let property = db_string("core.scma.dup.property").unwrap();
    let key = SchemaKey {
        entity: SchemaEntityKind::Node,
        label,
        property,
    };
    let rows = vec![
        (
            key.clone(),
            SchemaEntry {
                kind: TypedIndexKind::I64,
                name: None,
            },
        ),
        (
            key,
            SchemaEntry {
                kind: TypedIndexKind::String,
                name: None,
            },
        ),
    ];
    let mut bytes = vec![SCMA_VERSION];
    bytes.extend(
        rkyv::to_bytes::<rkyv::rancor::Error>(&rows)
            .unwrap()
            .into_vec(),
    );

    let result = decode_schemas(&bytes);

    assert!(result.is_err());
}

#[test]
fn scma_decode_rejects_empty_or_mismatched_version() {
    // GRAPH-23 clean break: with the legacy v1 no-magic fallback removed,
    // an empty section or a wrong leading version byte is a hard decode error.
    assert!(
        matches!(
            decode_schemas(&[]),
            Err(crate::ProviderError::InvalidPayload { .. })
        ),
        "empty CORE/SCMA must be rejected"
    );
    // A non-current version byte (e.g. the retired 0xA5 v2-magic value used a
    // different layout) must hard-reject rather than silently fall back.
    let wrong_version = SCMA_VERSION.wrapping_add(1);
    assert!(
        matches!(
            decode_schemas(&[wrong_version, 0, 0, 0, 0]),
            Err(crate::ProviderError::InvalidPayload { .. })
        ),
        "mismatched CORE/SCMA version must be rejected"
    );
}

#[test]
fn encode_decode_round_trip_bound_graph_type() {
    let graph = typed_graph();
    let rows = decode_graph_types(&encode_graph_types(&graph).unwrap()).unwrap();
    assert_eq!(rows, vec![(0, graph_type())]);

    let payload = decode_meta(&encode_meta(&graph.meta, 1).unwrap()).unwrap();
    assert_eq!(payload.bound_type_index, Some(0));
}

#[test]
fn recovery_rejects_meta_referencing_missing_gtyp() {
    let graph = typed_graph();
    let bytes = encode_meta(&graph.meta, 1).unwrap();
    let provider = CoreProvider::new_for_recovery();
    IndexProvider::read_section(provider.as_ref(), SubTag(CORE_META_SUB), &bytes).unwrap();
    assert!(matches!(
        provider.finish_recovery(GraphId::new(8), None),
        Err(GraphError::Provider(ProviderError::Inconsistent { reason }))
            if reason.contains("missing CORE/GTYP index 0")
    ));
}

#[test]
fn encode_decode_round_trip_nodes() {
    let empty = SeleneGraph::new(GraphId::new(1));
    assert!(
        decode_nodes(&encode_nodes(&empty).unwrap())
            .unwrap()
            .is_empty()
    );

    let one = graph_with_node();
    let rows = decode_nodes(&encode_nodes(&one).unwrap()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, NodeId::new(1));
    assert!(rows[0].1.alive);

    let shared = SharedGraph::builder(GraphId::new(2)).build().unwrap();
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        for index in 0..100 {
            mutator
                .create_node(
                    LabelSet::single(db_string("core.bulk").unwrap()),
                    prop("core.index", Value::Int(index)),
                )
                .unwrap();
        }
    }
    txn.commit().unwrap();
    let rows = decode_nodes(&encode_nodes(shared.read().as_ref()).unwrap()).unwrap();
    assert_eq!(rows.len(), 100);
    assert_eq!(rows[99].0, NodeId::new(100));
}

#[test]
fn encode_decode_round_trip_edges() {
    let graph = graph_with_edge();
    let rows = decode_edges(&encode_edges(&graph).unwrap()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, EdgeId::new(1));
    assert_eq!(rows[0].1.source, NodeId::new(1));
    assert_eq!(rows[0].1.target, NodeId::new(2));
}

#[test]
fn bytecheck_rejects_truncated_node_section() {
    let graph = graph_with_node();
    let mut bytes = encode_nodes(&graph).unwrap();
    let new_len = bytes.len().saturating_sub(16);
    bytes.truncate(new_len);
    assert!(matches!(
        decode_nodes(&bytes),
        Err(ProviderError::InvalidPayload { reason }) if reason.contains("bytecheck")
    ));
}

#[test]
fn bytecheck_rejects_corrupted_section_header() {
    let graph = graph_with_node();
    let mut bytes = encode_nodes(&graph).unwrap();
    bytes[0] ^= 0x80;
    assert!(matches!(
        decode_nodes(&bytes),
        Err(ProviderError::InvalidPayload { reason }) if reason.contains("bytecheck")
    ));
}

#[test]
fn properties_blob_round_trips_full_value_set() {
    let shared = SharedGraph::builder(GraphId::new(3)).build().unwrap();
    let expected = full_value_property_map("core.values");
    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(
                LabelSet::single(db_string("core.values.node").unwrap()),
                expected.clone(),
            )
            .unwrap();
    }
    txn.commit().unwrap();

    let rows = decode_nodes(&encode_nodes(shared.read().as_ref()).unwrap()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.properties, expected);
}

#[test]
fn encoded_nodes_carry_explicit_row_to_id() {
    // BRIEF-Item-4a STEP 9: the CORE/NODE section persists the explicit external
    // id from the `row_to_id` column (no longer synthesized as row+1, and no
    // longer contractually sorted-by-id). For this identity graph the single
    // committed node round-trips its real id at its row position.
    let graph = graph_with_node();
    let rows = decode_nodes(&encode_nodes(&graph).unwrap()).unwrap();
    let keys: Vec<_> = rows.into_iter().map(|(id, _)| id).collect();
    assert_eq!(keys, vec![NodeId::new(1)]);
}

#[test]
fn core_section_oversize_returns_inconsistent_error() {
    assert!(matches!(
        ensure_section_within_cap("CORE/NODE", selene_persist::MAX_SECTION_PAYLOAD_BYTES + 1),
        Err(ProviderError::Inconsistent { reason })
            if reason.contains("core section exceeds 1 GiB cap")
    ));
}

#[test]
fn live_mode_on_change_is_noop() {
    let snapshot = Arc::new(ArcSwap::from_pointee(SeleneGraph::new(GraphId::new(1))));
    let provider = CoreProvider::new_for_live(Arc::clone(&snapshot));
    IndexProvider::on_change(
        provider.as_ref(),
        &Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(db_string("core.noop").unwrap()),
            properties: PropertyMap::new(),
        },
    )
    .unwrap();
    let rows = decode_nodes(
        &IndexProvider::write_section(provider.as_ref(), SubTag(CORE_NODE_SUB)).unwrap(),
    )
    .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn live_mode_write_section_serializes_current_snapshot() {
    let snapshot = Arc::new(ArcSwap::from_pointee(SeleneGraph::new(GraphId::new(1))));
    let provider = CoreProvider::new_for_live(Arc::clone(&snapshot));
    snapshot.store(Arc::new(graph_with_node()));
    let rows = decode_nodes(
        &IndexProvider::write_section(provider.as_ref(), SubTag(CORE_NODE_SUB)).unwrap(),
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn live_mode_read_section_returns_inconsistent_error() {
    let snapshot = Arc::new(ArcSwap::from_pointee(SeleneGraph::new(GraphId::new(1))));
    let provider = CoreProvider::new_for_live(snapshot);
    assert!(matches!(
        IndexProvider::read_section(provider.as_ref(), SubTag(CORE_NODE_SUB), &[]),
        Err(ProviderError::Inconsistent { reason })
            if reason.contains("read_section called on live-mode")
    ));
}

#[test]
fn core_provider_writes_one_wal_entry_per_commit() {
    let path = temp_wal_path("one-entry");
    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    let snapshot = Arc::new(ArcSwap::from_pointee(SeleneGraph::new(GraphId::new(1))));
    let provider = CoreProvider::new_for_live_with_wal(snapshot, Some(DurableState::new(writer)));
    let changes = vec![
        Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(db_string("core.wal.a").unwrap()),
            properties: PropertyMap::new(),
        },
        Change::NodeCreated {
            id: NodeId::new(2),
            labels: LabelSet::single(db_string("core.wal.b").unwrap()),
            properties: PropertyMap::new(),
        },
        Change::EdgeCreated {
            directionality: selene_core::EdgeDirectionality::Directed,
            id: EdgeId::new(1),
            label: db_string("core.wal.edge").unwrap(),
            source: NodeId::new(1),
            target: NodeId::new(2),
            properties: PropertyMap::new(),
        },
    ];

    let timestamp = DurableProvider::next_timestamp(provider.as_ref());
    let seq = DurableProvider::write_commit(provider.as_ref(), None, &changes, timestamp).unwrap();
    assert_eq!(seq, 1);
    DurableProvider::flush(provider.as_ref()).unwrap();
    drop(provider);

    let entries = wal_entries(&path);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].changes.len(), 3);
    assert_eq!(entries[0].header.sequence, 1);
    assert_eq!(entries[0].header.hlc(), HlcTimestamp::new(1, 0));
}

#[test]
fn core_provider_threads_principal_through_wal() {
    let path = temp_wal_path("principal");
    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    let snapshot = Arc::new(ArcSwap::from_pointee(SeleneGraph::new(GraphId::new(1))));
    let provider = CoreProvider::new_for_live_with_wal(snapshot, Some(DurableState::new(writer)));
    let changes = vec![Change::NodeCreated {
        id: NodeId::new(1),
        labels: LabelSet::single(db_string("core.wal.principal").unwrap()),
        properties: PropertyMap::new(),
    }];

    let timestamp = DurableProvider::next_timestamp(provider.as_ref());
    let principal: Arc<[u8]> = Arc::from(&b"alice"[..]);
    DurableProvider::write_commit(provider.as_ref(), Some(&principal), &changes, timestamp)
        .unwrap();
    DurableProvider::flush(provider.as_ref()).unwrap();
    drop(provider);

    let entries = wal_entries(&path);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].header.principal.as_deref(), Some(&b"alice"[..]));
}
