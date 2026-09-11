//! Single unsealed format-2 segment selection. No rotation or snapshot lifecycle.

use super::*;
use crate::logical_frame::Context;
use serde::{Deserialize, Serialize};
use std::fs::File;

pub(crate) const LOG_NAME: &str = "WAL-00000000000000000001.logical";

#[derive(Serialize, Deserialize)]
struct LogicalManifest {
    metadata: EmptyManifest,
    segment: [u8; 32],
}

fn decode(bytes: &[u8]) -> PersistResult<LogicalManifest> {
    let manifest: LogicalManifest = codec::decode(bytes, *b"SLLM")?;
    manifest.metadata.validate()?;
    StoreId::from_bytes(*manifest.metadata.store_id.as_bytes())?;
    if manifest.segment == [0; 32] {
        return Err(ControlError::Lineage.into());
    }
    Ok(manifest)
}

pub(crate) fn validate(bytes: &[u8]) -> PersistResult<()> {
    decode(bytes).map(|_| ())
}

pub(crate) fn create(control: EmptyStoreControl) -> PersistResult<(StoreWriter, File, Context)> {
    if control.fenced {
        return Err(ControlError::RequiresReopen.into());
    }
    let guard = ManifestEpochGuard::acquire(&control.authority)?;
    let dir = guard.directory();
    let (_, selected) = read_state(dir, &control.manifest.identity)?;
    if selected != control.selector {
        return Err(ControlError::Stale.into());
    }
    let mut segment = [0; 32];
    segment[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    segment[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let manifest = LogicalManifest {
        metadata: EmptyManifest {
            generation: control.manifest.generation.next()?,
            parent: Some(Parent {
                generation: control.manifest.generation,
                digest: control.selector.digest,
            }),
            ..control.manifest
        },
        segment,
    };
    let bytes = codec::encode(&manifest, *b"SLLM")?;
    let selector =
        CurrentSelector::from_manifest(&manifest.metadata, *blake3::hash(&bytes).as_bytes());
    // Durable empty segment precedes the selector that makes it authoritative.
    // Failed bootstrap leaves an orphan, never a guessed/reused new store.
    let file = dir.create_new(Path::new(LOG_NAME))?;
    file.sync_all()?;
    dir.sync()?;
    publish_bytes(&guard, &bytes, &selector, false)?;
    let context = context(&manifest, &selector);
    drop(guard);
    Ok((control.authority, file, context))
}

pub(crate) fn select(
    guard: &PersistenceReadGuard,
    expected: &CompatibilityIdentity,
) -> PersistResult<(File, Context)> {
    expected.validate()?;
    let dir = guard.directory();
    for name in dir.entries()? {
        let path = Path::new(&name);
        dir.regular_metadata(path)?;
        let text = name
            .to_str()
            .ok_or_else(|| ControlError::MixedArtifacts(path.into()))?;
        if !matches!(
            text,
            CURRENT_FILE_NAME
                | LOG_NAME
                | crate::STORE_LOCK_FILE_NAME
                | crate::MANIFEST_LOCK_FILE_NAME
        ) && !is_manifest_name(text)
            && !is_stage_name(text)
        {
            return Err(ControlError::MixedArtifacts(path.into()).into());
        }
    }
    let selector = CurrentSelector::decode(&read_bounded(dir, Path::new(CURRENT_FILE_NAME))?)?;
    let bytes = read_bounded(dir, Path::new(&selector.manifest_name))?;
    if *blake3::hash(&bytes).as_bytes() != selector.digest {
        return Err(ControlError::Checksum.into());
    }
    let manifest = decode(&bytes)?;
    if manifest.metadata.store_id != selector.store_id
        || manifest.metadata.epoch != selector.epoch
        || manifest.metadata.generation != selector.generation
        || manifest.segment == [0; 32]
    {
        return Err(ControlError::Lineage.into());
    }
    if &manifest.metadata.identity != expected {
        return Err(ControlError::Compatibility.into());
    }
    Ok((dir.open_read(LOG_NAME)?, context(&manifest, &selector)))
}

fn context(manifest: &LogicalManifest, selector: &CurrentSelector) -> Context {
    Context {
        store: manifest.metadata.store_id,
        epoch: manifest.metadata.epoch,
        sequence: 1,
        segment: manifest.segment,
        previous: selector.digest,
    }
}
