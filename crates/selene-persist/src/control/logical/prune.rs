//! Selection/proof/plan precede every unlink. No fallback selection or repair.

use super::*;
use crate::logical_stream::{
    ArtifactBytes, LogicalReader, Position, PruneReport, RetainedArtifact, RetentionReason,
    StreamError,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MAX_ARTIFACTS: usize = 4096;

struct Root {
    name: String,
    selected: Option<Selected>,
    leased: bool,
}

fn dependencies(root: &Selected) -> Vec<String> {
    let mut names = vec![root.manifest_name().into(), root.log_name()];
    if let Some(s) = &root.checkpoint {
        names.push(s.name.clone());
    }
    names
}

fn inspect(dir: &StoreDirectory, name: &str, current: &Selected) -> Result<Root, StreamError> {
    let bytes = read_bounded(dir, Path::new(name))?;
    // Decode each distinct envelope explicitly; unknown/corrupt manifests stop
    // planning, rather than concealing unknown dependencies during deletion.
    let (metadata, empty) = if bytes.starts_with(b"SLEM") {
        (EmptyManifest::decode(&bytes)?, true)
    } else if bytes.starts_with(b"SLDM") {
        (checkpoint::decode_manifest(&bytes)?.metadata, false)
    } else if bytes.starts_with(b"SLRM") {
        (rotation::decode(&bytes)?.metadata, false)
    } else {
        (decode(&bytes)?.metadata, false)
    };
    if metadata.store_id != current.metadata.store_id
        || metadata.epoch != current.metadata.epoch
        || metadata.identity != current.metadata.identity
        || metadata.generation.name() != name
    {
        return Err(PersistError::Control(ControlError::Lineage).into());
    }
    let selector = CurrentSelector::from_manifest(&metadata, *blake3::hash(&bytes).as_bytes());
    // A writable, separately opened description makes the exclusive probe valid
    // on both supported platforms. Never relock or unlock a cloned description.
    let probe = dir.open_write(Path::new(name))?;
    let leased = match probe.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    };
    drop(probe); // epoch still excludes registration of a new reader lease
    Ok(Root {
        name: name.into(),
        selected: if empty {
            None
        } else {
            Some(select_root(dir, selector, &current.metadata.identity)?)
        },
        leased,
    })
}

/// Verify all available referenced bytes. Missing obsolete dependencies can be
/// remnants of an interrupted explicit prune; required roots never allow this.
pub(super) fn verify(
    dir: &StoreDirectory,
    root: &Selected,
    end: Option<Position>,
    required: bool,
) -> Result<(), StreamError> {
    let log_present = dir.contains(root.log_name())?;
    if !log_present {
        if required {
            return Err(StreamError::Protocol("required WAL missing"));
        }
        // A remaining snapshot is still validated before it can be deleted.
        if let Some(s) = &root.checkpoint
            && dir.contains(&s.name)?
        {
            verify_snapshot(dir, root)?;
        }
        return Ok(());
    }
    let file = dir.open_read(root.log_name())?;
    if let Some(end) = end
        && file.metadata()?.len() != end.offset
    {
        return Err(StreamError::Protocol("sealed WAL length"));
    }
    let mut reader =
        LogicalReader::from_selection(dir, file, root.clone(), crate::logical_frame::MAX_PAYLOAD)?;
    if let Some(s) = &root.checkpoint {
        if dir.contains(&s.name)? {
            reader.snapshot_body()?;
        } else if required {
            return Err(StreamError::Protocol("required snapshot missing"));
        }
    }
    let snapshot = root.checkpoint.as_ref().map(|s| s.boundary);
    let mut boundary_seen = root.rotation.is_some() || snapshot.is_none_or(|p| p == root.base());
    while reader.next_body()?.is_some() {
        if snapshot == Some(reader.position()) {
            boundary_seen = true;
        }
    }
    if reader.incomplete_tail() || !boundary_seen || end.is_some_and(|p| p != reader.position()) {
        return Err(StreamError::Protocol(
            "incomplete or mismatched required WAL boundary",
        ));
    }
    Ok(())
}

fn verify_snapshot(dir: &StoreDirectory, root: &Selected) -> Result<(), StreamError> {
    // The body loader does not consume this file; use an independently opened
    // manifest as its unused stream when an obsolete WAL was already removed.
    let reader = LogicalReader::from_selection(
        dir,
        dir.open_read(root.manifest_name())?,
        root.clone(),
        crate::logical_frame::MAX_PAYLOAD,
    )?;
    reader.snapshot_body().map(|_| ())
}

