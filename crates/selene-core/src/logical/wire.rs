//! Checked fixed-width wire fields and cumulative transaction accounting.

use super::{CodecError as E, CodecResult};

/// Absolute format-2 encoded and expanded payload ceiling (256 MiB).
pub const MAX_PAYLOAD: usize = 256 * 1024 * 1024;

/// Resource limits; callers may tighten but cannot raise the format ceilings.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum encoded or expanded payload bytes.
    pub bytes: usize,
    /// Conservative cumulative decoded allocation charge, including temporaries.
    pub allocation: usize,
    /// Maximum total container entries and semantic objects.
    pub items: usize,
    /// Maximum metadata entries (also bounds quadratic owner validation work).
    pub metadata: usize,
    /// Maximum recursive stored value/descriptor depth, including root.
    pub depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: MAX_PAYLOAD,
            allocation: MAX_PAYLOAD,
            items: 1_048_576,
            metadata: 4096,
            depth: crate::MAX_STORED_VALUE_DEPTH,
        }
    }
}

impl Limits {
    /// Validate a tightening of the default ceilings.
    pub fn validate(self) -> CodecResult<Self> {
        let ceiling = Self::default();
        if self.bytes > ceiling.bytes
            || self.allocation > ceiling.allocation
            || self.items > ceiling.items
            || self.metadata > ceiling.metadata
            || self.depth > ceiling.depth
        {
            return Err(E::Limit);
        }
        Ok(self)
    }
}

/// Cumulative accounting shared by all sections of one logical transaction.
#[derive(Clone, Debug)]
pub struct Budget {
    limits: Limits,
    allocation: usize,
    items: usize,
    metadata: usize,
}

impl Budget {
    /// Start an accounting domain. Nested decoders must reuse it.
    pub fn new(limits: Limits) -> CodecResult<Self> {
        Ok(Self {
            limits: limits.validate()?,
            allocation: 0,
            items: 0,
            metadata: 0,
        })
    }

    /// Charge before allocating or materializing count-controlled objects.
    pub fn charge(&mut self, items: usize, allocation: usize) -> CodecResult<()> {
        self.items = self.items.checked_add(items).ok_or(E::Limit)?;
        self.allocation = self.allocation.checked_add(allocation).ok_or(E::Limit)?;
        if self.items > self.limits.items || self.allocation > self.limits.allocation {
            return Err(E::Limit);
        }
        Ok(())
    }

    /// Charge metadata work before invoking an owning catalog/schema validator.
    pub fn metadata(&mut self, count: usize) -> CodecResult<()> {
        self.metadata = self.metadata.checked_add(count).ok_or(E::Limit)?;
        if self.metadata > self.limits.metadata {
            return Err(E::Limit);
        }
        Ok(())
    }

    /// Reject recursion before entering/materializing the next level.
    pub fn depth(&self, depth: usize) -> CodecResult<()> {
        if depth > self.limits.depth {
            Err(E::Limit)
        } else {
            Ok(())
        }
    }

    /// Conservative cumulative allocation charge, not allocator instrumentation.
    pub const fn allocation_charge(&self) -> usize {
        self.allocation
    }
    /// Number of semantic entries charged so far.
    pub const fn item_charge(&self) -> usize {
        self.items
    }
    pub(super) fn json_limit(&self) -> usize {
        self.limits
            .bytes
            .min(self.limits.items.saturating_sub(self.items))
            .min(self.limits.allocation.saturating_sub(self.allocation) / 136)
    }
}

/// Payload encoder using explicit little-endian fields, not Rust layout.
pub struct Encoder {
    bytes: Vec<u8>,
    length: usize,
    retain: bool,
    /// Transaction-wide resource budget.
    pub budget: Budget,
}

