//! Format-2 transaction framing, independent of SLDBWAL legacy codecs and I/O.
//!
//! BLAKE3 detects corruption; these unkeyed digests do not authenticate a writer
//! or prove the absence of rollback. The caller supplies trusted stream context.

use crate::control::{StoreEpoch, StoreId};
use std::{
    borrow::Cow,
    io::{Read, Write},
};

/// New format magic; never dispatched to the legacy WAL decoder.
pub const MAGIC: [u8; 8] = *b"SLTXN2\0\0";
/// Fixed header: validated fields (128 bytes) and their BLAKE3 digest (32 bytes).
pub const HEADER_LEN: usize = 160;
/// Trailer: end marker and BLAKE3 of header, encoded body and end marker.
pub const TRAILER_LEN: usize = 40;
/// Per-frame fixed overhead independent of compression.
pub const FRAME_OVERHEAD: usize = HEADER_LEN + TRAILER_LEN;
/// Maximum encoded or expanded transaction body.
pub const MAX_PAYLOAD: usize = 256 * 1024 * 1024;
/// Independent Zstd history ceiling, eight MiB (2^23).
pub const WINDOW_LOG_MAX: u32 = 23;
const END: &[u8; 8] = b"SLTXEND2";

/// Explicit compression policy; automatic level one keeps RAW if it is not smaller.
#[derive(Clone, Copy, Debug, Default)]
pub enum Compression {
    /// Never compress.
    Raw,
    /// Try Zstd level one for bodies of at least 4096 bytes.
    #[default]
    Auto,
}

/// Trusted expected identity, sequence and lineage, supplied by the stream owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Context {
    /// Durable store UUID, not the facade's process-local DatabaseId.
    pub store: StoreId,
    /// Durable nonzero store epoch.
    pub epoch: StoreEpoch,
    /// Exact next nonzero sequence, with no gaps.
    pub sequence: u64,
    /// Trusted selected segment/manifest anchor; not inferred from these bytes.
    pub segment: [u8; 32],
    /// Prior complete record digest, or the trusted initial lineage anchor.
    pub previous: [u8; 32],
}

/// Trusted location of the supplied input's end, not a guess based on EOF alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Boundary {
    /// Incomplete bytes at the end of the currently unsealed segment may be a torn tail.
    UnsealedEnd,
    /// The selected segment has a trusted sealed end boundary.
    SealedEnd,
    /// Bytes end before a known later authoritative record or sealed boundary.
    Interior,
}

