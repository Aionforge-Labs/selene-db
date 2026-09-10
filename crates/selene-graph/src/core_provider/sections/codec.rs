//! Generic rkyv/postcard section codec plumbing shared by the `CORE/*` snapshot
//! encoders and decoders.
//!
//! Split out of `sections.rs` (700-LOC cap): the section-specific encode/decode
//! functions live in the parent module; the cap check, the rkyv (snapshot) and
//! postcard (property-blob) round-trip wrappers, and the row-ordering /
//! id-uniqueness validators that every positional section reuses live here.

use std::sync::Arc;

use selene_core::PropertyMap;
use selene_persist::MAX_SECTION_PAYLOAD_BYTES;

use crate::core_provider::{inconsistent, invalid_payload, serialization_failed};

// Each supported recursive default uses at most two archive subtrees (Vec and
// Box) per semantic level. The remaining allowance covers the section, schema,
// and property carriers. Semantic limits are checked by the owning decoder.
const MAX_ARCHIVE_SUBTREE_DEPTH: usize = 2 * selene_core::MAX_STORED_VALUE_DEPTH + 32;

/// Reject a section payload that exceeds the per-section byte cap.
pub(in crate::core_provider) fn ensure_section_within_cap(
    section: &'static str,
    len: usize,
) -> Result<(), crate::ProviderError> {
    if len > MAX_SECTION_PAYLOAD_BYTES {
        return Err(inconsistent(format!(
            "{section} core section exceeds 1 GiB cap; multi-section payloads are not supported"
        )));
    }
    Ok(())
}

/// Encode `value` to an rkyv section payload, enforcing the byte cap.
pub(in crate::core_provider::sections) fn encode_rkyv<T>(
    value: &T,
    section: &'static str,
) -> Result<Vec<u8>, crate::ProviderError>
where
    T: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                rkyv::rancor::Error,
            >,
        >,
{
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(value)
        .map_err(|error| serialization_failed(format!("{section} rkyv encode failed: {error}")))?
        .into_vec();
    ensure_section_within_cap(section, bytes.len())?;
    Ok(bytes)
}

/// Decode (bytecheck + deserialize) an rkyv section payload, enforcing the cap.
pub(in crate::core_provider::sections) fn decode_rkyv<T>(
    bytes: &[u8],
    section: &'static str,
) -> Result<T, crate::ProviderError>
where
    T: rkyv::Archive,
    T::Archived: for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, rkyv::rancor::Error>>
        + rkyv::Deserialize<T, rkyv::api::high::HighDeserializer<rkyv::rancor::Error>>,
{
    ensure_section_within_cap(section, bytes.len())?;
    use rkyv::validation::{Validator, archive::ArchiveValidator, shared::SharedValidator};
    let mut validator = Validator::new(
        ArchiveValidator::with_max_depth(
            bytes,
            std::num::NonZeroUsize::new(MAX_ARCHIVE_SUBTREE_DEPTH),
        ),
        SharedValidator::new(),
    );
    let archived = rkyv::api::access_with_context::<T::Archived, _, rkyv::rancor::Error>(
        bytes,
        &mut validator,
    )
    .map_err(|error| invalid_payload(format!("{section} rkyv bytecheck/decode failed: {error}")))?;
    rkyv::api::deserialize_using::<T, _, rkyv::rancor::Error>(archived, &mut rkyv::de::Pool::new())
        .map_err(|error| {
            invalid_payload(format!("{section} rkyv bytecheck/decode failed: {error}"))
        })
}

#[cfg(test)]
#[path = "codec_nesting_tests.rs"]
mod nesting_tests;

/// Encode a property map to a postcard blob (the per-row property column form).
pub(in crate::core_provider::sections) fn encode_properties_blob(
    properties: &PropertyMap,
    section: &'static str,
) -> Result<Arc<[u8]>, crate::ProviderError> {
    let bytes = postcard::to_stdvec(properties).map_err(|error| {
        serialization_failed(format!(
            "{section} property postcard encode failed: {error}"
        ))
    })?;
    Ok(Arc::from(bytes.into_boxed_slice()))
}

