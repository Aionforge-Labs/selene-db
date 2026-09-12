//! Empty-store control publication. This is not durable facade/query execution.
//!
//! `CURRENT` selects exact immutable `MANIFEST-*.control` bytes. Version-1
//! control envelopes describe an empty format-2 store only; WAL 3.1, snapshot
//! 1.6, legacy MANIFEST 1 and audit 2 remain separate until F02-PR08. All legacy
//! data APIs reject a control directory, and empty control rejects data files.

mod codec;
pub(crate) mod logical;
mod types;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::manifest_lock::ManifestEpochGuard;
use crate::{
    ControlError, PersistError, PersistResult, PersistenceReadGuard, StoreDirectory, StoreWriter,
};
pub use codec::MAX_CONTROL_BYTES;
use types::Parent;
pub use types::{
    CompatibilityIdentity, CurrentSelector, EmptyManifest, ManifestGeneration, StoreEpoch, StoreId,
};

/// Authoritative selector filename for the empty-control protocol.
pub const CURRENT_FILE_NAME: &str = "CURRENT";

/// Frame bounded fuzz payloads using the production control integrity primitive.
/// This creates no selected state or filesystem authority and validates no semantics.
#[cfg(feature = "test-harness")]
#[doc(hidden)]
pub fn frame_fuzz_payload(body: &[u8], magic: [u8; 4]) -> PersistResult<Vec<u8>> {
    codec::encode_payload(body, magic)
}

/// Single-writer handle to a complete empty-store control state.
///
/// No database sessions, graph/catalog recovery, or transaction commits are
/// provided. Closing and reopening preserves StoreId/epoch, not process-local
/// DatabaseId or handles. Publication errors after CURRENT replacement require
/// reopening and fence this handle. Process-crash tests are not power-loss proof.
#[derive(Debug)]
pub struct EmptyStoreControl {
    authority: StoreWriter,
    manifest: EmptyManifest,
    selector: CurrentSelector,
    fenced: bool,
}

impl EmptyStoreControl {
    /// Exclusively create empty control in an otherwise empty retained directory.
    ///
    /// The caller creates the containing directory. Permanent coordination
    /// entries may exist. Unpublished orphans are never guessed to be a store or
    /// overwritten as a new lineage; offline reconciliation is required.
    ///
    /// # Errors
    /// Returns contention, unsupported/mixed state, compatibility, publication,
    /// or typed uncertain-publication errors. Existing CURRENT is not overwritten.
    pub fn create_empty(
        dir: &StoreDirectory,
        identity: CompatibilityIdentity,
    ) -> PersistResult<Self> {
        identity.validate()?;
        crate::legacy_probe::reject(dir)?;
        // Preflight before even creating LOCK. Repeat under the epoch below.
        validate_directory(dir)?;
        let authority = StoreWriter::acquire(dir)?;
        let epoch = ManifestEpochGuard::acquire(&authority)?;
        let artifacts = validate_directory(dir)?;
        if dir.contains(CURRENT_FILE_NAME)? {
            return Err(ControlError::AlreadyInitialized.into());
        }
        if artifacts {
            return Err(ControlError::UnpublishedArtifacts.into());
        }
        let manifest = EmptyManifest {
            store_id: StoreId::fresh(),
            epoch: StoreEpoch(1),
            generation: ManifestGeneration(1),
            format: [2, 0],
            identity,
            parent: None,
        };
        let selector = publish(&epoch, &manifest, true)?;
        Ok(Self {
            authority,
            manifest,
            selector,
            fenced: false,
        })
    }

    /// Validate CURRENT and its self-contained selected manifest, retaining writer ownership.
    /// Unselected ancestor files are not recovery dependencies.
    ///
    /// # Errors
    /// Rejects absent, corrupt, mixed, foreign, or incompatible control and contention.
    pub fn open(dir: &StoreDirectory, expected: &CompatibilityIdentity) -> PersistResult<Self> {
        expected.validate()?;
        crate::legacy_probe::reject(dir)?;
        let authority = StoreWriter::acquire(dir)?;
        let _epoch = PersistenceReadGuard::acquire_in(dir)?;
        let (manifest, selector) = read_state(dir, expected)?;
        Ok(Self {
            authority,
            manifest,
            selector,
            fenced: false,
        })
    }

