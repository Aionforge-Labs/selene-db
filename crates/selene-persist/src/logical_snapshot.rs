//! Format-2 immutable snapshot envelope, independent of legacy SLSN providers.
//! The semantic body belongs to catalog/graph. Hashes detect corruption, not forgery.

use crate::{
    control::{StoreEpoch, StoreId},
    logical_frame::FrameError,
    logical_stream::Position,
};
use selene_core::logical::{Budget, CodecError, Decoder, Encoder, Limits};

/// Fixed header, including its independent integrity digest.
pub const HEADER_LEN: usize = 168;
/// Header plus end marker and complete envelope digest.
pub const OVERHEAD: usize = HEADER_LEN + 40;
const MAGIC: &[u8; 8] = b"SLSNP2\0\0";
const END: &[u8; 8] = b"SLSNEND2";

/// Trusted checkpoint boundary supplied by selected control, not by image bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotContext {
    /// Exact WAL boundary from the same immutable outer publication.
    pub boundary: Position,
    /// Facade publication ordinal, not catalog or graph generation.
    pub publication: u64,
}

/// Encode one bounded full semantic body, with explicit fields and full integrity.
pub fn encode(body: &[u8], context: SnapshotContext, limit: usize) -> Result<Vec<u8>, CodecError> {
    if limit > Limits::default().bytes || body.len() > limit {
        return Err(CodecError::Limit);
    }
    validate_context(context)?;
    let mut bytes = header(body.len(), context)?;
    bytes
        .try_reserve(body.len().checked_add(40).ok_or(CodecError::Limit)?)
        .map_err(|_| CodecError::Limit)?;
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(END);
    let digest = *blake3::hash(&bytes).as_bytes();
    bytes.extend_from_slice(&digest);
    Ok(bytes)
}

fn header(length: usize, context: SnapshotContext) -> Result<Vec<u8>, CodecError> {
    let mut e = Encoder::new(Limits::default())?;
    e.fixed(MAGIC)?;
    e.u32(2)?; // major 2, minor 0, each little-endian u16
    e.u32(0)?; // flags/reserved
    e.u64(length as u64)?;
    e.u64(context.publication)?;
    let p = context.boundary;
    e.fixed(p.store.as_bytes())?;
    e.u64(p.epoch.get())?;
    e.fixed(&p.segment)?;
    e.u64(p.sequence)?;
    e.u64(p.offset)?;
    e.fixed(&p.digest)?;
    let mut bytes = e.finish();
    bytes.extend_from_slice(blake3::hash(&bytes).as_bytes());
    Ok(bytes)
}

/// Validate a fixed header before reading or allocating its full payload.
/// Returns the exact complete file length. Unknown versions and reserved bits fail.
pub fn required_length(
    bytes: &[u8],
    expected: SnapshotContext,
    limit: usize,
) -> Result<usize, FrameError> {
    validate_context(expected)?;
    if limit > Limits::default().bytes {
        return Err(FrameError::Limit);
    }
    let header = bytes
        .get(..HEADER_LEN)
        .ok_or(FrameError::CorruptIncomplete)?;
    if &header[..8] != MAGIC {
        return Err(FrameError::Invalid("snapshot magic"));
    }
    if blake3::hash(&header[..136]).as_bytes() != &header[136..] {
        return Err(FrameError::Integrity("snapshot header"));
    }
    let mut budget = Budget::new(Limits::default())?;
    let mut d = Decoder::new(&header[..136], &mut budget)?;
    if d.take(8)? != MAGIC {
        return Err(FrameError::Invalid("snapshot magic"));
    }
    if d.u32()? != 2 {
        return Err(FrameError::Unsupported("snapshot version"));
    }
    if d.u32()? != 0 {
        return Err(FrameError::Invalid("snapshot reserved"));
    }
    let length = usize::try_from(d.u64()?).map_err(|_| CodecError::Limit)?;
    if length > limit {
        return Err(FrameError::Limit);
    }
    let publication = d.u64()?;
    let store = StoreId::from_bytes(d.take(16)?.try_into().expect("fixed width"))
        .map_err(|_| CodecError::Semantic)?;
    let epoch = StoreEpoch::new(d.u64()?).map_err(|_| CodecError::Semantic)?;
    let boundary = Position {
        store,
        epoch,
        segment: d.take(32)?.try_into().expect("fixed width"),
        sequence: d.u64()?,
        offset: d.u64()?,
        digest: d.take(32)?.try_into().expect("fixed width"),
    };
    d.finish()?;
    if boundary.store != expected.boundary.store {
        return Err(FrameError::Store);
    }
    if boundary.epoch != expected.boundary.epoch {
        return Err(FrameError::Epoch);
    }
    if boundary.segment != expected.boundary.segment {
        return Err(FrameError::Segment);
    }
    if boundary.digest != expected.boundary.digest {
        return Err(FrameError::Origin);
    }
    if boundary.sequence != expected.boundary.sequence
        || boundary.offset != expected.boundary.offset
        || publication != expected.publication
    {
        return Err(FrameError::SnapshotBoundary);
    }
    length.checked_add(OVERHEAD).ok_or(FrameError::Limit)
}

