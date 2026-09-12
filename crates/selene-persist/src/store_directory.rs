//! Retained filesystem authority for all managed persistence artifacts.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{DirectoryError, PersistError, PersistResult};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod native;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[path = "store_directory/unsupported.rs"]
mod native;
#[cfg(feature = "test-harness")]
mod observation;
#[cfg(test)]
mod test_hooks;

/// Persistent store-wide writer coordination entry. Never unlink or replace it.
pub const STORE_LOCK_FILE_NAME: &str = "LOCK";

/// An opened directory, not a pathname lease.
///
/// All child operations use this retained handle. Renaming the directory or any
/// ancestor cannot redirect them. Initial ambient path resolution is outside
/// this guarantee; use [`Self::from_file`] to supply an already-open authority.
/// Only Linux/macOS native local filesystems providing file locks, hard links,
/// atomic same-directory rename, and file/directory synchronization are supported.
/// Other platforms return [`DirectoryError::UnsupportedPlatform`].
///
/// Managed entries must be regular files. External hard links and concurrent
/// replacement of entries *inside* the directory by non-cooperating writers are
/// unsupported. Final symlinks are never followed, including after a preflight
/// race. Coordination entries must remain permanently named.
#[derive(Clone, Debug)]
pub struct StoreDirectory {
    file: Arc<File>,
    locator: Arc<PathBuf>,
    #[cfg(test)]
    hooks: Arc<test_hooks::TestHooks>,
    #[cfg(feature = "test-harness")]
    observation: Arc<observation::Observation>,
}

/// Metadata obtained without following the final managed component.
#[derive(Clone, Debug)]
pub(crate) struct EntryMetadata {
    pub regular: bool,
    pub single_link: bool,
    pub len: u64,
}

impl StoreDirectory {
    /// Anchor an existing directory once using ambient process authority.
    ///
    /// # Errors
    /// Returns unsupported-platform, resolution, or directory-open errors.
    pub fn open(path: &Path) -> PersistResult<Self> {
        native::supported()?;
        let path = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        // Canonicalization is for a useful locator, not later I/O authority.
        // The guarantee begins with the native directory open below.
        let locator = std::fs::canonicalize(path)?;
        Self::from_file(native::open_directory(&locator)?, locator)
    }

    /// Accept a caller-owned open directory handle; `locator` is diagnostic only.
    ///
    /// # Errors
    /// Rejects unsupported platforms, non-directory handles, or metadata errors.
    pub fn from_file(file: File, locator: impl Into<PathBuf>) -> PersistResult<Self> {
        native::supported()?;
        if !file.metadata()?.is_dir() {
            return Err(DirectoryError::NotDirectory.into());
        }
        Ok(Self {
            file: Arc::new(file),
            locator: Arc::new(locator.into()),
            #[cfg(test)]
            hooks: Arc::default(),
            #[cfg(feature = "test-harness")]
            observation: Arc::default(),
        })
    }

    /// Diagnostic locator captured at open. It may no longer identify this store.
    #[must_use]
    pub fn locator(&self) -> &Path {
        &self.locator
    }

    /// Compare retained physical directory identity, not locators or StoreIds.
    ///
    /// # Errors
    /// Returns native metadata errors.
    pub fn same_directory(&self, other: &Self) -> PersistResult<bool> {
        native::same_directory(&self.file, &other.file)
    }

    /// Check presence of a regular managed entry without following symlinks.
    ///
    /// # Errors
    /// Rejects invalid names, special files, symlinks, and metadata errors.
    pub fn contains(&self, name: impl AsRef<Path>) -> PersistResult<bool> {
        Ok(self.regular_metadata(name.as_ref())?.is_some())
    }

    /// Open a managed regular file read-only, relative to this capability.
    ///
    /// The returned file retains inode authority but is not an epoch lease.
    /// Hold [`crate::PersistenceReadGuard`] for multi-artifact reads.
    ///
    /// # Errors
    /// Returns invalid-name, non-regular-file, or native open errors.
    pub fn open_read(&self, name: impl AsRef<Path>) -> PersistResult<File> {
        self.open_file(name.as_ref(), false, false)
    }

    pub(crate) fn open_write(&self, name: &Path) -> PersistResult<File> {
        self.open_file(name, true, false)
    }

    pub(crate) fn create_new(&self, name: &Path) -> PersistResult<File> {
        self.open_file(name, true, true)
    }

    fn open_file(&self, name: &Path, write: bool, exclusive: bool) -> PersistResult<File> {
        #[cfg(feature = "test-harness")]
        if write {
            self.observe_mutation()?;
        }
        validate_name(name)?;
        if !exclusive {
            self.regular_metadata(name)?;
        }
        #[cfg(test)]
        self.run_open_hook();
        let file = native::open_file(&self.file, name, write, exclusive)?;
        if !native::is_regular(&file)? {
            return Err(DirectoryError::NotRegular(self.locate(name)).into());
        }
        Ok(file)
    }