/// Decode a per-row property postcard blob.
pub(in crate::core_provider::sections) fn decode_properties_blob(
    bytes: &[u8],
    section: &'static str,
) -> Result<PropertyMap, crate::ProviderError> {
    postcard::from_bytes(bytes).map_err(|error| {
        invalid_payload(format!(
            "{section} property postcard decode failed: {error}"
        ))
    })
}

/// Validate that rows in a key-sorted section are strictly ascending and unique.
pub(in crate::core_provider::sections) fn validate_sorted_unique<K, V>(
    rows: &[(K, V)],
    section: &'static str,
) -> Result<(), crate::ProviderError>
where
    K: Ord + std::fmt::Debug,
{
    for pair in rows.windows(2) {
        if pair[0].0 >= pair[1].0 {
            return Err(invalid_payload(format!(
                "{section} rows must be strictly sorted by key with no duplicates; observed {:?} then {:?}",
                pair[0].0, pair[1].0
            )));
        }
    }
    Ok(())
}

/// Validate one *non-tombstone* id from a positional row section is unique.
///
/// BRIEF-Item-4a STEP 9: the `CORE/NODE` and `CORE/EDGE` sections are positional
/// (row order = section order) and store the explicit external id per row, so
/// they are NOT sorted by id — a 4b-compacted snapshot may store ids in any
/// order. Aborted-tx hole rows all carry the type's `TOMBSTONE` sentinel and are
/// exempt from the uniqueness check (many holes share it). Every committed id
/// must still appear at most once; a duplicate is a corrupt snapshot.
pub(in crate::core_provider::sections) fn validate_id_unique<K>(
    seen: &mut std::collections::HashSet<K>,
    id: K,
    tombstone: K,
    section: &'static str,
) -> Result<(), crate::ProviderError>
where
    K: Copy + Eq + std::hash::Hash + std::fmt::Debug,
{
    if id == tombstone {
        return Ok(());
    }
    if !seen.insert(id) {
        return Err(invalid_payload(format!(
            "{section} rows must have unique non-tombstone ids; observed duplicate {id:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod validate_id_unique_tests {
    use std::collections::HashSet;

    use super::validate_id_unique;
    use selene_core::NodeId;

    // BRIEF-Item-4a STEP 9: the two contract branches the positional format
    // introduces — duplicate-real-id rejection and many-holes-exempt — under
    // direct test (the round-trip tests only cover them incidentally).

    #[test]
    fn rejects_duplicate_non_tombstone_id() {
        let mut seen = HashSet::new();
        validate_id_unique(&mut seen, NodeId::new(5), NodeId::TOMBSTONE, "CORE/NODE")
            .expect("first id is unique");
        validate_id_unique(&mut seen, NodeId::TOMBSTONE, NodeId::TOMBSTONE, "CORE/NODE")
            .expect("tombstone row is exempt");
        let err = validate_id_unique(&mut seen, NodeId::new(5), NodeId::TOMBSTONE, "CORE/NODE")
            .expect_err("a duplicate committed id is a corrupt snapshot");
        assert!(
            format!("{err}").contains("unique non-tombstone"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn allows_multiple_tombstone_hole_rows() {
        // Aborted-tx hole rows all share the sentinel and are exempt; only the
        // real ids between them must be unique.
        let mut seen = HashSet::new();
        for id in [
            NodeId::new(1),
            NodeId::TOMBSTONE,
            NodeId::TOMBSTONE,
            NodeId::new(2),
        ] {
            validate_id_unique(&mut seen, id, NodeId::TOMBSTONE, "CORE/NODE")
                .expect("unique real ids and multiple tombstones are valid");
        }
    }
}
