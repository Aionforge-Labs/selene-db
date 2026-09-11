//! Checkpoint-coupled rotation. No old segment is modified, truncated or renamed.

use super::*;
use crate::{
    logical_snapshot::SnapshotContext,
    logical_stream::{Position, StreamError},
};

/// A completed prior selection and the exact end sealed by its successor.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PreviousRoot {
    pub selector: CurrentSelector,
    pub end: Position,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Rotation {
    pub base: Position,
    pub snapshot_base: Position,
    pub previous: PreviousRoot,
}

#[derive(Serialize, Deserialize)]
pub(super) struct RotatingManifest {
    pub(super) metadata: EmptyManifest,
    snapshot: SnapshotDescriptor,
    rotation: Rotation,
}

pub(super) fn log_name(segment: [u8; 32]) -> String {
    format!("WAL-{}.logical", blake3::Hash::from(segment).to_hex())
}

pub(super) fn is_log_name(name: &str) -> bool {
    name.strip_prefix("WAL-")
        .and_then(|s| s.strip_suffix(".logical"))
        .is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

pub(super) fn decode(bytes: &[u8]) -> PersistResult<RotatingManifest> {
    let m: RotatingManifest = codec::decode(bytes, *b"SLRM")?;
    m.metadata.validate()?;
    m.rotation.previous.selector.validate()?;
    let r = &m.rotation;
    let p = m.snapshot.boundary;
    let b = r.snapshot_base;
    if r.base.store != m.metadata.store_id
        || r.base.epoch != m.metadata.epoch
        || p.store != r.base.store
        || p.epoch != r.base.epoch
        || b.store != p.store
        || b.epoch != p.epoch
        || b.segment != p.segment
        || b.offset != 0
        || b.digest == [0; 32]
        || b.segment == [0; 32]
        || p.sequence < b.sequence
        || p.digest == [0; 32]
        || (p.sequence == b.sequence) != (p.offset == 0)
        || (p.offset == 0 && p.digest != b.digest)
        || r.base.sequence != p.sequence
        || r.base.sequence == u64::MAX
        || r.base.offset != 0
        || r.base.segment == [0; 32]
        || r.base.segment == p.segment
        || r.base.digest != r.base.segment
        || r.previous.end != p
        || r.previous.selector.store_id != p.store
        || r.previous.selector.epoch != p.epoch
        || r.previous.selector.generation.get() >= m.metadata.generation.get()
        || m.metadata.parent.as_ref().is_none_or(|parent| {
            parent.generation != r.previous.selector.generation
                || parent.digest != r.previous.selector.digest
        })
        || m.snapshot.name != checkpoint::snapshot_name(m.metadata.generation.get())
        || m.snapshot.bytes < crate::logical_snapshot::OVERHEAD as u64
        || m.snapshot.bytes
            > (selene_core::logical::Limits::default().bytes + crate::logical_snapshot::OVERHEAD)
                as u64
    {
        return Err(ControlError::Lineage.into());
    }
    Ok(m)
}

pub(super) fn validate(bytes: &[u8]) -> PersistResult<()> {
    decode(bytes).map(|_| ())
}

pub(super) fn decode_selected(
    bytes: &[u8],
    selector: CurrentSelector,
    expected: &CompatibilityIdentity,
) -> PersistResult<Selected> {
    let m = decode(bytes)?;
    if m.metadata.store_id != selector.store_id
        || m.metadata.epoch != selector.epoch
        || m.metadata.generation != selector.generation
    {
        return Err(ControlError::Lineage.into());
    }
    if &m.metadata.identity != expected {
        return Err(ControlError::Compatibility.into());
    }
    let b = m.rotation.base;
    Ok(Selected {
        context: Context {
            store: b.store,
            epoch: b.epoch,
            segment: b.segment,
            sequence: b.sequence + 1,
            previous: b.digest,
        },
        metadata: m.metadata,
        selector,
        checkpoint: Some(m.snapshot),
        rotation: Some(m.rotation),
    })
}

pub(crate) fn publish_rotation(
    guard: &ManifestEpochGuard,
    previous: &Selected,
    body: &[u8],
    context: SnapshotContext,
) -> Result<(File, Selected, std::time::Duration), StreamError> {
    let dir = guard.directory();
    // Leave room for this attempt's staged and final files so a caller cannot
    // grow past the bounded maintenance inventory merely by deferring prune.
    dir.entries_bounded(prune::MAX_ARTIFACTS - 8)?;
    let current = select_in(dir, &previous.metadata.identity)?;
    if current.selector != previous.selector || previous.checkpoint.is_none() {
        return Err(PersistError::Control(ControlError::Stale).into());
    }
    // A valid in-memory image must not hide damage in the selected recovery root.
    // Verify the exact sealed end before creating any replacement artifacts.
    let validation_started = std::time::Instant::now();
    prune::verify(dir, previous, Some(context.boundary), true)?;
    let validation_elapsed = validation_started.elapsed();
    let generation = previous.metadata.generation.next()?;
    let snapshot = checkpoint::publish_snapshot(dir, generation, body, context)?;
    let started = std::time::Instant::now();
    // The seal is in the successor control, not appended to the old active file:
    // a pre-CURRENT failure must leave that old selector writable on reopen.
    dir.check_fault("rotation.seal")?;
    let old_file = dir.open_read(previous.log_name())?;
    if old_file.metadata()?.len() != context.boundary.offset {
        return Err(StreamError::Protocol(
            "sealed end differs from synchronized boundary",
        ));
    }
    old_file.sync_all()?;
    let mut segment = [0; 32];
    segment[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    segment[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let base = Position {
        segment,
        offset: 0,
        digest: segment,
        ..context.boundary
    };
    let m = RotatingManifest {
        metadata: EmptyManifest {
            generation,
            parent: Some(Parent {
                generation: previous.metadata.generation,
                digest: previous.selector.digest,
            }),
            ..previous.metadata.clone()
        },
        snapshot,
        rotation: Rotation {
            base,
            snapshot_base: previous.base(),
            previous: PreviousRoot {
                selector: previous.selector.clone(),
                end: context.boundary,
            },
        },
    };
    let bytes = codec::encode(&m, *b"SLRM")?;
    validate(&bytes)?;
    let selector = CurrentSelector::from_manifest(&m.metadata, *blake3::hash(&bytes).as_bytes());
    let selected = decode_selected(&bytes, selector, &previous.metadata.identity)?;
    dir.check_fault("rotation.create")?;
    let file = dir.create_new(Path::new(&selected.log_name()))?;
    // No segment header: the zero-byte base is declared by the selected envelope.
    dir.check_fault("rotation.file_sync")?;
    file.sync_all()?;
    dir.check_fault("rotation.dir_sync")?;
    dir.sync()?;
    publish_bytes(guard, &bytes, &selected.selector, false)?;
    Ok((file, selected, validation_elapsed + started.elapsed()))
}

#[cfg(any(test, feature = "test-harness"))]
fn seed_manifest() -> RotatingManifest {
    let mut id = [0; 16];
    id[6] = 0x40;
    id[8] = 0x80;
    let store = StoreId::from_bytes(id).unwrap();
    let epoch = StoreEpoch::new(1).unwrap();
    let metadata = EmptyManifest {
        store_id: store,
        epoch,
        generation: ManifestGeneration(3),
        format: [2, 0],
        identity: CompatibilityIdentity::new("rotation-seed", 1, [7; 32], [17, 0, 0], "binary", 1)
            .unwrap(),
        parent: Some(Parent {
            generation: ManifestGeneration(2),
            digest: [2; 32],
        }),
    };
    let previous = CurrentSelector::from_manifest(&metadata, [3; 32]);
    let boundary = Position {
        store,
        epoch,
        segment: [1; 32],
        sequence: 2,
        offset: 400,
        digest: [9; 32],
    };
    RotatingManifest {
        metadata: EmptyManifest {
            generation: ManifestGeneration(4),
            parent: Some(Parent {
                generation: ManifestGeneration(3),
                digest: [3; 32],
            }),
            ..metadata
        },
        snapshot: SnapshotDescriptor {
            name: checkpoint::snapshot_name(4),
            bytes: 209,
            digest: [6; 32],
            boundary,
            publication: 2,
        },
        rotation: Rotation {
            base: Position {
                segment: [4; 32],
                offset: 0,
                digest: [4; 32],
                ..boundary
            },
            snapshot_base: Position {
                sequence: 0,
                offset: 0,
                digest: [2; 32],
                ..boundary
            },
            previous: PreviousRoot {
                selector: previous,
                end: boundary,
            },
        },
    }
}

#[cfg(feature = "test-harness")]
pub(crate) fn fuzz_payload() -> Vec<u8> {
    postcard::to_stdvec(&seed_manifest()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotating_control_rejects_wrong_base_origin_sequence_and_seal() {
        for damage in [
            "base-offset",
            "base-origin",
            "base-sequence",
            "reused-segment",
            "seal",
            "snapshot-base",
            "future",
        ] {
            let mut m = seed_manifest();
            validate(&codec::encode(&m, *b"SLRM").unwrap()).unwrap();
            match damage {
                "base-offset" => m.rotation.base.offset = 400,
                "base-origin" => m.rotation.base.digest = [8; 32],
                "base-sequence" => m.rotation.base.sequence += 1,
                "reused-segment" => m.rotation.base.segment = m.snapshot.boundary.segment,
                "seal" => m.rotation.previous.end.offset += 1,
                "snapshot-base" => m.rotation.snapshot_base.sequence = 2,
                _ => m.metadata.format = [2, 1],
            }
            assert!(
                validate(&codec::encode(&m, *b"SLRM").unwrap()).is_err(),
                "{damage}"
            );
        }
    }
}