    /// Last acknowledged control metadata; no query/data durability is implied.
    /// After uncertain publication this is not a statement of CURRENT: reopen
    /// before making an authoritative selection.
    #[must_use]
    pub fn manifest(&self) -> &EmptyManifest {
        &self.manifest
    }

    /// Retained directory authority (the locator is not a retention lease).
    #[must_use]
    pub fn directory(&self) -> &StoreDirectory {
        self.authority.directory()
    }

    /// Publish the next immutable generation of the same empty state.
    ///
    /// This exercises the control protocol without inventing a transaction WAL.
    /// A byte-identical unselected generation may be retried; a different one
    /// is rejected. No prior immutable generation is overwritten or pruned.
    ///
    /// # Errors
    /// Rejects stale/fenced handles, mixed state, exhausted generations, invalid
    /// lineage, or I/O failures. An uncertain publication fences further calls.
    pub fn publish_empty(&mut self) -> PersistResult<ManifestGeneration> {
        if self.fenced {
            return Err(ControlError::RequiresReopen.into());
        }
        let dir = self.directory();
        let epoch = ManifestEpochGuard::acquire(&self.authority)?;
        let (_, current) = read_state(dir, &self.manifest.identity)?;
        if current != self.selector {
            return Err(ControlError::Stale.into());
        }
        let manifest = EmptyManifest {
            generation: self.manifest.generation.next()?,
            parent: Some(Parent {
                generation: self.manifest.generation,
                digest: self.selector.digest,
            }),
            ..self.manifest.clone()
        };
        match publish(&epoch, &manifest, false) {
            Ok(selector) => {
                self.manifest = manifest;
                self.selector = selector;
                Ok(self.manifest.generation)
            }
            Err(error) => {
                if matches!(
                    error,
                    PersistError::Control(ControlError::PublicationUncertain { .. })
                ) {
                    self.fenced = true;
                }
                Err(error)
            }
        }
    }
}

fn read_state(
    dir: &StoreDirectory,
    expected: &CompatibilityIdentity,
) -> PersistResult<(EmptyManifest, CurrentSelector)> {
    validate_directory(dir)?;
    let bytes = match read_bounded(dir, Path::new(CURRENT_FILE_NAME)) {
        Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ControlError::NotInitialized.into());
        }
        result => result?,
    };
    let selector = CurrentSelector::decode(&bytes)?;
    let manifest = read_selected(dir, &selector)?;
    if &manifest.identity != expected {
        return Err(ControlError::Compatibility.into());
    }
    // The selected manifest is self-contained. Its parent link is structural
    // publication provenance, not a requirement to retain or open history.
    // Directory enumeration above remains O(entries); payload reads stay at two.
    Ok((manifest, selector))
}

fn read_selected(dir: &StoreDirectory, selector: &CurrentSelector) -> PersistResult<EmptyManifest> {
    selector.validate()?;
    let bytes =
        read_bounded(dir, Path::new(&selector.manifest_name)).map_err(|error| match error {
            PersistError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound => {
                ControlError::Lineage.into()
            }
            error => error,
        })?;
    if *blake3::hash(&bytes).as_bytes() != selector.digest {
        return Err(ControlError::Checksum.into());
    }
    let manifest = EmptyManifest::decode(&bytes)?;
    if manifest.store_id != selector.store_id
        || manifest.epoch != selector.epoch
        || manifest.generation != selector.generation
    {
        return Err(ControlError::Lineage.into());
    }
    Ok(manifest)
}