/// Framing error. No variant authorizes file truncation or salvage.
#[derive(Debug, Clone, Copy, Eq, PartialEq, thiserror::Error)]
pub enum FrameError {
    /// Structural corruption or noncanonical frame content.
    #[error("invalid format-2 frame: {0}")]
    Invalid(&'static str),
    /// Store, epoch, exact sequence, or lineage disagrees with trusted context.
    #[error("format-2 stream context mismatch")]
    Context,
    /// An intact common envelope selects an unsupported version or codec.
    #[error("unsupported format-2 frame: {0}")]
    Unsupported(&'static str),
    /// Header or complete-record checksum failure; never a harmless torn tail.
    #[error("format-2 integrity failure: {0}")]
    Integrity(&'static str),
    /// Durable store identity differs from the selected context.
    #[error("foreign format-2 store")]
    Store,
    /// Store epoch differs from the selected context.
    #[error("foreign format-2 epoch")]
    Epoch,
    /// Segment anchor differs from selected control.
    #[error("foreign format-2 segment")]
    Segment,
    /// Previous-record or origin digest differs from the trusted boundary.
    #[error("format-2 digest lineage mismatch")]
    Origin,
    /// An exact sequence gap or overlap, with bounded numeric evidence.
    #[error("format-2 sequence mismatch: expected {expected}, observed {observed}")]
    Sequence {
        /// Trusted next sequence.
        expected: u64,
        /// Header sequence after header-integrity validation.
        observed: u64,
    },
    /// Snapshot sequence, offset or publication differs from selected control.
    #[error("format-2 checkpoint boundary mismatch")]
    SnapshotBoundary,
    /// Fixed-field decoding failed; retains its owning typed cause.
    #[error(transparent)]
    Payload(#[from] selene_core::logical::CodecError),
    /// Encoded or expanded bytes exceed the configured ceiling.
    #[error("format-2 frame resource limit")]
    Limit,
    /// Incomplete bytes where the caller knows a complete authoritative frame is required.
    #[error("incomplete sealed or interior format-2 frame")]
    CorruptIncomplete,
    /// Zstd rejected a frame, including excessive history or invalid compressed bytes.
    #[error("invalid bounded Zstd frame")]
    Compression,
}

/// A validated frame or a specifically classified incomplete unsealed suffix.
#[derive(Debug)]
pub enum Decoded<'a> {
    /// Full encoded integrity is validated before decompression and semantic decoding.
    Complete {
        /// RAW borrows input; Zstd owns precisely the expanded body, without spare input buffers.
        body: Cow<'a, [u8]>,
        /// Complete-record digest for the next record's lineage expectation.
        digest: [u8; 32],
        /// Total bytes consumed; the caller may continue at this exact next boundary.
        consumed: usize,
    },
    /// Only valid at an explicitly unsealed final boundary; no repair is performed.
    Incomplete {
        /// Minimum total bytes needed to continue fixed-header or complete-frame validation.
        needed: usize,
    },
}

/// Encode one already-bounded complete logical transaction. This does not append or sync.
pub fn encode(
    body: &[u8],
    context: Context,
    compression: Compression,
    limit: usize,
) -> Result<Vec<u8>, FrameError> {
    check_context(context)?;
    if limit > MAX_PAYLOAD || body.len() > limit {
        return Err(FrameError::Limit);
    }
    let compressed = if matches!(compression, Compression::Auto) && body.len() >= 4096 {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1)
            .map_err(|_| FrameError::Compression)?;
        encoder
            .window_log(WINDOW_LOG_MAX)
            .map_err(|_| FrameError::Compression)?;
        encoder
            .set_pledged_src_size(Some(body.len() as u64))
            .map_err(|_| FrameError::Compression)?;
        encoder
            .write_all(body)
            .map_err(|_| FrameError::Compression)?;
        let bytes = encoder.finish().map_err(|_| FrameError::Compression)?;
        (bytes.len() < body.len()).then_some(bytes)
    } else {
        None
    };
    let encoded = compressed.as_deref().unwrap_or(body);
    let mut result = Vec::with_capacity(FRAME_OVERHEAD + encoded.len());
    result.extend_from_slice(&MAGIC);
    result.extend_from_slice(&2u16.to_le_bytes());
    result.extend_from_slice(&0u16.to_le_bytes());
    result.extend_from_slice(&[u8::from(compressed.is_some()), 0, 0, 0]);
    result.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
    result.extend_from_slice(&(body.len() as u64).to_le_bytes());
    result.extend_from_slice(&context.sequence.to_le_bytes());
    result.extend_from_slice(context.store.as_bytes());
    result.extend_from_slice(&context.epoch.get().to_le_bytes());
    result.extend_from_slice(&context.segment);
    result.extend_from_slice(&context.previous);
    result.extend_from_slice(blake3::hash(&result).as_bytes());
    result.extend_from_slice(encoded);
    result.extend_from_slice(END);
    result.extend_from_slice(blake3::hash(&result).as_bytes());
    Ok(result)
}

/// Decode one frame using explicit trusted end classification. Complete checksum failure
/// is always corruption, even when there are no later bytes. Header integrity and lengths
/// are checked before output allocation; encoded integrity is checked before Zstd.
pub fn decode<'a>(
    bytes: &'a [u8],
    context: Context,
    boundary: Boundary,
    limit: usize,
) -> Result<Decoded<'a>, FrameError> {
    check_context(context)?;
    if limit > MAX_PAYLOAD {
        return Err(FrameError::Limit);
    }
    if bytes.get(..8).unwrap_or(bytes) != &MAGIC[..bytes.len().min(8)] {
        return Err(FrameError::Invalid("magic"));
    }
    if bytes.len() < HEADER_LEN {
        return incomplete(boundary, HEADER_LEN);
    }
    let header = &bytes[..HEADER_LEN];
    if blake3::hash(&header[..128]).as_bytes() != &header[128..160] {
        return Err(FrameError::Integrity("header"));
    }
    if header[8..12] != [2, 0, 0, 0] {
        return Err(FrameError::Unsupported("version"));
    }
    if header[12] > 1 {
        return Err(FrameError::Unsupported("codec"));
    }
    if header[13..16] != [0; 3] {
        return Err(FrameError::Invalid("reserved"));
    }
    let encoded = usize::try_from(number(header, 16)).map_err(|_| FrameError::Limit)?;
    let expanded = usize::try_from(number(header, 24)).map_err(|_| FrameError::Limit)?;
    if encoded > limit || expanded > limit {
        return Err(FrameError::Limit);
    }
    if header[12] == 0 && encoded != expanded {
        return Err(FrameError::Invalid("raw length"));
    }
    if header[40..56] != *context.store.as_bytes() {
        return Err(FrameError::Store);
    }
    if number(header, 56) != context.epoch.get() {
        return Err(FrameError::Epoch);
    }
    if header[64..96] != context.segment {
        return Err(FrameError::Segment);
    }
    if number(header, 32) != context.sequence {
        return Err(FrameError::Sequence {
            expected: context.sequence,
            observed: number(header, 32),
        });
    }
    if header[96..128] != context.previous {
        return Err(FrameError::Origin);
    }
    let total = encoded
        .checked_add(FRAME_OVERHEAD)
        .ok_or(FrameError::Limit)?;
    if bytes.len() < total {
        return incomplete(boundary, total);
    }
    let trailer = HEADER_LEN + encoded;
    if &bytes[trailer..trailer + 8] != END {
        return Err(FrameError::Invalid("trailer"));
    }
    let digest = blake3::hash(&bytes[..trailer + 8]);
    if digest.as_bytes() != &bytes[trailer + 8..total] {
        return Err(FrameError::Integrity("record"));
    }
    let payload = &bytes[HEADER_LEN..trailer];
    let body = if header[12] == 0 {
        Cow::Borrowed(payload)
    } else {
        Cow::Owned(expand(payload, expanded)?)
    };
    Ok(Decoded::Complete {
        body,
        digest: *digest.as_bytes(),
        consumed: total,
    })
}

