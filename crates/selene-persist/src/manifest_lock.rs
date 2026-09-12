//! Per-directory serialization for MANIFEST epoch reads and mutations.
//!
//! Cooperating handles and processes on a supported local filesystem use one
//! persistent lock-file inode. Legacy recovery and backup-style readers take a shared
//! lock while rotation, prune, and direct MANIFEST publication take an
//! exclusive lock. A writer's lock order is store `LOCK`, lifetime `wal.log`,
//! then this epoch lock, then any replacement-WAL temporary lock. All lock-file
//! opens are relative to the retained StoreDirectory handle; renaming the real
//! directory or its ancestors cannot redirect an acquired capability.
//! Format-2 LogicalReader holds this epoch only through selection and registration
//! of a shared immutable-manifest artifact lock. That separate owned lease retains
//! all selected names through consumption without excluding newer publication.

use std::fs::File;
use std::path::Path;

use crate::{PersistError, PersistResult, StoreDirectory, StoreWriter};

/// Filename of the persistent lock that serializes MANIFEST epoch operations.
///
/// The file is coordination state, not recovery data. It must never be
/// unlinked or replaced while a process may still hold it, because a new inode
/// would create a second, independent lock domain. The operating system
/// releases the advisory lock when the guard is dropped or its process exits,
/// while the named file remains in place for later operations.
pub const MANIFEST_LOCK_FILE_NAME: &str = "MANIFEST.lock";

/// Shared RAII guard for a stable persistence-directory epoch (legacy through-use contract).
///
/// Acquire this guard before reading the authoritative MANIFEST or selecting
/// snapshot/WAL/archive paths, and retain it until every selected artifact has
/// been consumed or copied. Multiple readers may coexist. Rotation, prune, and
/// direct MANIFEST publication block until all read guards are dropped, while
/// ordinary append-only WAL commits continue.
///
/// `CheckpointOutcome` paths are locators, not retention leases: backup code
/// must re-read the MANIFEST through the retained [`Self::directory`] handle
/// after acquiring this guard. Do not invoke same-directory checkpoint, rotation, prune, or
/// MANIFEST publication while holding a read guard; lock upgrades are not
/// supported and may deadlock. Operations that also own a [`crate::StoreWriter`]
/// acquire that writer first, then this guard.
#[must_use = "dropping the guard releases the persistence epoch lease"]
pub struct PersistenceReadGuard {
    dir: StoreDirectory,
    _file: File,
}

impl PersistenceReadGuard {
    /// Acquire a shared epoch lock for `dir`, blocking behind an in-flight
    /// rotation, prune, or direct MANIFEST publication.
    ///
    /// The directory is anchored before opening the persistent lock file.
    /// Acquiring a guard can therefore create `MANIFEST.lock` even for an empty
    /// or legacy MANIFEST-less directory; the file is coordination state and
    /// must not be copied into a backup.
    ///
    /// # Errors
    ///
    /// Returns directory-resolution, lock-file open, or platform file-locking
    /// errors.
    pub fn acquire(dir: &Path) -> PersistResult<Self> {
        Self::acquire_in(&StoreDirectory::open(dir)?)
    }

    /// Acquire an epoch lease relative to a retained directory capability.
    ///
    /// # Errors
    /// Returns lock-file validation, open, or locking errors.
    pub fn acquire_in(dir: &StoreDirectory) -> PersistResult<Self> {
        crate::legacy_probe::reject(dir)?;
        let file = open_lock_file(dir)?;
        Self::lock_shared(dir, file)
    }

    /// Acquire the existing epoch through an independently opened read-only file.
    /// Never creates or repairs coordination state and requires no write access.
    /// Selection still participates in the same permanent epoch lock domain.
    ///
    /// # Errors
    /// Returns missing-file, validation, read-open, or shared-lock errors.
    pub fn acquire_existing_in(dir: &StoreDirectory) -> PersistResult<Self> {
        Self::lock_shared(dir, dir.open_read(MANIFEST_LOCK_FILE_NAME)?)
    }

    fn lock_shared(dir: &StoreDirectory, file: File) -> PersistResult<Self> {
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                #[cfg(test)]
                run_contention_hook();
                file.lock_shared()?;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(PersistError::Io(error));
            }
        }
        Ok(Self {
            dir: dir.clone(),
            _file: file,
        })
    }

    /// Diagnostic locator only; use [`Self::directory`] for retained authority.
    #[must_use]
    pub fn dir(&self) -> &Path {
        self.dir.locator()
    }

    /// Retained directory protected by this lease; `dir()` is only a locator.
    #[must_use]
    pub fn directory(&self) -> &StoreDirectory {
        &self.dir
    }
}

/// Exclusive epoch plus retained writer proof. There is no directory-only
/// constructor: mutation callers must already own the store writer lease.
pub(crate) struct ManifestEpochGuard {
    // Release the epoch before releasing the last writer-lease reference.
    _file: File,
    authority: StoreWriter,
}

impl ManifestEpochGuard {
    /// Acquire the directory's stable epoch lock, blocking behind another
    /// cooperating reader, rotation, prune, or direct MANIFEST publication.
    pub(crate) fn acquire(authority: &StoreWriter) -> PersistResult<Self> {
        let file = open_lock_file(authority.directory())?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                #[cfg(test)]
                run_contention_hook();
                file.lock()?;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(PersistError::Io(error));
            }
        }
        Ok(Self {
            _file: file,
            authority: authority.clone(),
        })
    }

    pub(crate) fn directory(&self) -> &StoreDirectory {
        self.authority.directory()
    }
}

fn open_lock_file(dir: &StoreDirectory) -> PersistResult<File> {
    dir.open_or_create(Path::new(MANIFEST_LOCK_FILE_NAME))
}

#[cfg(test)]
thread_local! {
    static CONTENTION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_contention_hook(hook: impl FnOnce() + 'static) {
    CONTENTION_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn run_contention_hook() {
    CONTENTION_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
#[path = "manifest_lock/tests.rs"]
mod tests;
