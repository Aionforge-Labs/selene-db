//! WAL payload codec.

use selene_core::Change;

use crate::compression::{ZstdCompressor, compress_zstd, decompress_zstd_bounded};
use crate::entry_header::{
    COMPRESS_THRESHOLD, FLAG_PAYLOAD_COMPRESSED, MAX_WAL_ENTRY_BYTES, ensure_payload_len,
};
use crate::{PersistError, PersistResult, WalEntryHeader};

/// WAL payload compression policy.
///
/// The current writer default is zstd level 1 at [`COMPRESS_THRESHOLD`]. The
/// level remains fixed for now; this policy controls only whether a serialized
/// payload is compressed based on its encoded byte length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalCompression {
    threshold: Option<usize>,
}

impl WalCompression {
    /// Compress serialized payloads whose encoded length is at least
    /// `threshold_bytes`.
    #[must_use]
    pub const fn zstd(threshold_bytes: usize) -> Self {
        Self {
            threshold: Some(threshold_bytes),
        }
    }

    /// Disable WAL payload compression.
    #[must_use]
    pub const fn disabled() -> Self {
        Self { threshold: None }
    }

    /// Return the zstd threshold in bytes, or `None` when compression is off.
    #[must_use]
    pub const fn threshold_bytes(self) -> Option<usize> {
        self.threshold
    }

    fn should_compress(self, raw_len: usize) -> bool {
        self.threshold.is_some_and(|threshold| raw_len >= threshold)
    }
}

impl Default for WalCompression {
    fn default() -> Self {
        Self::zstd(COMPRESS_THRESHOLD)
    }
}

pub(crate) struct EncodedPayload {
    pub(crate) bytes: Vec<u8>,
    pub(crate) flags: u8,
    pub(crate) checksum_lo: u32,
}

#[cfg(test)]
pub(crate) fn encode_changes(changes: &[Change]) -> PersistResult<EncodedPayload> {
    encode_changes_with_compression(changes, WalCompression::default())
}

#[cfg(test)]
pub(crate) fn encode_changes_with_compression(
    changes: &[Change],
    compression: WalCompression,
) -> PersistResult<EncodedPayload> {
    encode_changes_with_compressor(changes, compression, None)
}

pub(crate) fn encode_changes_with_compressor(
    changes: &[Change],
    compression: WalCompression,
    compressor: Option<&mut Option<ZstdCompressor>>,
) -> PersistResult<EncodedPayload> {
    let raw = postcard::to_stdvec(changes)
        .map_err(|error| PersistError::PayloadCodec(error.to_string()))?;
    let (bytes, flags) = if compression.should_compress(raw.len()) {
        let bytes = match compressor {
            Some(slot) => {
                if slot.is_none() {
                    *slot = Some(ZstdCompressor::new(1)?);
                }
                slot.as_mut()
                    .expect("compressor slot was initialized")
                    .compress(raw.as_slice())?
            }
            None => compress_zstd(raw.as_slice(), 1)?,
        };
        (bytes, FLAG_PAYLOAD_COMPRESSED)
    } else {
        (raw, 0)
    };
    ensure_payload_len(bytes.len())?;
    let checksum_lo = checksum_lo(&bytes);
    Ok(EncodedPayload {
        bytes,
        flags,
        checksum_lo,
    })
}

pub(crate) fn decode_changes(bytes: &[u8], compressed: bool) -> PersistResult<Vec<Change>> {
    // postcard decodes from a borrow, so the uncompressed branch hands `bytes`
    // straight to the decoder rather than copying it into an owned buffer first.
    // Only the compressed branch must materialize the decompressed payload.
    if compressed {
        let raw = decompress_zstd_bounded(bytes, MAX_WAL_ENTRY_BYTES, |len, max| {
            PersistError::PayloadTooLarge { len, max }
        })?;
        postcard::from_bytes(&raw).map_err(|error| PersistError::PayloadCodec(error.to_string()))
    } else {
        postcard::from_bytes(bytes).map_err(|error| PersistError::PayloadCodec(error.to_string()))
    }
}

