//! Cross-decoder symmetry coverage (GRAPH-20, GRAPH-21).
//!
//! The positional `CORE/NODE` and `CORE/EDGE` decoders both validate duplicate
//! real ids after the rkyv bytecheck; the `validate_sorted_unique` and bytecheck
//! guards repeat across `CORE/META` and `CORE/GTYP`. The per-id helper is
//! unit-tested in `sections::codec`, but the *wiring* — that
//! `decode_nodes`/`decode_edges` actually invoke it — and the bytecheck
//! rejection across the structurally identical edge/meta/gtyp decoders were only
//! covered for `decode_nodes`. A decoder that skipped duplicate-id validation or
//! bytecheck would corrupt recovery silently; these make every decoder's guard
//! observable.

use selene_core::PropertyValueType;

use super::sections::{
    decode_edges, decode_graph_types, decode_meta, decode_nodes, encode_edges, encode_graph_types,
    encode_meta,
};
use super::*;
use crate::graph_types::{GraphTypeDef, NodeTypeDef, PropertyTypeDef, ValidationMode};

// ───────────────────────── GRAPH-20: duplicate-id wiring ─────────────────────

#[test]
fn decode_nodes_rejects_duplicate_committed_id() {
    // Forge a corrupt CORE/NODE section that the real `encode_nodes` will faithfully
    // serialize: two alive rows whose `row_to_id` column carries the SAME external
    // id (5). This is constructible only by a raw column build (the funnel never
    // reuses an id), and the rkyv bytecheck passes (5 is a valid NodeId), so ONLY
    // the duplicate-id guard WIRED into `decode_nodes` can reject it. A dropped
    // guard would silently accept the section and collide two rows onto one
    // id during recovery. Going through the real encode + decode (not byte
    // surgery) keeps the test robust against rkyv layout changes.
    let mut graph = SeleneGraph::new(GraphId::new(2_000));
    for _ in 0..2 {
        graph.node_store.labels.push(LabelSet::new());
        graph.node_store.properties.push(PropertyMap::new());
        graph.node_store.row_to_id.push(NodeId::new(5)); // DUPLICATE id
    }
    graph.node_store.alive_mut().insert(0);
    graph.node_store.alive_mut().insert(1);

    let bytes = encode_nodes(&graph).unwrap();
    let err = decode_nodes(&bytes).expect_err("decode_nodes must reject a duplicate committed id");
    assert!(
        matches!(&err, ProviderError::InvalidPayload { reason } if reason.contains("unique non-tombstone")),
        "expected duplicate-id rejection, got {err:?}",
    );
}

#[test]
fn decode_edges_rejects_duplicate_committed_id() {
    // Edge-side sibling: two alive edge rows whose `row_to_id` column shares one
    // EdgeId (5). Faithfully encoded by `encode_edges`, bytecheck-valid, so only
    // the duplicate-id guard wired into `decode_edges` rejects it.
    let mut graph = SeleneGraph::new(GraphId::new(2_001));
    let label = db_string("dup.edge").unwrap();
    for _ in 0..2 {
        graph.edge_store.label.push(label.clone());
        graph.edge_store.source.push(NodeId::new(1));
        graph
            .edge_store
            .directionality
            .push(selene_core::EdgeDirectionality::Directed);
        graph.edge_store.target.push(NodeId::new(2));
        graph.edge_store.properties.push(PropertyMap::new());
        graph.edge_store.row_to_id.push(EdgeId::new(5)); // DUPLICATE id
    }
    graph.edge_store.alive_mut().insert(0);
    graph.edge_store.alive_mut().insert(1);

    let bytes = encode_edges(&graph).unwrap();
    let err = decode_edges(&bytes).expect_err("decode_edges must reject a duplicate committed id");
    assert!(
        matches!(&err, ProviderError::InvalidPayload { reason } if reason.contains("unique non-tombstone")),
        "expected duplicate-id rejection, got {err:?}",
    );
}

// ───────────────────────── GRAPH-21: bytecheck symmetry ─────────────────────

#[test]
fn bytecheck_rejects_truncated_edge_section() {
    // Symmetric with `bytecheck_rejects_truncated_node_section`: a truncated
    // CORE/EDGE payload must fail the rkyv bytecheck inside `decode_edges`.
    let graph = graph_with_edge();
    let mut bytes = encode_edges(&graph).unwrap();
    let new_len = bytes.len().saturating_sub(16);
    bytes.truncate(new_len);
    assert!(
        matches!(
            decode_edges(&bytes),
            Err(ProviderError::InvalidPayload { reason }) if reason.contains("bytecheck")
        ),
        "truncated CORE/EDGE must fail bytecheck",
    );
}

