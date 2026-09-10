//! Bounded postcard control envelopes, independent of filesystem/platform support.

use super::{CurrentSelector, EmptyManifest};
use crate::{ControlError, PersistResult};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Maximum complete encoded control record, including envelope and checksum.
pub const MAX_CONTROL_BYTES: usize = 4096;
const VERSION: u16 = 1;

#[derive(Serialize, Deserialize)]
struct Envelope<'a> {
    magic: [u8; 4],
    version: u16,
    #[serde(borrow)]
    body: &'a [u8],
    digest: [u8; 32],
}

fn encode<T: Serialize>(value: &T, magic: [u8; 4]) -> PersistResult<Vec<u8>> {
    let body = postcard::to_stdvec(value).map_err(|_| ControlError::Envelope("encode"))?;
    let envelope = Envelope {
        magic,
        version: VERSION,
        digest: *blake3::hash(&body).as_bytes(),
        body: &body,
    };
    let bytes =
        postcard::to_stdvec(&envelope).map_err(|_| ControlError::Envelope("encode envelope"))?;
    if bytes.len() > MAX_CONTROL_BYTES {
        return Err(ControlError::TooLarge.into());
    }
    Ok(bytes)
}

fn decode<T: DeserializeOwned>(bytes: &[u8], magic: [u8; 4]) -> PersistResult<T> {
    if bytes.len() > MAX_CONTROL_BYTES {
        return Err(ControlError::TooLarge.into());
    }
    let (envelope, tail): (Envelope<'_>, _) =
        postcard::take_from_bytes(bytes).map_err(|_| ControlError::Envelope("framing"))?;
    if !tail.is_empty() || envelope.magic != magic {
        return Err(ControlError::Envelope("magic or trailing bytes").into());
    }
    if envelope.version != VERSION {
        return Err(ControlError::UnsupportedVersion.into());
    }
    if *blake3::hash(envelope.body).as_bytes() != envelope.digest {
        return Err(ControlError::Checksum.into());
    }
    let (value, tail) =
        postcard::take_from_bytes(envelope.body).map_err(|_| ControlError::Envelope("payload"))?;
    if !tail.is_empty() {
        return Err(ControlError::Envelope("payload trailing bytes").into());
    }
    Ok(value)
}

impl EmptyManifest {
    /// Encode a validated version-1 empty-control envelope (not a transaction format).
    ///
    /// # Errors
    /// Returns identity, version, lineage, size, or serialization errors.
    pub fn encode(&self) -> PersistResult<Vec<u8>> {
        self.validate()?;
        encode(self, *b"SLEM")
    }

    /// Decode bounded untrusted bytes without any filesystem or engine access.
    ///
    /// # Errors
    /// Rejects malformed, oversized, foreign-version, checksum, or invalid records.
    pub fn decode(bytes: &[u8]) -> PersistResult<Self> {
        let value: Self = decode(bytes, *b"SLEM")?;
        value.validate()?;
        Ok(value)
    }
}

impl CurrentSelector {
    /// Encode a validated immutable-manifest selector.
    ///
    /// # Errors
    /// Returns invalid-identity/name, size, or serialization errors.
    pub fn encode(&self) -> PersistResult<Vec<u8>> {
        self.validate()?;
        encode(self, *b"SLCU")
    }

    /// Decode a bounded selector, validating its child name before any file access.
    ///
    /// # Errors
    /// Returns envelope, version, checksum, size, identity, or child-name errors.
    pub fn decode(bytes: &[u8]) -> PersistResult<Self> {
        let value: Self = decode(bytes, *b"SLCU")?;
        value.validate()?;
        Ok(value)
    }
}