fn check_context(context: Context) -> Result<(), FrameError> {
    if context.sequence == 0 || context.epoch.get() == 0 {
        return Err(FrameError::Context);
    }
    StoreId::from_bytes(*context.store.as_bytes()).map_err(|_| FrameError::Context)?;
    Ok(())
}
fn number(header: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        header[offset..offset + 8]
            .try_into()
            .expect("validated fixed header"),
    )
}
fn incomplete<'a>(boundary: Boundary, needed: usize) -> Result<Decoded<'a>, FrameError> {
    if boundary == Boundary::UnsealedEnd {
        Ok(Decoded::Incomplete { needed })
    } else {
        Err(FrameError::CorruptIncomplete)
    }
}
fn expand(bytes: &[u8], expected: usize) -> Result<Vec<u8>, FrameError> {
    // Refuse skippable frames and dictionary-bearing envelopes. No dictionary service.
    if bytes.get(..4) != Some(&[0x28, 0xb5, 0x2f, 0xfd]) || bytes.get(4).is_none_or(|b| b & 3 != 0)
    {
        return Err(FrameError::Compression);
    }
    let mut decoder = zstd::stream::read::Decoder::with_buffer(bytes)
        .map_err(|_| FrameError::Compression)?
        .single_frame();
    decoder
        .window_log_max(WINDOW_LOG_MAX)
        .map_err(|_| FrameError::Compression)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(expected)
        .map_err(|_| FrameError::Limit)?;
    let mut chunk = [0; 8192];
    loop {
        let count = decoder
            .read(&mut chunk)
            .map_err(|_| FrameError::Compression)?;
        if count == 0 {
            break;
        }
        if count > expected.saturating_sub(output.len()) {
            return Err(FrameError::Limit);
        }
        output.extend_from_slice(&chunk[..count]);
    }
    if output.len() != expected || !decoder.finish().is_empty() {
        return Err(FrameError::Invalid("Zstd consumption/length"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests;
