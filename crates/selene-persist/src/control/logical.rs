//! Single unsealed format-2 segment selection. No rotation or snapshot lifecycle.

use super::*;
use crate::logical_frame::Context;
use serde::{Deserialize, Serialize};
use std::fs::File;
mod checkpoint;
pub(crate) use checkpoint::{SnapshotDescriptor, publish_checkpoint};

pub(crate) struct Selected {
    pub metadata: EmptyManifest,
    pub selector: CurrentSelector,
    pub context: Context,
    pub checkpoint: Option<SnapshotDescriptor>,
}

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
    if bytes.starts_with(b"SLDM") {
        return checkpoint::validate(bytes);
    }
    decode(bytes).map(|_| ())
}

pub(crate) fn create(control: EmptyStoreControl) -> PersistResult<(StoreWriter, File, Selected)> {
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
    Ok((
        control.authority,
        file,
        Selected {
            metadata: manifest.metadata,
            selector,
            context,
            checkpoint: None,
        },
    ))
}

pub(crate) fn select(
    guard: &PersistenceReadGuard,
    expected: &CompatibilityIdentity,
) -> PersistResult<(File, Selected)> {
    expected.validate()?;
    let dir = guard.directory();
    let selected = select_in(dir, expected)?;
    Ok((dir.open_read(LOG_NAME)?, selected))
}

pub(crate) fn select_in(
    dir: &StoreDirectory,
    expected: &CompatibilityIdentity,
) -> PersistResult<Selected> {
    expected.validate()?;
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
            && !checkpoint::is_snapshot_name(text)
            && !text
                .strip_prefix(".snapshot.")
                .and_then(|s| s.strip_suffix(".tmp"))
                .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
        {
            return Err(ControlError::MixedArtifacts(path.into()).into());
        }
    }
    let selector = CurrentSelector::decode(&read_bounded(dir, Path::new(CURRENT_FILE_NAME))?)?;
    let bytes = read_bounded(dir, Path::new(&selector.manifest_name))?;
    if *blake3::hash(&bytes).as_bytes() != selector.digest {
        return Err(ControlError::Checksum.into());
    }
    if bytes.starts_with(b"SLDM") {
        return checkpoint::decode_selected(&bytes, selector, expected);
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
    Ok(Selected {
        context: context(&manifest, &selector),
        metadata: manifest.metadata,
        selector,
        checkpoint: None,
    })
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
