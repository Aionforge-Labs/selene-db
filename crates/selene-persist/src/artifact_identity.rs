//! Exact identity checks for immutable persistence artifacts.

use std::io::{BufReader, Read};
use std::path::Path;

use crate::{PersistError, PersistResult};

const COMPARE_BUFFER_BYTES: usize = 64 * 1024;

/// Require `existing` to be the same regular-file bytes as `expected`.
///
/// Rotation calls this only after a no-overwrite publish reports that the final
/// path already exists. Comparing the just-written temporary instead of stored
/// checksums proves the complete file identity, including headers and trailing
/// bytes that an envelope-level checksum might not cover.
pub(crate) fn require_identical_regular_files(
    dir: &crate::StoreDirectory,
    expected: &Path,
    existing: &Path,
) -> PersistResult<()> {
    let expected_file = dir.open_read(expected)?;
    let existing_file = dir.open_read(existing).map_err(|error| match error {
        PersistError::Directory(_) => identity_mismatch(&dir.locate(existing)),
        PersistError::Io(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
            identity_mismatch(&dir.locate(existing))
        }
        error => error,
    })?;
    let expected_metadata = expected_file.metadata()?;
    if expected_metadata.len() != existing_file.metadata()?.len() {
        return Err(identity_mismatch(&dir.locate(existing)));
    }

    let mut expected_reader = BufReader::with_capacity(COMPARE_BUFFER_BYTES, expected_file);
    let mut existing_reader = BufReader::with_capacity(COMPARE_BUFFER_BYTES, existing_file);
    let mut expected_bytes = [0_u8; COMPARE_BUFFER_BYTES];
    let mut existing_bytes = [0_u8; COMPARE_BUFFER_BYTES];
    let mut remaining = expected_metadata.len();
    while remaining != 0 {
        let chunk = usize::try_from(remaining.min(COMPARE_BUFFER_BYTES as u64))
            .expect("comparison chunk is bounded by the usize buffer length");
        expected_reader.read_exact(&mut expected_bytes[..chunk])?;
        existing_reader.read_exact(&mut existing_bytes[..chunk])?;
        if expected_bytes[..chunk] != existing_bytes[..chunk] {
            return Err(identity_mismatch(&dir.locate(existing)));
        }
        remaining -= chunk as u64;
    }
    let mut trailing = [0_u8; 1];
    if expected_reader.read(&mut trailing)? != 0 || existing_reader.read(&mut trailing)? != 0 {
        return Err(identity_mismatch(&dir.locate(existing)));
    }
    Ok(())
}

/// Require `path` itself (not a symlink target) to be a regular file.
pub(crate) fn require_regular_file(dir: &crate::StoreDirectory, path: &Path) -> PersistResult<()> {
    match dir.regular_metadata(path) {
        Ok(Some(_)) => Ok(()),
        Ok(None) | Err(PersistError::Directory(_)) => Err(identity_mismatch(&dir.locate(path))),
        Err(error) => Err(error),
    }
}

pub(crate) fn identity_mismatch(path: &Path) -> PersistError {
    PersistError::ArtifactIdentityMismatch {
        path: path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "selene-artifact-identity-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn exact_comparison_streams_multiple_chunks_and_detects_tail_difference() {
        let dir = temp_dir();
        let expected = dir.join("expected");
        let existing = dir.join("existing");
        let bytes = (0..(COMPARE_BUFFER_BYTES * 3 + 17))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        std::fs::write(&expected, &bytes).unwrap();
        std::fs::write(&existing, &bytes).unwrap();
        let cap = crate::StoreDirectory::open(&dir).unwrap();
        require_identical_regular_files(&cap, Path::new("expected"), Path::new("existing"))
            .unwrap();

        let mut different = bytes;
        *different.last_mut().unwrap() ^= 1;
        std::fs::write(&existing, different).unwrap();
        assert!(matches!(
            require_identical_regular_files(&cap, Path::new("expected"), Path::new("existing")),
            Err(PersistError::ArtifactIdentityMismatch { path }) if path == cap.locator().join("existing")
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