pub(crate) fn prune(
    guard: &ManifestEpochGuard,
    expected: &Selected,
) -> Result<PruneReport, StreamError> {
    let dir = guard.directory();
    // The exclusive epoch keeps this admitted inventory stable through selection
    // and planning; do not collect an unbounded list before checking the cap.
    let names = dir.entries_bounded(MAX_ARTIFACTS)?;
    let current = select_in(dir, &expected.metadata.identity)?;
    if current.selector != expected.selector {
        return Err(PersistError::Control(ControlError::Stale).into());
    }
    let mut inventory = BTreeMap::new();
    let mut roots = Vec::new();
    for name in names {
        let text = name.to_str().ok_or_else(|| {
            PersistError::Control(ControlError::MixedArtifacts(name.clone().into()))
        })?;
        let metadata = dir
            .regular_metadata(Path::new(&name))?
            .ok_or(PersistError::Control(ControlError::Lineage))?;
        inventory.insert(text.to_owned(), metadata.len);
        if is_manifest_name(text) {
            roots.push(inspect(dir, text, &current)?);
        }
    }
    let mut kept = BTreeMap::new();
    let mut candidates = BTreeSet::new();
    let mut history = None;
    if let Some(rotation) = &current.rotation {
        let previous = select_root(
            dir,
            rotation.previous.selector.clone(),
            &current.metadata.identity,
        )?;
        if previous.base() != rotation.snapshot_base {
            return Err(PersistError::Control(ControlError::Lineage).into());
        }
        verify(dir, &previous, Some(rotation.previous.end), true)?;
        history = Some(previous);
    } else if current.checkpoint.is_some() {
        // PR05 has no persisted completed-root list. Its parent is the immediately
        // preceding completed selection when that selection contains a checkpoint.
        if let Some(parent) = &current.metadata.parent
            && let Some(root) = roots.iter().filter_map(|r| r.selected.as_ref()).find(|r| {
                r.metadata.generation == parent.generation
                    && r.selector.digest == parent.digest
                    && r.checkpoint.is_some()
            })
        {
            history = Some(root.clone());
        }
    }
    verify(dir, &current, None, true)?;
    for root in &roots {
        let reason = if root.name == current.manifest_name() {
            Some(RetentionReason::Selected)
        } else if history
            .as_ref()
            .is_some_and(|p| root.name == p.manifest_name())
        {
            Some(RetentionReason::History)
        } else if root.leased {
            Some(RetentionReason::Reader)
        } else {
            None
        };
        if let Some(selected) = &root.selected {
            // Filename order grants no authority. A complete unselected staged
            // root can only become obsolete after CURRENT's durability is proved
            // below; malformed or ambiguous roots instead stop all deletion.
            verify(dir, selected, None, reason.is_some())?;
            for name in dependencies(selected) {
                if let Some(reason) = reason {
                    retain(&mut kept, name, reason);
                } else {
                    candidates.insert(name);
                }
            }
        } else if let Some(reason) = reason {
            retain(&mut kept, root.name.clone(), reason);
        } else {
            candidates.insert(root.name.clone());
        }
    }
    retain(
        &mut kept,
        CURRENT_FILE_NAME.into(),
        RetentionReason::Selected,
    );
    for name in [crate::STORE_LOCK_FILE_NAME, crate::MANIFEST_LOCK_FILE_NAME] {
        retain(&mut kept, name.into(), RetentionReason::Coordination);
    }
    // Reestablish CURRENT and every current dependency after a possibly uncertain
    // prior replacement. WAL-only synchronization on reopen is NOT sufficient.
    for name in dependencies(&current)
        .into_iter()
        .chain(std::iter::once(CURRENT_FILE_NAME.into()))
    {
        dir.check_fault("prune.establish_file_sync")?;
        dir.open_read(&name)?.sync_all()?;
    }
    dir.check_fault("prune.establish_dir_sync")?;
    dir.sync()?;
    let mut report = PruneReport::default();
    // Dependencies first, manifests last: partial cleanup leaves descriptors to
    // classify remaining files on the next explicit call.
    let mut pending: Vec<_> = inventory
        .into_iter()
        .filter_map(|(name, bytes)| {
            let artifact = ArtifactBytes {
                name: name.clone(),
                bytes,
            };
            if let Some(reason) = kept.get(&name) {
                report.retained.push(RetainedArtifact {
                    artifact,
                    reason: *reason,
                });
                None
            } else if candidates.contains(&name) {
                Some(artifact)
            } else {
                report.retained.push(RetainedArtifact {
                    artifact,
                    reason: RetentionReason::Deferred,
                });
                None
            }
        })
        .collect();
    pending.sort_by_key(|a| is_manifest_name(&a.name));
    for artifact in pending {
        if report.cleanup_error.is_none() {
            let result = (|| -> Result<(), StreamError> {
                dir.check_fault("prune.unlink")?;
                dir.remove(Path::new(&artifact.name))?;
                dir.check_fault("prune.dir_sync")?;
                dir.sync()?;
                Ok(())
            })();
            match result {
                Ok(()) => {
                    report.removed.push(artifact);
                    continue;
                }
                Err(error) => report.cleanup_error = Some(error),
            }
        }
        report.retained.push(RetainedArtifact {
            artifact,
            reason: RetentionReason::Deferred,
        });
    }
    Ok(report)
}

fn retain(kept: &mut BTreeMap<String, RetentionReason>, name: String, reason: RetentionReason) {
    let rank = |r| match r {
        RetentionReason::Selected => 0,
        RetentionReason::History => 1,
        RetentionReason::Reader => 2,
        RetentionReason::Coordination => 3,
        RetentionReason::Deferred => 4,
    };
    kept.entry(name)
        .and_modify(|old| {
            if rank(reason) < rank(*old) {
                *old = reason;
            }
        })
        .or_insert(reason);
}