#[test]
fn bytecheck_rejects_corrupted_edge_root_pointer() {
    // rkyv stores the archive root (a relative pointer + length) at the END of
    // the buffer; corrupting it makes the root point out of bounds, which the
    // bytecheck inside `decode_edges` must reject (symmetric with the parent
    // `bytecheck_rejects_corrupted_section_header` node coverage).
    let graph = graph_with_edge();
    let mut bytes = encode_edges(&graph).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    assert!(
        matches!(
            decode_edges(&bytes),
            Err(ProviderError::InvalidPayload { reason }) if reason.contains("bytecheck")
        ),
        "corrupted CORE/EDGE root pointer must fail bytecheck",
    );
}

#[test]
fn bytecheck_rejects_truncated_meta_section() {
    // CORE/META is a fixed-shape rkyv archive; a truncated payload must fail
    // bytecheck inside `decode_meta` rather than decode garbage metadata.
    let graph = graph_with_node();
    let mut bytes = encode_meta(&graph.meta, 7).unwrap();
    let new_len = bytes.len().saturating_sub(8);
    bytes.truncate(new_len);
    assert!(
        matches!(
            decode_meta(&bytes),
            Err(ProviderError::InvalidPayload { reason }) if reason.contains("bytecheck")
        ),
        "truncated CORE/META must fail bytecheck",
    );
}

#[test]
fn bytecheck_rejects_truncated_meta_to_one_byte() {
    // A single residual byte cannot satisfy the fixed-size CORE/META archive
    // layout, so `decode_meta`'s bytecheck must reject it rather than read past
    // the buffer. (CORE/META is a pointerless POD archive, so a flipped DATA byte
    // is a legitimately-valid different archive — only length/structure
    // violations are bytecheck-rejectable, hence the truncation form here.)
    let graph = graph_with_node();
    let bytes = encode_meta(&graph.meta, 7).unwrap();
    let truncated = &bytes[..1];
    assert!(
        matches!(
            decode_meta(truncated),
            Err(ProviderError::InvalidPayload { reason }) if reason.contains("bytecheck")
        ),
        "a 1-byte CORE/META must fail bytecheck",
    );
}

#[test]
fn bytecheck_rejects_corrupted_gtyp_rkyv_body() {
    // CORE/GTYP is `[version_byte][rkyv archive]`. A flipped byte INSIDE the rkyv
    // body (past the leading version byte, which `decode_graph_types` checks
    // separately) must fail the rkyv bytecheck inside `decode_rkyv`, not decode a
    // garbled GraphTypeDef. Pairs with `gtyp::decode_rejects_unknown_or_empty_version`
    // (which covers the version byte) to give CORE/GTYP the same truncate/flip
    // coverage CORE/NODE already had.
    let person = db_string("CorruptGtypPerson").unwrap();
    let graph_type = GraphTypeDef {
        name: db_string("corrupt.gtyp.graph").unwrap(),
        node_types: vec![NodeTypeDef {
            name: person.clone(),
            key_labels: LabelSet::single(person),
            properties: vec![PropertyTypeDef {
                name: db_string("serial").unwrap(),
                value_type: PropertyValueType::String,
                list_element_type: None,
                required: false,
                default: None,
                immutable: false,
                unique: false,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                record_field_types: None,
            }],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    };
    let graph = SharedGraph::builder(GraphId::new(2_010))
        .bound_to(graph_type)
        .unwrap()
        .build()
        .unwrap()
        .read()
        .as_ref()
        .clone();
    let mut bytes = encode_graph_types(&graph).unwrap();
    // Honest round-trip first.
    assert_eq!(decode_graph_types(&bytes).unwrap().len(), 1);
    // Flip a byte in the rkyv body (index 1.. is past the version byte). Flip the
    // LAST byte (the rkyv root-pointer region is most sensitive to corruption).
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    assert!(
        matches!(
            decode_graph_types(&bytes),
            Err(ProviderError::InvalidPayload { reason }) if reason.contains("bytecheck")
        ),
        "corrupted CORE/GTYP rkyv body must fail bytecheck",
    );
}
