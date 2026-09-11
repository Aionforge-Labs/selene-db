//! Self-contained snapshot selection retaining the independent original WAL anchor.

use super::*;
use crate::{
    logical_snapshot::{self, SnapshotContext},
    logical_stream::Position,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SnapshotDescriptor {
    pub name: String,
    pub bytes: u64,
    pub digest: [u8; 32],
    pub boundary: Position,
    pub publication: u64,
}

#[derive(Serialize, Deserialize)]
pub(super) struct DataManifest {
    pub(super) metadata: EmptyManifest,
    segment: [u8; 32],
    origin: [u8; 32],
    snapshot: SnapshotDescriptor,
}

#[cfg(feature = "test-harness")]
pub(super) fn fixture_encode(selected: &Selected) -> PersistResult<Vec<u8>> {
    codec::encode(
        &DataManifest {
            metadata: selected.metadata.clone(),
            segment: selected.context.segment,
            origin: selected.context.previous,
            snapshot: selected.checkpoint.clone().expect("fixture snapshot"),
        },
        *b"SLDM",
    )
}

pub(super) fn is_snapshot_name(name: &str) -> bool {
    name.strip_prefix("SNAPSHOT-")
        .and_then(|s| s.strip_suffix(".logical"))
        .and_then(|s| s.parse::<u64>().ok())
        .is_some_and(|n| n != 0 && snapshot_name(n) == name)
}
pub(super) fn snapshot_name(generation: u64) -> String {
    format!("SNAPSHOT-{generation:020}.logical")
}

pub(super) fn decode_selected(
    bytes: &[u8],
    selector: CurrentSelector,
    expected: &CompatibilityIdentity,
) -> PersistResult<Selected> {
    let manifest = decode_manifest(bytes)?;
    let p = manifest.snapshot.boundary;
    if manifest.metadata.store_id != selector.store_id
        || manifest.metadata.epoch != selector.epoch
        || manifest.metadata.generation != selector.generation
    {
        return Err(ControlError::Lineage.into());
    }
    if &manifest.metadata.identity != expected {
        return Err(ControlError::Compatibility.into());
    }
    Ok(Selected {
        context: Context {
            store: p.store,
            epoch: p.epoch,
            segment: manifest.segment,
            sequence: 1,
            previous: manifest.origin,
        },
        metadata: manifest.metadata,
        selector,
        checkpoint: Some(manifest.snapshot),
        rotation: None,
    })
}

pub(super) fn validate(bytes: &[u8]) -> PersistResult<()> {
    decode_manifest(bytes).map(|_| ())
}

pub(super) fn decode_manifest(bytes: &[u8]) -> PersistResult<DataManifest> {
    let manifest: DataManifest = codec::decode(bytes, *b"SLDM")?;
    manifest.metadata.validate()?;
    let p = manifest.snapshot.boundary;
    StoreId::from_bytes(*p.store.as_bytes())?;
    StoreEpoch::new(p.epoch.get())?;
    if p.store != manifest.metadata.store_id
        || p.epoch != manifest.metadata.epoch
        || p.segment != manifest.segment
        || p.segment == [0; 32]
        || manifest.origin == [0; 32]
        || p.digest == [0; 32]
        || (p.sequence == 0) != (p.offset == 0)
        || (p.sequence == 0 && p.digest != manifest.origin)
        || manifest.snapshot.name != snapshot_name(manifest.metadata.generation.get())
        || manifest.snapshot.bytes < logical_snapshot::OVERHEAD as u64
        || manifest.snapshot.bytes
            > (selene_core::logical::Limits::default().bytes + logical_snapshot::OVERHEAD) as u64
    {
        return Err(ControlError::Lineage.into());
    }
    Ok(manifest)
}

pub(crate) fn publish_checkpoint(
    guard: &ManifestEpochGuard,
    previous: &Selected,
    body: &[u8],
    context: SnapshotContext,
) -> Result<Selected, crate::logical_stream::StreamError> {
    let dir = guard.directory();
    let current = select_in(dir, &previous.metadata.identity)?;
    if current.selector != previous.selector {
        return Err(PersistError::Control(ControlError::Stale).into());
    }
    let generation = previous.metadata.generation.next()?;
    let snapshot = publish_snapshot(dir, generation, body, context)?;
    let manifest = DataManifest {
        metadata: EmptyManifest {
            generation,
            parent: Some(Parent {
                generation: previous.metadata.generation,
                digest: previous.selector.digest,
            }),
            ..previous.metadata.clone()
        },
        segment: previous.context.segment,
        origin: previous.context.previous,
        snapshot,
    };
    let bytes = codec::encode(&manifest, *b"SLDM")?;
    let selector =
        CurrentSelector::from_manifest(&manifest.metadata, *blake3::hash(&bytes).as_bytes());
    publish_bytes(guard, &bytes, &selector, false)?;
    Ok(decode_selected(
        &bytes,
        selector,
        &previous.metadata.identity,
    )?)
}

pub(super) fn publish_snapshot(
    dir: &StoreDirectory,
    generation: ManifestGeneration,
    body: &[u8],
    context: SnapshotContext,
) -> Result<SnapshotDescriptor, crate::logical_stream::StreamError> {
    let name = snapshot_name(generation.get());
    let bytes =
        logical_snapshot::encode(body, context, selene_core::logical::Limits::default().bytes)
            .map_err(|e| crate::logical_stream::StreamError::Preparation(Box::new(e)))?;
    let digest: [u8; 32] = bytes[bytes.len() - 32..]
        .try_into()
        .expect("snapshot digest");
    logical_snapshot::decode(
        &bytes,
        context,
        &digest,
        selene_core::logical::Limits::default().bytes,
    )
    .map_err(|e| crate::logical_stream::StreamError::Preparation(Box::new(e)))?;
    let temp = format!(".snapshot.{}.tmp", uuid::Uuid::new_v4());
    dir.check_fault("snapshot.create")?;
    let mut file = dir.create_new(Path::new(&temp))?;
    dir.check_fault("snapshot.write")?;
    let split = bytes.len() / 2;
    file.write_all(&bytes[..split])?;
    dir.check_fault("snapshot.partial_write")?;
    file.write_all(&bytes[split..])?;
    dir.check_fault("snapshot.file_sync")?;
    file.sync_all()?;
    // Verify staged bytes, not just the encoder's buffer, before selection.
    let mut staged = Vec::new();
    dir.open_read(&temp)?
        .take(bytes.len() as u64 + 1)
        .read_to_end(&mut staged)?;
    logical_snapshot::decode(
        &staged,
        context,
        &digest,
        selene_core::logical::Limits::default().bytes,
    )
    .map_err(|e| crate::logical_stream::StreamError::Preparation(Box::new(e)))?;
    dir.check_fault("snapshot.publish")?;
    dir.publish_new(Path::new(&temp), Path::new(&name))?;
    dir.check_fault("snapshot.dir_sync")?;
    dir.sync()?;
    Ok(SnapshotDescriptor {
        name,
        bytes: bytes.len() as u64,
        digest,
        boundary: context.boundary,
        publication: context.publication,
    })
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;