impl Encoder {
    /// Start an empty bounded payload.
    pub fn new(limits: Limits) -> CodecResult<Self> {
        Ok(Self {
            bytes: Vec::new(),
            length: 0,
            retain: true,
            budget: Budget::new(limits)?,
        })
    }
    /// Traverse the same semantic codec and accounting without retaining output.
    /// Used to charge retained catalog state before isolated replay clones it.
    pub fn counting(budget: Budget) -> Self {
        Self {
            bytes: Vec::new(),
            length: 0,
            retain: false,
            budget,
        }
    }
    /// Append an already-defined wire field after a checked length calculation.
    pub fn fixed(&mut self, bytes: &[u8]) -> CodecResult<()> {
        let length = self.length.checked_add(bytes.len()).ok_or(E::Limit)?;
        if length > self.budget.limits.bytes {
            return Err(E::Limit);
        }
        if self.retain {
            self.bytes.try_reserve(bytes.len()).map_err(|_| E::Limit)?;
            self.bytes.extend_from_slice(bytes);
        }
        self.length = length;
        Ok(())
    }
    /// Encode one explicit tag or byte.
    pub fn u8(&mut self, value: u8) -> CodecResult<()> {
        self.fixed(&[value])
    }
    /// Encode a canonical boolean (zero or one).
    pub fn boolean(&mut self, value: bool) -> CodecResult<()> {
        self.u8(u8::from(value))
    }
    /// Encode a fixed-width little-endian integer.
    pub fn u32(&mut self, value: u32) -> CodecResult<()> {
        self.fixed(&value.to_le_bytes())
    }
    /// Encode a fixed-width little-endian integer.
    pub fn u64(&mut self, value: u64) -> CodecResult<()> {
        self.fixed(&value.to_le_bytes())
    }
    /// Encode a count, charging a conservative 256 bytes per decoded entry.
    pub fn count(&mut self, count: usize) -> CodecResult<()> {
        self.count_for::<[u8; 64]>(count)
    }
    /// Encode a collection count after charging at least four resident elements
    /// per entry for materialization/validation copies. Rust sizes affect only
    /// memory admission, never the wire representation (always a u32 count).
    pub fn count_for<T>(&mut self, count: usize) -> CodecResult<()> {
        let charge = std::mem::size_of::<T>().saturating_mul(4).max(256);
        self.budget
            .charge(count, count.checked_mul(charge).ok_or(E::Limit)?)?;
        self.u32(u32::try_from(count).map_err(|_| E::Limit)?)
    }
    /// Encode length-prefixed bytes, charging eight times their size for copies.
    pub fn blob(&mut self, value: &[u8]) -> CodecResult<()> {
        self.budget
            .charge(0, value.len().checked_mul(8).ok_or(E::Limit)?)?;
        self.u32(u32::try_from(value.len()).map_err(|_| E::Limit)?)?;
        self.fixed(value)
    }
    /// Encode exact UTF-8 text, without normalization or intern IDs.
    pub fn text(&mut self, value: &str) -> CodecResult<()> {
        self.blob(value.as_bytes())
    }
    /// Finish an owned payload. No partial buffer escapes an encoding error.
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Borrowed checked payload cursor; owns no buffer and shares a transaction budget.
pub struct Decoder<'a, 'b> {
    bytes: &'a [u8],
    /// Transaction-wide resource budget.
    pub budget: &'b mut Budget,
}

impl<'a, 'b> Decoder<'a, 'b> {
    /// Begin decoding one bounded section; the caller retains its backing bytes.
    pub fn new(bytes: &'a [u8], budget: &'b mut Budget) -> CodecResult<Self> {
        if bytes.len() > budget.limits.bytes {
            return Err(E::Limit);
        }
        Ok(Self { bytes, budget })
    }
    /// Borrow a fixed-width field, validating before slicing.
    pub fn take(&mut self, len: usize) -> CodecResult<&'a [u8]> {
        let (field, rest) = self.bytes.split_at_checked(len).ok_or(E::Incomplete)?;
        self.bytes = rest;
        Ok(field)
    }
    /// Read a byte/tag.
    pub fn u8(&mut self) -> CodecResult<u8> {
        Ok(self.take(1)?[0])
    }
    /// Read a canonical zero/one boolean.
    pub fn boolean(&mut self) -> CodecResult<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(E::Invalid("boolean")),
        }
    }
    /// Read a fixed little-endian integer.
    pub fn u32(&mut self) -> CodecResult<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four checked bytes"),
        ))
    }
    /// Read a fixed little-endian integer.
    pub fn u64(&mut self) -> CodecResult<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight checked bytes"),
        ))
    }
    /// Read an entry count and charge before any reserve/recursive materialization.
    pub fn count(&mut self) -> CodecResult<usize> {
        self.count_for::<[u8; 64]>()
    }
    /// Decode a count, accounting for the actual resident element type before
    /// reserve. Large legacy in-memory carriers do not evade the memory ceiling.
    pub fn count_for<T>(&mut self) -> CodecResult<usize> {
        let count = self.u32()? as usize;
        // Every format-2 collection entry consumes at least one wire byte.
        if count > self.bytes.len() {
            return Err(E::Incomplete);
        }
        let charge = std::mem::size_of::<T>().saturating_mul(4).max(256);
        self.budget
            .charge(count, count.checked_mul(charge).ok_or(E::Limit)?)?;
        Ok(count)
    }
    /// Borrow length-prefixed bytes after accounting for owned copies.
    pub fn blob(&mut self) -> CodecResult<&'a [u8]> {
        let len = self.u32()? as usize;
        if len > self.bytes.len() {
            return Err(E::Incomplete);
        }
        self.budget.charge(0, len.checked_mul(8).ok_or(E::Limit)?)?;
        self.take(len)
    }
    /// Borrow validated UTF-8 text; no allocation occurs here.
    pub fn text(&mut self) -> CodecResult<&'a str> {
        std::str::from_utf8(self.blob()?).map_err(|_| E::Invalid("UTF-8"))
    }
    /// Reject trailing bytes at the end of a complete payload or section.
    pub fn finish(self) -> CodecResult<()> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(E::Invalid("trailing bytes"))
        }
    }
}