fn read_bounded(dir: &StoreDirectory, name: &Path) -> PersistResult<Vec<u8>> {
    let file = dir.open_read(name)?;
    #[cfg(test)]
    dir.record_control_payload_open();
    if file.metadata()?.len() > MAX_CONTROL_BYTES as u64 {
        return Err(ControlError::TooLarge.into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONTROL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONTROL_BYTES {
        return Err(ControlError::TooLarge.into());
    }
    Ok(bytes)
}

// Unknown/data files are not silently made part of an empty format-2 store.
fn validate_directory(dir: &StoreDirectory) -> PersistResult<bool> {
    let mut artifacts = false;
    for name in dir.entries()? {
        let path = Path::new(&name);
        dir.regular_metadata(path)?;
        if name == crate::STORE_LOCK_FILE_NAME || name == crate::MANIFEST_LOCK_FILE_NAME {
            continue;
        }
        let text = name
            .to_str()
            .ok_or_else(|| ControlError::MixedArtifacts(path.into()))?;
        if text == CURRENT_FILE_NAME || is_manifest_name(text) || is_stage_name(text) {
            artifacts = true;
        } else {
            return Err(ControlError::MixedArtifacts(path.into()).into());
        }
    }
    Ok(artifacts)
}

fn is_manifest_name(name: &str) -> bool {
    name.strip_prefix("MANIFEST-")
        .and_then(|s| s.strip_suffix(".control"))
        .and_then(|s| s.parse::<u64>().ok())
        .is_some_and(|n| n != 0 && ManifestGeneration(n).name() == name)
}

fn is_stage_name(name: &str) -> bool {
    name.strip_prefix(".control.")
        .and_then(|s| s.strip_suffix(".tmp"))
        .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
}

fn stage(guard: &ManifestEpochGuard, bytes: &[u8], kind: &'static str) -> PersistResult<PathBuf> {
    let dir = guard.directory();
    let (create, write, sync) = if kind == "manifest" {
        ("manifest.create", "manifest.write", "manifest.file_sync")
    } else {
        ("current.create", "current.write", "current.file_sync")
    };
    dir.check_fault(create)?;
    let name = PathBuf::from(format!(".control.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = dir.create_new(&name)?;
    let result = (|| -> PersistResult<()> {
        dir.check_fault(write)?;
        file.write_all(bytes)?;
        dir.check_fault(sync)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = dir.remove(&name);
        return Err(error);
    }
    Ok(name)
}

fn publish(
    guard: &ManifestEpochGuard,
    manifest: &EmptyManifest,
    initial: bool,
) -> PersistResult<CurrentSelector> {
    let bytes = manifest.encode()?;
    let selector = CurrentSelector::from_manifest(manifest, *blake3::hash(&bytes).as_bytes());
    publish_bytes(guard, &bytes, &selector, initial)?;
    Ok(selector)
}

fn publish_bytes(
    guard: &ManifestEpochGuard,
    bytes: &[u8],
    selector: &CurrentSelector,
    initial: bool,
) -> PersistResult<()> {
    let dir = guard.directory();
    let current_bytes = selector.encode()?;
    let name = PathBuf::from(&selector.manifest_name);
    if dir.contains(&name)? && read_bounded(dir, &name)? != bytes {
        return Err(ControlError::Lineage.into());
    }
    let temp = stage(guard, bytes, "manifest")?;
    let immutable = (|| -> PersistResult<()> {
        dir.check_fault("manifest.publish")?;
        match dir.publish_new(&temp, &name) {
            Ok(()) => {}
            Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if read_bounded(dir, &name)? != bytes {
                    return Err(ControlError::Lineage.into());
                }
                dir.open_read(&name)?.sync_all()?;
                dir.remove(&temp)?;
            }
            Err(error) => return Err(error),
        }
        dir.check_fault("manifest.dir_sync")?;
        dir.sync()
    })();
    if let Err(error) = immutable {
        let _ = dir.remove(&temp);
        return Err(error);
    }
    let temp = stage(guard, &current_bytes, "current")?;
    let mut published = false;
    let result = (|| -> PersistResult<()> {
        dir.check_fault("current.replace")?;
        // Conservatively treat a native replacement error as uncertain too:
        // an I/O failure is not proof that the directory entry stayed unchanged.
        published = true;
        if initial {
            if let Err(error) = dir.publish_new(&temp, Path::new(CURRENT_FILE_NAME)) {
                if matches!(&error, PersistError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists)
                {
                    published = false;
                    return Err(ControlError::AlreadyInitialized.into());
                }
                return Err(error);
            }
        } else {
            dir.rename(&temp, Path::new(CURRENT_FILE_NAME))?;
        }
        dir.check_fault("current.dir_sync")?;
        dir.sync()
    })();
    if let Err(error) = result {
        let _ = dir.remove(&temp);
        return if published {
            Err(ControlError::PublicationUncertain {
                source: Box::new(error),
            }
            .into())
        } else {
            Err(error)
        };
    }
    Ok(())
}

#[cfg(test)]
mod selected_state_tests;
#[cfg(test)]
mod tests;
