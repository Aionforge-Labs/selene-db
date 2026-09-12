//! Safe native handle-relative filesystem operations; no ambient child access.

use std::ffi::OsString;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};

use super::EntryMetadata;
use crate::PersistResult;

pub(super) fn supported() -> PersistResult<()> {
    Ok(())
}

pub(super) fn open_directory(path: &Path) -> PersistResult<File> {
    Ok(fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?
    .into())
}

pub(super) fn same_directory(a: &File, b: &File) -> PersistResult<bool> {
    let a = a.metadata()?;
    let b = b.metadata()?;
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

pub(super) fn open_file(
    dir: &File,
    name: &Path,
    write: bool,
    exclusive: bool,
) -> PersistResult<File> {
    // NONBLOCK prevents a swapped FIFO from hanging before fstat rejection;
    // NOCTTY avoids terminal acquisition. Neither alters regular-file I/O.
    let mut flags = OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY;
    flags |= if write { OFlags::RDWR } else { OFlags::RDONLY };
    if exclusive {
        flags |= OFlags::CREATE | OFlags::EXCL;
    }
    Ok(fs::openat(
        dir,
        name,
        flags,
        Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
    )
    .map_err(std::io::Error::from)?
    .into())
}

pub(super) fn is_regular(file: &File) -> std::io::Result<bool> {
    let metadata = file.metadata()?;
    Ok(metadata.is_file() && metadata.nlink() == 1)
}

pub(super) fn metadata(dir: &File, name: &Path) -> PersistResult<Option<EntryMetadata>> {
    let stat = match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    Ok(Some(EntryMetadata {
        regular: FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile,
        single_link: stat.st_nlink == 1,
        len: stat.st_size.max(0) as u64,
    }))
}

pub(super) fn entries(dir: &File, limit: usize) -> PersistResult<Vec<OsString>> {
    let entries = fs::Dir::read_from(dir).map_err(std::io::Error::from)?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(std::io::Error::from)?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            if names.len() == limit {
                return Err(crate::ControlError::TooLarge.into());
            }
            names.push(std::ffi::OsStr::from_bytes(bytes).to_os_string());
        }
    }
    Ok(names)
}

pub(super) fn rename(dir: &File, from: &Path, to: &Path) -> PersistResult<()> {
    fs::renameat(dir, from, dir, to).map_err(std::io::Error::from)?;
    Ok(())
}

pub(super) fn remove(dir: &File, name: &Path) -> PersistResult<()> {
    fs::unlinkat(dir, name, AtFlags::empty()).map_err(std::io::Error::from)?;
    Ok(())
}

pub(super) fn publish_new(dir: &File, from: &Path, to: &Path) -> PersistResult<()> {
    fs::renameat_with(dir, from, dir, to, fs::RenameFlags::NOREPLACE)
        .map_err(std::io::Error::from)?;
    Ok(())
}
