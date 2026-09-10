//! Snapshot file naming helpers.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::PersistResult;

/// Final snapshot file extension.
pub const SNAPSHOT_FILE_EXTENSION: &str = "snap";
/// Base suffix for snapshot temporary-attempt files.
pub const SNAPSHOT_TMP_EXTENSION: &str = "snap.tmp";

/// Build the final snapshot path for `sequence`.
#[must_use]
pub fn snapshot_path(dir: &Path, sequence: u64) -> PathBuf {
    dir.join(format!("snapshot.{sequence}.{SNAPSHOT_FILE_EXTENSION}"))
}

/// Build the base temporary snapshot path for `sequence`.
///
/// [`crate::SnapshotBuilder`] appends a process id and attempt counter to this
/// base. The unsuffixed path remains useful for recognizing legacy crash
/// temporaries and constructing test fixtures.
#[must_use]
pub fn snapshot_tmp_path(dir: &Path, sequence: u64) -> PathBuf {
    dir.join(format!("snapshot.{sequence}.{SNAPSHOT_TMP_EXTENSION}"))
}

/// Parse `snapshot.{seq}.snap` into a sequence number.
#[must_use]
pub fn parse_snapshot_filename(name: &OsStr) -> Option<u64> {
    let name = name.to_str()?;
    let rest = name.strip_prefix("snapshot.")?;
    if rest.ends_with(".snap.tmp") {
        return None;
    }
    let seq = rest.strip_suffix(".snap")?;
    if seq.is_empty() {
        return None;
    }
    seq.parse::<u64>().ok()
}

/// Find the highest-sequence snapshot in `dir`.
///
/// Malformed names, temporary files, and non-regular-file entries (directories,
/// symlinks) are ignored — recovery should not try to open a directory named
/// `snapshot.{seq}.snap` even if its parsed sequence is higher than any real
/// snapshot present.
///
/// # Errors
///
/// Returns I/O errors from directory scanning.
pub fn find_latest_snapshot(dir: &Path) -> PersistResult<Option<(u64, PathBuf)>> {
    let cap = crate::StoreDirectory::open(dir)?;
    Ok(find_latest_snapshot_in(&cap)?.map(|(seq, name)| (seq, dir.join(name))))
}

/// Find the highest legacy snapshot through retained authority, returning its child name.
///
/// # Errors
/// Returns protocol, directory-scan, or entry metadata errors.
pub fn find_latest_snapshot_in(
    dir: &crate::StoreDirectory,
) -> PersistResult<Option<(u64, PathBuf)>> {
    dir.require_legacy()?;
    let mut latest: Option<(u64, PathBuf)> = None;
    for name in dir.entries()? {
        let Some(sequence) = parse_snapshot_filename(&name) else {
            continue;
        };
        if !dir
            .metadata(Path::new(&name))?
            .is_some_and(|m| m.regular && m.single_link)
        {
            continue;
        }
        let path = PathBuf::from(name);
        match &latest {
            Some((current, _)) if *current >= sequence => {}
            _ => latest = Some((sequence, path)),
        }
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "selene-snapshot-path-{name}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_snapshot_filename_round_trips() {
        let path = snapshot_path(Path::new("/tmp"), 42);
        assert_eq!(parse_snapshot_filename(path.file_name().unwrap()), Some(42));
    }

    #[test]
    fn parse_rejects_tmp_files() {
        let path = snapshot_tmp_path(Path::new("/tmp"), 42);
        assert_eq!(parse_snapshot_filename(path.file_name().unwrap()), None);
    }

    #[test]
    fn parse_returns_none_for_missing_extension() {
        assert_eq!(parse_snapshot_filename(OsStr::new("snapshot.42")), None);
        assert_eq!(parse_snapshot_filename(OsStr::new("snapshot..snap")), None);
        assert_eq!(parse_snapshot_filename(OsStr::new("other.42.snap")), None);
    }

    #[test]
    fn find_latest_snapshot_picks_highest_sequence() {
        let dir = temp_dir("latest");
        fs::write(snapshot_path(&dir, 1), []).unwrap();
        fs::write(snapshot_path(&dir, 9), []).unwrap();
        fs::write(snapshot_path(&dir, 3), []).unwrap();
        let (sequence, path) = find_latest_snapshot(&dir).unwrap().unwrap();
        assert_eq!(sequence, 9);
        assert_eq!(path, snapshot_path(&dir, 9));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn find_latest_snapshot_ignores_tmp_and_garbage() {
        let dir = temp_dir("ignore");
        fs::write(snapshot_tmp_path(&dir, 99), []).unwrap();
        fs::write(dir.join("snapshot.nope.snap"), []).unwrap();
        fs::write(snapshot_path(&dir, 2), []).unwrap();
        assert_eq!(find_latest_snapshot(&dir).unwrap().unwrap().0, 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn find_latest_snapshot_returns_none_for_empty_dir() {
        let dir = temp_dir("empty");
        assert!(find_latest_snapshot(&dir).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn find_latest_snapshot_skips_directories_named_like_snapshots() {
        let dir = temp_dir("dir-named-snap");
        fs::create_dir(snapshot_path(&dir, 99)).unwrap(); // directory shaped like a snapshot
        fs::write(snapshot_path(&dir, 2), []).unwrap();
        let (sequence, path) = find_latest_snapshot(&dir).unwrap().unwrap();
        assert_eq!(sequence, 2);
        assert_eq!(path, snapshot_path(&dir, 2));
        let _ = fs::remove_dir_all(dir);
    }
}
