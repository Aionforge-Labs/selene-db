//! Handle-relative active WAL opening and atomic initialization.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::file_header::WalFileHeader;
use crate::{DirectoryError, PersistError, PersistResult, StoreDirectory, StoreWriter};

const OPEN_RACE_RETRIES: usize = 8;
const INIT_TEMP_RETRIES: usize = 32;
static WAL_INIT_NONCE: AtomicU64 = AtomicU64::new(0);

/// Open and exclusively lock a WAL through its canonical parent directory.
///
/// Parent aliases are resolved once before the final component is inspected.
/// Existing final entries must be regular files. An absent WAL is initialized
/// and fsynced under a unique sibling name, then hard-linked into the final path
/// with fail-on-existing semantics. Readers can therefore observe only absence
/// or a complete header, while the returned handle retains the inode lock that
/// was acquired before publication.
pub(crate) fn open_locked_wal(
    path: &Path,
    initial_snapshot_seq: u64,
) -> PersistResult<(File, PathBuf, StoreWriter)> {
    let (dir, name) = StoreDirectory::for_file(path)?;
    crate::store_directory::validate_data_name(&name)?;
    #[cfg(test)]
    run_after_parent_anchor_hook();
    let authority = StoreWriter::acquire(&dir)?;
    let file = open_locked_wal_in(&authority, &name, initial_snapshot_seq)?;
    Ok((file, dir.locate(&name), authority))
}

pub(crate) fn open_locked_wal_in(
    authority: &StoreWriter,
    name: &Path,
    initial_snapshot_seq: u64,
) -> PersistResult<File> {
    let dir = authority.directory();
    crate::store_directory::validate_data_name(name)?;
    dir.require_legacy()?;
    let path = dir.locate(name);

    for _ in 0..OPEN_RACE_RETRIES {
        let file = match dir.open_write(name) {
            Ok(file) => file,
            Err(PersistError::Directory(DirectoryError::NotRegular(_))) => {
                return Err(PersistError::WalPathNotRegular { path });
            }
            Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                match create_initialized_wal(dir, name, initial_snapshot_seq)? {
                    Some(file) => return Ok(file),
                    None => continue,
                }
            }
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() {
            return Err(PersistError::WalPathNotRegular { path });
        }
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(PersistError::WriterLockHeld);
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        return Ok(file);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "active WAL path changed repeatedly while opening",
    )
    .into())
}

fn create_initialized_wal(
    dir: &StoreDirectory,
    name: &Path,
    snapshot_seq: u64,
) -> PersistResult<Option<File>> {
    let (mut file, temp_path) = create_init_temp(dir, name.as_os_str())?;
    if let Err(error) = file.try_lock() {
        cleanup_init_temp(dir, file, &temp_path);
        return match error {
            std::fs::TryLockError::WouldBlock => Err(PersistError::WriterLockHeld),
            std::fs::TryLockError::Error(error) => Err(error.into()),
        };
    }
    if let Err(error) = (|| -> PersistResult<()> {
        WalFileHeader::new(snapshot_seq).write_to(&mut file)?;
        file.sync_all()?;
        Ok(())
    })() {
        cleanup_init_temp(dir, file, &temp_path);
        return Err(error);
    }

    #[cfg(test)]
    run_before_wal_publish_hook();
    match dir.hard_link(&temp_path, name) {
        Ok(()) => {}
        Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            cleanup_init_temp(dir, file, &temp_path);
            return Ok(None);
        }
        Err(error) => {
            cleanup_init_temp(dir, file, &temp_path);
            return Err(error);
        }
    }
    if let Err(error) = dir.remove(&temp_path) {
        drop(file);
        let _ = dir.remove(&temp_path);
        return Err(error);
    }
    dir.sync()?;
    Ok(Some(file))
}

fn create_init_temp(
    dir: &StoreDirectory,
    file_name: &std::ffi::OsStr,
) -> PersistResult<(File, PathBuf)> {
    for _ in 0..INIT_TEMP_RETRIES {
        let nonce = WAL_INIT_NONCE.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = file_name.to_os_string();
        temp_name.push(format!(".init.{}.{nonce}.tmp", std::process::id()));
        let temp_path = PathBuf::from(temp_name);
        match dir.create_new(&temp_path) {
            Ok(file) => return Ok((file, temp_path)),
            Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique WAL initialization temporary",
    )
    .into())
}

fn cleanup_init_temp(dir: &StoreDirectory, file: File, temp_path: &Path) {
    drop(file);
    if let Err(error) = dir.remove(temp_path) {
        tracing::warn!(path = %temp_path.display(), %error, "could not remove WAL initialization temporary");
    }
}

/// Reject a present WAL path unless its directory entry is a regular file.
pub(crate) fn require_regular_wal_or_absent(
    dir: &StoreDirectory,
    name: &Path,
) -> PersistResult<()> {
    match dir.regular_metadata(name) {
        Ok(_) => Ok(()),
        Err(PersistError::Directory(DirectoryError::NotRegular(_))) => {
            Err(PersistError::WalPathNotRegular {
                path: dir.locate(name),
            })
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
thread_local! {
    static AFTER_PARENT_ANCHOR_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static AFTER_ROTATION_PREFLIGHT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static BEFORE_WAL_PUBLISH_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_after_parent_anchor_hook(hook: impl FnOnce() + 'static) {
    AFTER_PARENT_ANCHOR_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
pub(crate) fn set_after_rotation_preflight_hook(hook: impl FnOnce() + 'static) {
    AFTER_ROTATION_PREFLIGHT_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
pub(crate) fn set_before_wal_publish_hook(hook: impl FnOnce() + 'static) {
    BEFORE_WAL_PUBLISH_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn run_after_parent_anchor_hook() {
    AFTER_PARENT_ANCHOR_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn run_before_wal_publish_hook() {
    BEFORE_WAL_PUBLISH_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
pub(crate) fn run_after_rotation_preflight_hook() {
    AFTER_ROTATION_PREFLIGHT_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
#[path = "wal_path/tests.rs"]
mod tests;