/// Verify exactly one whole snapshot and its selected descriptor digest before decoding.
/// Additional bytes, missing bytes and corrupt required payloads are never salvageable.
pub fn decode<'a>(
    bytes: &'a [u8],
    expected: SnapshotContext,
    digest: &[u8; 32],
    limit: usize,
) -> Result<&'a [u8], FrameError> {
    let length = required_length(bytes, expected, limit)?;
    if bytes.len() < length {
        return Err(FrameError::CorruptIncomplete);
    }
    if bytes.len() > length {
        return Err(FrameError::Invalid("snapshot file length"));
    }
    if &bytes[length - 40..length - 32] != END
        || &bytes[length - 32..] != digest
        || blake3::hash(&bytes[..length - 32]).as_bytes() != digest
    {
        return Err(FrameError::Integrity("snapshot complete"));
    }
    Ok(&bytes[HEADER_LEN..length - 40])
}

fn validate_context(context: SnapshotContext) -> Result<(), CodecError> {
    let p = context.boundary;
    // A rotated empty segment can cover a nonzero global sequence. The selected
    // control validates offset/sequence/digest against its trusted segment base;
    // the envelope must exactly match that externally supplied context.
    if p.segment == [0; 32] || p.digest == [0; 32] || (p.sequence == 0 && p.offset != 0) {
        return Err(CodecError::Invalid("snapshot context"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independently_constructed_snapshot_fields_and_corruption() {
        let expected = SnapshotContext {
            boundary: Position {
                store: StoreId::from_bytes([0, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 1])
                    .unwrap(),
                epoch: StoreEpoch::new(7).unwrap(),
                segment: [9; 32],
                sequence: 13,
                offset: 2048,
                digest: [17; 32],
            },
            publication: 19,
        };
        let bytes = encode(b"semantic image", expected, 1024).unwrap();
        assert_eq!(&bytes[..8], b"SLSNP2\0\0");
        assert_eq!(&bytes[8..16], &[2, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[16..24], &14_u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &19_u64.to_le_bytes());
        assert_eq!(&bytes[88..96], &13_u64.to_le_bytes());
        assert_eq!(&bytes[96..104], &2048_u64.to_le_bytes());
        let digest = bytes[bytes.len() - 32..].try_into().unwrap();
        assert_eq!(
            decode(&bytes, expected, &digest, 1024).unwrap(),
            b"semantic image"
        );
        for i in 0..bytes.len() {
            let mut corrupt = bytes.clone();
            corrupt[i] ^= 1;
            assert!(
                decode(&corrupt, expected, &digest, 1024).is_err(),
                "byte {i}"
            );
            assert!(
                decode(&bytes[..i], expected, &digest, 1024).is_err(),
                "cut {i}"
            );
        }
        let mut foreign = expected;
        foreign.boundary.offset += 1;
        assert!(decode(&bytes, foreign, &digest, 1024).is_err());
        assert_eq!(
            decode(&bytes, expected, &digest, 13).unwrap_err(),
            FrameError::Limit
        );
    }
}
