//! rkyv section payloads for the core graph provider.
//!
//! Section payload format: rkyv 0.8 archives over sorted `Vec<(K, Row)>`
//! intermediates per spec 04 §4.3 / decision D14. Per-row property bags
//! are postcard-encoded inside an `Arc<[u8]>` field on each archived row until
//! every stored `Value` variant has rkyv archivability.
//!
//! Compatibility note: per spec 04 §4.6, any format change to a CORE section
//! bumps the snapshot envelope version so older snapshots fail with
//! `PersistError::UnsupportedVersion` instead of being decoded as garbled
//! bytes. BRIEF-Item-4a STEP 9 exercises this: `CORE/NODE` / `CORE/EDGE` now
//! persist the explicit external `NodeId` / `EdgeId` per row (read from the
//! `row_to_id` column) instead of synthesizing `row + 1`, so a future
//! 4b-compacted snapshot whose ids != row+1 round-trips. That change bumped
//! `SNAPSHOT_VERSION_MINOR` 0 -> 1 (selene-persist); pre-STEP-9 (minor 0)
//! snapshots are cleanly rejected — a clean break, not a dual decoder
//! (deferred to 4c per the D14 amendment).
//! `CORE/VIDX` later added optional IVF construction config beside HNSW config,
//! which bumped `SNAPSHOT_VERSION_MINOR` 2 -> 3 for the same clean-break reason.
//! The typed-descriptor stream then added `decimal_type` /
//! `character_string_type` / `byte_string_type` fields to the `CORE/GTYP`
//! `PropertyTypeDef` archive (plus descriptor variants on
//! `PropertyElementType` / `RecordFieldType`), which bumped
//! `SNAPSHOT_VERSION_MINOR` 3 -> 4 and the section-internal `GTYP_VERSION`
//! 1 -> 2.
//! Edge-property index registrations then added an entity discriminator to
//! `CORE/SCMA`, which bumped `SNAPSHOT_VERSION_MINOR` 4 -> 5 and
//! `SCMA_VERSION` 2 -> 3.
//! F01-PR03 adds edge directionality to reconstruction inputs, guarded by
//! `SNAPSHOT_VERSION_MINOR` 5 -> 6. This only adapts the current alpha codec;
//! F02-PR03 still owns format-2 byte encoding.
//!
//! Schema rows are stored in canonical wire order by their string keys and are
//! decoded defensively before duplicate validation.

mod codec;
mod gtyp;
mod rows;
mod schema;

// Re-exported to `core_provider` so the cap-boundary unit test can reach it.
#[cfg(test)]
pub(super) use codec::ensure_section_within_cap;
pub(super) use gtyp::{decode_graph_types, encode_graph_types};
pub(super) use rows::{
    EdgeRow, MetaPayload, NodeRow, decode_edges, decode_meta, decode_nodes, encode_edges,
    encode_meta, encode_nodes,
};
#[cfg(test)]
pub(super) use schema::SCMA_VERSION;
pub(super) use schema::{
    CompositeSchemaEntry, CompositeSchemaKey, SchemaEntityKind, SchemaEntry, SchemaKey,
    TextSchemaEntry, TextSchemaKey, VectorSchemaEntry, VectorSchemaKey, decode_composite_schemas,
    decode_schemas, decode_text_schemas, decode_vector_schemas, encode_composite_schemas,
    encode_schemas, encode_text_schemas, encode_vector_schemas,
};
