//! Explicit unsupported-platform boundary. Pure byte codecs remain usable.

use super::EntryMetadata;
use crate::{DirectoryError, PersistResult};
use std::{ffi::OsString, fs::File, path::Path};

fn unsupported<T>() -> PersistResult<T> {
    Err(DirectoryError::UnsupportedPlatform.into())
}
pub(super) fn supported() -> PersistResult<()> {
    unsupported()
}
pub(super) fn open_directory(_: &Path) -> PersistResult<File> {
    unsupported()
}
pub(super) fn same_directory(_: &File, _: &File) -> PersistResult<bool> {
    unsupported()
}
pub(super) fn open_file(_: &File, _: &Path, _: bool, _: bool) -> PersistResult<File> {
    unsupported()
}
pub(super) fn is_regular(_: &File) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "unsupported native store platform",
    ))
}
pub(super) fn metadata(_: &File, _: &Path) -> PersistResult<Option<EntryMetadata>> {
    unsupported()
}
pub(super) fn entries(_: &File, _: usize) -> PersistResult<Vec<OsString>> {
    unsupported()
}
pub(super) fn rename(_: &File, _: &Path, _: &Path) -> PersistResult<()> {
    unsupported()
}
pub(super) fn remove(_: &File, _: &Path) -> PersistResult<()> {
    unsupported()
}
pub(super) fn publish_new(_: &File, _: &Path, _: &Path) -> PersistResult<()> {
    unsupported()
}
