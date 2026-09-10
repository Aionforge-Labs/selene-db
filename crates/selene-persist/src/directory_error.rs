//! Filesystem authority failures, independent of byte codecs.

use std::path::PathBuf;

/// A storage mode or managed directory entry violates the authority contract.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DirectoryError {
    /// Native handle-relative storage is supported only on Linux and macOS.
    #[error("native store directories are unsupported on this platform")]
    UnsupportedPlatform,
    /// A supplied handle does not identify a directory.
    #[error("store capability is not a directory")]
    NotDirectory,
    /// Managed names must be nonempty single components, without portable separators.
    #[error("invalid managed child name: {0:?}")]
    InvalidName(PathBuf),
    /// A managed entry is a symlink, special file, or unsupported hard link.
    #[error("managed entry is not an unaliased regular file: {0}")]
    NotRegular(PathBuf),
    /// Permanent coordination entries cannot be removed or replaced.
    #[error("cannot remove or replace persistent coordination entry: {0}")]
    CoordinationEntry(PathBuf),
    /// Legacy data APIs cannot consume or mutate a format-2 control directory.
    #[error("legacy persistence cannot access a format-2 control directory")]
    ControlDirectory,
    /// A replace became visible but its final synchronization failed.
    #[error("artifact publication is uncertain; reopen: {source}")]
    PublicationUncertain {
        /// Failure after atomic replacement.
        source: Box<crate::PersistError>,
    },
    /// Further mutation is fenced after uncertain publication.
    #[error("artifact writer requires reopen after uncertain publication")]
    RequiresReopen,
}