pub(crate) fn verify_checksum(header: &WalEntryHeader, bytes: &[u8]) -> PersistResult<()> {
    if checksum_lo(bytes) != header.checksum_lo {
        return Err(PersistError::ChecksumMismatch {
            sequence: header.sequence,
        });
    }
    Ok(())
}

pub(crate) fn checksum_lo(bytes: &[u8]) -> u32 {
    let hash = xxhash_rust::xxh3::xxh3_64(bytes);
    (hash & 0xFFFF_FFFF) as u32
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use proptest::prelude::*;
    use selene_core::{
        Change, DbString, EdgeId, GraphId, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap,
        Record, SchemaChange, Value, db_string,
    };

    use super::*;

    fn provider(name: &str) -> DbString {
        db_string(name).unwrap()
    }

    // A `NodeCreated` carrying a `Value::Bytes` property is the byte-payload-
    // bearing change used to exercise the size-sensitive codec paths
    // (compression threshold, bounded decode). Its serialized footprint scales
    // with the supplied byte buffer just like the former extension-event payload.
    fn change(bytes: impl Into<Vec<u8>>) -> Change {
        Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(provider("payload.node")),
            properties: PropertyMap::from_pairs([(
                provider("payload.property"),
                Value::Bytes(Arc::from(bytes.into())),
            )])
            .unwrap(),
        }
    }

    /// Full-fidelity `Value` generator covering every leaf variant the WAL frame
    /// must round-trip inside a `PropertyMap` (the ledger's coverage-followup #1
    /// surface): all numeric widths, `Decimal`, both string variants, `Bytes`,
    /// temporals, `Bool`, `Null`, `Uuid`, plus the nested named `List` / `Record`
    /// containers. `prop_recursive` bounds the nesting depth so the
    /// generator terminates.
    fn value_strategy() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::Int),
            any::<u64>().prop_map(Value::Uint),
            any::<i128>().prop_map(Value::Int128),
            any::<u128>().prop_map(Value::Uint128),
            any::<f64>().prop_map(Value::Float),
            any::<f32>().prop_map(Value::Float32),
            any::<i64>().prop_map(|m| Value::Decimal(rust_decimal::Decimal::new(m, 3))),
            "[a-z]{1,8}".prop_map(|s| Value::String(db_string(&format!("payload.v.{s}")).unwrap())),
            "[a-zA-Z0-9 ]{0,16}".prop_map(|s| Value::String(db_string(&s).unwrap())),
            proptest::collection::vec(any::<u8>(), 0..24).prop_map(|b| Value::Bytes(Arc::from(b))),
            Just(Value::Date("2024-06-15".parse().unwrap())),
            Just(Value::LocalDateTime("2024-06-15T12:30:00".parse().unwrap())),
            Just(Value::LocalTime("12:30:00".parse().unwrap())),
            Just(Value::Duration(Box::new("PT3H15M".parse().unwrap()))),
            any::<u128>().prop_map(|n| Value::Uuid(uuid::Uuid::from_u128(n))),
            Just(Value::Null),
        ];
        leaf.prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::List),
                proptest::collection::btree_map("[a-z]{1,6}", inner, 0..3).prop_map(|fields| {
                    Value::Record(Box::new(Record::Open(
                        fields
                            .into_iter()
                            .map(|(k, v)| (db_string(&format!("payload.f.{k}")).unwrap(), v))
                            .collect(),
                    )))
                }),
            ]
        })
    }

    /// A `PropertyMap` carrying 0..4 distinct keys, each a full-fidelity `Value`.
    fn property_map_strategy() -> impl Strategy<Value = PropertyMap> {
        proptest::collection::vec(("[a-z]{1,6}", value_strategy()), 0..4).prop_map(|pairs| {
            let pairs = pairs
                .into_iter()
                .map(|(k, v)| (db_string(&format!("payload.k.{k}")).unwrap(), v));
            PropertyMap::from_pairs(pairs).unwrap()
        })
    }

    /// Reuse the core `SchemaChange::ALL` census so every schema-change variant
    /// (including the heavy `GraphTypeCreated` / `NodeTypeAddedV2` shapes) flows
    /// through the WAL frame, not just a hand-picked subset.
    fn schema_change_strategy() -> impl Strategy<Value = SchemaChange> {
        (0..SchemaChange::VARIANT_COUNT).prop_map(|idx| SchemaChange::ALL[idx]())
    }

    fn label_diff_strategy() -> impl Strategy<Value = LabelDiff> {
        proptest::collection::vec("[a-z]{1,6}", 0..3).prop_map(|added| {
            let added = added
                .into_iter()
                .map(|s| db_string(&format!("payload.la.{s}")).unwrap());
            LabelDiff::new(added, []).unwrap()
        })
    }

    fn property_diff_strategy() -> impl Strategy<Value = PropertyDiff> {
        (
            proptest::collection::vec(("[a-z]{1,6}", value_strategy()), 0..3),
            proptest::collection::vec("[a-z]{1,6}", 0..2),
        )
            .prop_map(|(set, removed)| {
                let set = set
                    .into_iter()
                    .map(|(k, v)| (db_string(&format!("payload.ds.{k}")).unwrap(), v));
                let removed = removed
                    .into_iter()
                    .map(|s| db_string(&format!("payload.dr.{s}")).unwrap());
                PropertyDiff::new(set, removed).unwrap()
            })
    }

    fn change_strategy() -> impl Strategy<Value = Change> {
        let node_label = provider("payload.node");
        let edge_label = provider("payload.edge");
        let prop = provider("payload.property");
        prop_oneof![
            (1_u64..10_000, property_map_strategy()).prop_map({
                let value = node_label.clone();
                move |(id, properties)| Change::NodeCreated {
                    id: NodeId::new(id),
                    labels: LabelSet::single(value.clone()),
                    properties,
                }
            }),
            (
                1_u64..10_000,
                label_diff_strategy(),
                property_diff_strategy()
            )
                .prop_map(|(id, labels_diff, properties_diff)| Change::NodeUpdated {
                    id: NodeId::new(id),
                    labels_diff,
                    properties_diff,
                }),
            (1_u64..10_000).prop_map(|id| Change::NodeDeleted {
                id: NodeId::new(id),
            }),
            (1_u64..10_000).prop_map({
                let value = prop.clone();
                move |id| Change::NodePropertyRemoved {
                    id: NodeId::new(id),
                    property: value.clone(),
                }
            }),
            (1_u64..10_000).prop_map({
                let value = node_label.clone();
                move |id| Change::NodeLabelRemoved {
                    id: NodeId::new(id),
                    label: value.clone(),
                }
            }),
            (1_u64..10_000, 1_u64..10_000, property_map_strategy()).prop_map({
                let value = edge_label.clone();
                move |(id, target, properties)| Change::EdgeCreated {
                    directionality: selene_core::EdgeDirectionality::Directed,
                    id: EdgeId::new(id),
                    label: value.clone(),
                    source: NodeId::new(id),
                    target: NodeId::new(target),
                    properties,
                }
            }),
            (1_u64..10_000, property_diff_strategy()).prop_map(|(id, properties_diff)| {
                Change::EdgeUpdated {
                    id: EdgeId::new(id),
                    properties_diff,
                }
            }),
            (1_u64..10_000).prop_map(|id| Change::EdgeDeleted {
                id: EdgeId::new(id),
            }),
            (1_u64..10_000).prop_map(move |id| Change::EdgePropertyRemoved {
                id: EdgeId::new(id),
                property: prop.clone(),
            }),
            schema_change_strategy().prop_map(|change| Change::SchemaChanged {
                graph: GraphId::new(1),
                change,
            }),
            Just(Change::NodesOfTypeTruncated { label: node_label }),
            Just(Change::EdgesOfTypeTruncated { label: edge_label }),
            Just(Change::GraphReset {}),
            proptest::collection::vec(any::<u8>(), 0..512).prop_map(change),
        ]
    }

    #[test]
    fn small_payload_stays_uncompressed() {
        let changes = vec![change([1_u8, 2, 3])];
        let encoded = encode_changes(&changes).unwrap();
        assert_eq!(encoded.flags, 0);
        assert_eq!(decode_changes(&encoded.bytes, false).unwrap(), changes);
    }

    #[test]
    fn larger_payload_is_compressed() {
        let changes = vec![change(vec![7_u8; COMPRESS_THRESHOLD * 4])];
        let encoded = encode_changes(&changes).unwrap();
        assert_eq!(encoded.flags, FLAG_PAYLOAD_COMPRESSED);
        assert_eq!(decode_changes(&encoded.bytes, true).unwrap(), changes);
    }

    #[test]
    fn reusable_compressor_round_trips_multiple_payloads() {
        let mut compressor = None;
        for fill in [7_u8, 8] {
            let changes = vec![change(vec![fill; COMPRESS_THRESHOLD * 4])];
            let encoded = encode_changes_with_compressor(
                &changes,
                WalCompression::default(),
                Some(&mut compressor),
            )
            .unwrap();
            assert_eq!(encoded.flags, FLAG_PAYLOAD_COMPRESSED);
            assert_eq!(decode_changes(&encoded.bytes, true).unwrap(), changes);
        }
        assert!(compressor.is_some());
    }

    #[test]
    fn raised_compression_threshold_leaves_large_payload_uncompressed() {
        let changes = vec![change(vec![7_u8; COMPRESS_THRESHOLD * 4])];
        let encoded =
            encode_changes_with_compression(&changes, WalCompression::zstd(usize::MAX)).unwrap();
        assert_eq!(encoded.flags, 0);
        assert_eq!(decode_changes(&encoded.bytes, false).unwrap(), changes);
    }

    #[test]
    fn disabled_compression_leaves_large_payload_uncompressed() {
        let changes = vec![change(vec![7_u8; COMPRESS_THRESHOLD * 4])];
        let encoded =
            encode_changes_with_compression(&changes, WalCompression::disabled()).unwrap();
        assert_eq!(encoded.flags, 0);
        assert_eq!(decode_changes(&encoded.bytes, false).unwrap(), changes);
    }

    #[test]
    fn zero_compression_threshold_compresses_small_payload() {
        let changes = vec![change([1_u8, 2, 3])];
        let encoded = encode_changes_with_compression(&changes, WalCompression::zstd(0)).unwrap();
        assert_eq!(encoded.flags, FLAG_PAYLOAD_COMPRESSED);
        assert_eq!(decode_changes(&encoded.bytes, true).unwrap(), changes);
    }

    #[test]
    fn checksum_uses_on_disk_bytes() {
        let changes = vec![change(vec![8_u8; COMPRESS_THRESHOLD * 4])];
        let encoded = encode_changes(&changes).unwrap();
        assert_eq!(encoded.checksum_lo, checksum_lo(&encoded.bytes));
    }

    #[test]
    fn xxh3_checksum_is_stable() {
        assert_eq!(checksum_lo(b"selene"), 0xD795_2FA1);
    }

    #[test]
    fn checksum_mismatch_is_reported() {
        let changes = vec![change([1_u8])];
        let encoded = encode_changes(&changes).unwrap();
        let header = WalEntryHeader::new(
            encoded.bytes.len(),
            encoded.checksum_lo ^ 1,
            7,
            selene_core::HlcTimestamp::zero(),
            selene_core::Origin::Local,
            encoded.flags,
            None,
        )
        .unwrap();
        assert!(matches!(
            verify_checksum(&header, &encoded.bytes),
            Err(PersistError::ChecksumMismatch { sequence: 7 })
        ));
    }

    #[test]
    fn compression_threshold_boundary_is_inclusive_at_default() {
        // The `>= COMPRESS_THRESHOLD` gate in encode_changes keys on the encoded
        // (postcard) length, not the raw byte-buffer length. Grow the byte buffer
        // one at a time until the encoded length crosses threshold - 1 ->
        // threshold, then assert the boundary. This pins the off-by-one (`>` vs
        // `>=`) the codec tests far from the boundary can't.
        assert_eq!(COMPRESS_THRESHOLD, 4_096);

        let encoded_len = |buf_len: usize| -> usize {
            let changes = vec![change(vec![0_u8; buf_len])];
            postcard::to_stdvec(&changes).unwrap().len()
        };

        let find_buf_len = |target_len: usize| -> usize {
            for buf_len in 0..(target_len + 512) {
                if encoded_len(buf_len) == target_len {
                    return buf_len;
                }
            }
            panic!("a buffer encoding to exactly {target_len} bytes exists");
        };

        let below_len = COMPRESS_THRESHOLD - 1;
        let buf_below = find_buf_len(below_len);
        let buf_at = find_buf_len(COMPRESS_THRESHOLD);
        assert_eq!(
            encoded_len(buf_below),
            below_len,
            "below-threshold encoded payload"
        );
        assert_eq!(
            encoded_len(buf_at),
            COMPRESS_THRESHOLD,
            "threshold-byte encoded payload"
        );

        let below_threshold = vec![change(vec![0_u8; buf_below])];
        let enc_below = encode_changes(&below_threshold).unwrap();
        assert_eq!(
            enc_below.flags, 0,
            "below-threshold encoded payload stays uncompressed"
        );
        assert_eq!(
            decode_changes(&enc_below.bytes, false).unwrap(),
            below_threshold
        );

        let at_threshold = vec![change(vec![0_u8; buf_at])];
        let enc_at = encode_changes(&at_threshold).unwrap();
        assert_eq!(
            enc_at.flags, FLAG_PAYLOAD_COMPRESSED,
            "threshold-byte encoded payload is compressed (>= threshold)"
        );
        assert_eq!(decode_changes(&enc_at.bytes, true).unwrap(), at_threshold);
    }

    #[test]
    fn old_128_byte_payload_stays_uncompressed_by_default() {
        let encoded_len = |buf_len: usize| -> usize {
            let changes = vec![change(vec![0_u8; buf_len])];
            postcard::to_stdvec(&changes).unwrap().len()
        };
        let mut buf_at_128 = None;
        for buf_len in 0..512 {
            if encoded_len(buf_len) == 128 {
                buf_at_128 = Some(buf_len);
                break;
            }
        }
        let buf_128 = buf_at_128.expect("a buffer encoding to exactly 128 bytes exists");
        let at_128 = vec![change(vec![0_u8; buf_128])];
        let enc_128 = encode_changes(&at_128).unwrap();
        assert_eq!(
            enc_128.flags, 0,
            "128-byte encoded payload stays uncompressed after the threshold increase"
        );
        assert_eq!(decode_changes(&enc_128.bytes, false).unwrap(), at_128);
    }

    #[test]
    fn corrupt_compressed_bytes_report_compression_error() {
        let err = decode_changes(&[0, 1, 2, 3], true).unwrap_err();
        assert!(matches!(err, PersistError::Compression(_)));
    }

    #[test]
    fn bounded_decompress_rejects_oversized_output() {
        // Construct a small compressed payload that decompresses to
        // MAX_WAL_ENTRY_BYTES + 1, then assert decompress_bounded returns
        // PayloadTooLarge before runaway allocation. We use a highly-
        // compressible all-zero buffer so the compressed footprint stays
        // small while the decompressed footprint exceeds the cap.
        let oversize = MAX_WAL_ENTRY_BYTES + 1;
        let huge = vec![0_u8; oversize];
        let compressed =
            zstd::stream::encode_all(huge.as_slice(), 1).expect("zstd encode of test buffer");
        let err = decompress_zstd_bounded(&compressed, MAX_WAL_ENTRY_BYTES, |len, max| {
            PersistError::PayloadTooLarge { len, max }
        })
        .unwrap_err();
        match err {
            PersistError::PayloadTooLarge { len, max } => {
                assert!(len > max);
                assert_eq!(max, MAX_WAL_ENTRY_BYTES);
            }
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
    }

    proptest! {
        #[test]
        fn byte_payload_change_round_trips(payload in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let changes = vec![change(payload)];
            let encoded = encode_changes(&changes).unwrap();
            let decoded = decode_changes(&encoded.bytes, encoded.flags & FLAG_PAYLOAD_COMPRESSED != 0).unwrap();
            prop_assert_eq!(decoded, changes);
        }

        #[test]
        fn mixed_change_vec_round_trips(changes in proptest::collection::vec(change_strategy(), 0..64)) {
            let encoded = encode_changes(&changes).unwrap();
            let decoded = decode_changes(&encoded.bytes, encoded.flags & FLAG_PAYLOAD_COMPRESSED != 0).unwrap();
            prop_assert_eq!(decoded, changes);
        }
    }
}
