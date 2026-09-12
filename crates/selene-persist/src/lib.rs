//! Format-2 persistence beneath graph semantics.
//!
//! Retained directory/writer capabilities, bounded control and logical frames,
//! snapshots, artifact leases and explicit retention. Old artifacts are recognized
//! by a read-only header probe only: no legacy decoder, audit log or migration.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod control;
mod control_error;
mod directory_error;
pub mod error;
mod legacy_probe;
pub mod logical_frame;
pub mod logical_snapshot;
pub mod logical_stream;
mod manifest_lock;
mod store_directory;

pub use control_error::ControlError;
pub use directory_error::DirectoryError;
pub use error::{PersistArtifact, PersistError, PersistResult};
pub use manifest_lock::{MANIFEST_LOCK_FILE_NAME, PersistenceReadGuard};
pub use store_directory::{STORE_LOCK_FILE_NAME, StoreDirectory, StoreWriter};
