//! Explicit corruption fixtures for real facade consumers, never a recovery engine.
//! Only available through the existing opt-in test harness feature.

use super::*;
use std::io::{Read, Write};

/// Recompute common fixed-envelope integrity after a bounded test mutation.
/// `header` is 160 for SLTXN2 or 168 for SLSNP2. Does not validate semantics.
#[doc(hidden)]
pub fn repair_fixture_integrity(bytes: &mut [u8], header: usize) {
    assert!(matches!(header, 160 | 168) && bytes.len() >= header + 40);
    let hash = *blake3::hash(&bytes[..header - 32]).as_bytes();
    bytes[header - 32..header].copy_from_slice(&hash);
    let end = bytes.len() - 32;
    let hash = *blake3::hash(&bytes[..end]).as_bytes();
    bytes[end..].copy_from_slice(&hash);
}

/// Corrupt one selected snapshot, then rebind exact descriptor/control hashes.
/// The mutation owns whether common snapshot integrity is repaired. This function
/// intentionally writes an existing scratch fixture, never creates a production store.
#[doc(hidden)]
pub fn mutate_snapshot_fixture(
    dir: &StoreDirectory,
    expected: &CompatibilityIdentity,
    mutate: impl FnOnce(&mut Vec<u8>),
) -> PersistResult<()> {
    let mut selected = select_in(dir, expected)?;
    let snapshot = selected.checkpoint.as_mut().expect("fixture snapshot");
    let mut bytes = Vec::new();
    dir.open_read(&snapshot.name)?.read_to_end(&mut bytes)?;
    mutate(&mut bytes);
    assert!(bytes.len() >= crate::logical_snapshot::OVERHEAD);
    snapshot.bytes = bytes.len() as u64;
    snapshot.digest = bytes[bytes.len() - 32..].try_into().unwrap();
    dir.open_write(Path::new(&snapshot.name))?
        .write_all(&bytes)?;
    write_selection(dir, &mut selected, |_| {})
}

/// Independently selected compatibility/control fixture mutation.
#[derive(Clone, Copy, Debug)]
#[doc(hidden)]
pub enum ControlMutation {
    /// Foreign store with all common hashes repaired.
    Store,
    /// Foreign epoch with all common hashes repaired.
    Epoch,
    /// Stale manifest generation.
    Generation,
    /// Unsupported control envelope version, with valid payload integrity.
    Version,
    /// Different profile hash.
    Profile,
    /// Different Unicode version.
    Unicode,
    /// Different collation identity.
    Collation,
}

/// Reencode a selected control fixture so corruption passes outer integrity guards.
#[doc(hidden)]
pub fn mutate_control_fixture(
    dir: &StoreDirectory,
    expected: &CompatibilityIdentity,
    mutation: ControlMutation,
) -> PersistResult<()> {
    let mut selected = select_in(dir, expected)?;
    let metadata = &mut selected.metadata;
    match mutation {
        ControlMutation::Store => metadata.store_id = StoreId::fresh(),
        ControlMutation::Epoch => metadata.epoch = StoreEpoch::new(metadata.epoch.get() + 1)?,
        ControlMutation::Generation => metadata.generation = metadata.generation.next()?,
        ControlMutation::Profile => metadata.identity.profile_hash[0] ^= 1,
        ControlMutation::Unicode => metadata.identity.unicode_version[0] += 1,
        ControlMutation::Collation => metadata.identity.collation.push_str("-different"),
        ControlMutation::Version => {}
    }
    write_selection(dir, &mut selected, |bytes| {
        if matches!(mutation, ControlMutation::Version) {
            bytes[4] = 2;
        }
    })
}

fn write_selection(
    dir: &StoreDirectory,
    selected: &mut Selected,
    mutate: impl FnOnce(&mut Vec<u8>),
) -> PersistResult<()> {
    let mut bytes = if selected.rotation.is_some() {
        rotation::fixture_encode(selected)?
    } else {
        checkpoint::fixture_encode(selected)?
    };
    mutate(&mut bytes);
    selected.selector.digest = *blake3::hash(&bytes).as_bytes();
    let file = dir.open_write(Path::new(selected.manifest_name()))?;
    file.set_len(0)?;
    (&file).write_all(&bytes)?;
    let file = dir.open_write(Path::new(CURRENT_FILE_NAME))?;
    file.set_len(0)?;
    (&file).write_all(&selected.selector.encode()?)?;
    Ok(())
}