    pub(crate) fn open_or_create(&self, name: &Path) -> PersistResult<File> {
        match self.create_new(name) {
            Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.open_write(name)
            }
            result => result,
        }
    }

    pub(crate) fn metadata(&self, name: &Path) -> PersistResult<Option<EntryMetadata>> {
        validate_name(name)?;
        native::metadata(&self.file, name)
    }

    pub(crate) fn regular_metadata(&self, name: &Path) -> PersistResult<Option<EntryMetadata>> {
        let metadata = self.metadata(name)?;
        if metadata
            .as_ref()
            .is_some_and(|m| !m.regular || !m.single_link)
        {
            return Err(DirectoryError::NotRegular(self.locate(name)).into());
        }
        Ok(metadata)
    }

    pub(crate) fn entries(&self) -> PersistResult<Vec<OsString>> {
        native::entries(&self.file, usize::MAX)
    }

    /// Reject an oversized maintenance inventory during enumeration, not after
    /// collecting all names. Other existing directory callers keep their policy.
    pub(crate) fn entries_bounded(&self, limit: usize) -> PersistResult<Vec<OsString>> {
        native::entries(&self.file, limit)
    }

    pub(crate) fn rename(&self, from: &Path, to: &Path) -> PersistResult<()> {
        validate_mutable_name(from)?;
        validate_mutable_name(to)?;
        self.regular_metadata(from)?;
        self.regular_metadata(to)?;
        native::rename(&self.file, from, to)
    }

    pub(crate) fn publish_new(&self, from: &Path, to: &Path) -> PersistResult<()> {
        validate_mutable_name(from)?;
        validate_mutable_name(to)?;
        self.regular_metadata(from)?;
        native::publish_new(&self.file, from, to)
    }

    pub(crate) fn check_fault(&self, _point: &'static str) -> PersistResult<()> {
        #[cfg(feature = "test-harness")]
        self.observe_phase(_point);
        #[cfg(test)]
        self.run_fault(_point)?;
        Ok(())
    }

    pub(crate) fn remove(&self, name: &Path) -> PersistResult<()> {
        validate_mutable_name(name)?;
        // unlinkat removes the entry itself, never a symlink target. Permit
        // removal of our hard-link publication temp (temporarily nlink == 2).
        if self.metadata(name)?.is_some_and(|m| !m.regular) {
            return Err(DirectoryError::NotRegular(self.locate(name)).into());
        }
        native::remove(&self.file, name)
    }

    /// Synchronize directory entries using the retained handle.
    ///
    /// # Errors
    /// Propagates native synchronization failures; there is no weaker fallback.
    pub fn sync(&self) -> PersistResult<()> {
        #[cfg(feature = "test-harness")]
        self.observe_mutation()?;
        self.file.sync_all()?;
        Ok(())
    }

    pub(crate) fn locate(&self, name: &Path) -> PathBuf {
        self.locator.join(name)
    }
}

/// Owned single-writer authority shared only by explicitly composed components.
///
/// Cloning shares the *existing* lease; acquiring through any directory alias
/// opens a separate lock handle and fails while the lease is held. No registry
/// grants implicit reentrant permission. The lock entry is never removed.
#[derive(Clone, Debug)]
pub struct StoreWriter {
    directory: StoreDirectory,
    _lock: Arc<File>,
}

impl StoreWriter {
    /// Acquire an existing permanent writer entry without creating any artifact.
    /// Used by non-destructive format-2 open; a missing lock is an error, not repair.
    pub fn acquire_existing(directory: &StoreDirectory) -> PersistResult<Self> {
        crate::legacy_probe::reject(directory)?;
        Self::lock_file(
            directory,
            directory.open_write(Path::new(STORE_LOCK_FILE_NAME))?,
        )
    }
    /// Acquire nonblocking writer ownership for this physical directory.
    ///
    /// # Errors
    /// Returns [`PersistError::WriterLockHeld`] on contention, or native errors.
    pub fn acquire(directory: &StoreDirectory) -> PersistResult<Self> {
        crate::legacy_probe::reject(directory)?;
        let file = directory.open_or_create(Path::new(STORE_LOCK_FILE_NAME))?;
        Self::lock_file(directory, file)
    }

    fn lock_file(directory: &StoreDirectory, file: File) -> PersistResult<Self> {
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(PersistError::WriterLockHeld),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        Ok(Self {
            directory: directory.clone(),
            _lock: Arc::new(file),
        })
    }

    /// The retained directory protected by this writer lease.
    #[must_use]
    pub fn directory(&self) -> &StoreDirectory {
        &self.directory
    }
}

pub(crate) fn validate_name(name: &Path) -> PersistResult<()> {
    let bytes = name.as_os_str().as_encoded_bytes();
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes.iter().any(|b| matches!(b, b'/' | b'\\' | b':' | 0))
        || name.file_name() != Some(name.as_os_str())
    {
        return Err(DirectoryError::InvalidName(name.into()).into());
    }
    Ok(())
}

fn validate_mutable_name(name: &Path) -> PersistResult<()> {
    validate_name(name)?;
    if name == OsStr::new(STORE_LOCK_FILE_NAME)
        || name == OsStr::new(crate::MANIFEST_LOCK_FILE_NAME)
    {
        return Err(DirectoryError::CoordinationEntry(name.into()).into());
    }
    Ok(())
}

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
