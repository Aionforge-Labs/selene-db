//! Format-2 semantic payload codec. Independent of legacy serde/rkyv layouts.
//!
//! This module performs no I/O or publication. The envelope belongs to persist;
//! catalog semantics and graph validation remain with their owning crates.

mod graph;
mod schema;
mod tags;
mod value;
mod wire;

pub use graph::GraphDelta;
pub use schema::GraphDefinition;

pub use value::{decode_value, encode_value};
pub use wire::{Budget, Decoder, Encoder, Limits};

/// A format-2 payload failure, without including potentially private payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CodecError {
    /// Bytes end before the declared semantic payload is complete.
    #[error("incomplete logical payload")]
    Incomplete,
    /// A configured resource ceiling was exceeded.
    #[error("logical codec resource limit")]
    Limit,
    /// An unknown authoritative tag, reserved bit, or noncanonical representation.
    #[error("invalid logical representation: {0}")]
    Invalid(&'static str),
    /// An owning semantic validator rejected the decoded value or declaration.
    #[error("invalid logical semantics")]
    Semantic,
}

/// Result from a format-2 semantic codec.
pub type CodecResult<T> = Result<T, CodecError>;

#[cfg(test)]
mod tests;
