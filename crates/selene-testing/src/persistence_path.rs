//! Cleanup-owned filesystem fixtures with one directory per persistence store.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A file locator inside a unique, cleanup-owned directory.
///
/// Unlike a bare file in the process temp directory, each fixture gets its own
/// persistent `LOCK` domain. Drop the guard after all handles and child workers.
/// A dereferenced clone copies only the locator, not cleanup ownership.
#[derive(Debug)]
pub struct PersistenceTestPath {
    root: PathBuf,
    path: PathBuf,
}

impl PersistenceTestPath {
    /// Allocate an isolated store directory and a not-yet-created `wal.log` path.
    ///
    /// # Panics
    /// Panics if the test host cannot create an exclusive temporary directory.
    #[must_use]
    pub fn new() -> Self {
        loop {
            let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "selene-isolated-store-{}-{ordinal}",
                std::process::id()
            ));
            match std::fs::create_dir(&root) {
                Ok(()) => {
                    return Self {
                        path: root.join("wal.log"),
                        root,
                    };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated persistence fixture: {error}"),
            }
        }
    }
}

impl Default for PersistenceTestPath {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for PersistenceTestPath {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

impl AsRef<Path> for PersistenceTestPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for PersistenceTestPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
