use super::*;
use crate::logical_snapshot::SnapshotContext;

/// Exact immutable artifact and transaction boundary selected by a successful checkpoint.
#[derive(Clone, Debug)]
pub struct CheckpointInfo {
    /// Immutable manifest generation, independent of graph/catalog generations.
    pub generation: u64,
    /// Diagnostic artifact name; not a retention lease.
    pub name: String,
    /// Complete encoded snapshot file bytes.
    pub bytes: u64,
    /// Full snapshot integrity digest.
    pub digest: [u8; 32],
    /// Exact complete WAL boundary covered by this checkpoint.
    pub boundary: Position,
    /// Facade publication ordinal of the encoded view.
    pub publication: u64,
    /// Old-root validation, seal, fresh-segment and control publication time,
    /// excluding new-image encoding/I/O. Zero for metadata-only observations.
    pub rotation_elapsed: std::time::Duration,
}

impl LogicalWal {
    /// Snapshot the caller's pinned semantic image at the established live boundary.
    /// The caller retains its serial publication reservation for the entire call.
    /// Subsequent checkpoints rotate to a fresh segment without resetting sequence.
    /// All old artifacts remain until explicit prune; no automatic deletion occurs.
    /// Any publication/I/O error fences this owner; reopen is non-destructive.
    pub fn checkpoint(
        &mut self,
        body: &[u8],
        publication: u64,
    ) -> Result<CheckpointInfo, StreamError> {
        let progress = self.progress;
        if self.fenced
            || progress.written != progress.synchronized
            || (progress.synchronized.sequence != 0
                && progress.published != Some(progress.synchronized))
        {
            return Err(StreamError::Protocol(
                "checkpoint requires an established live boundary",
            ));
        }
        let epoch = ManifestEpochGuard::acquire(&self.authority)?;
        self.fenced = true;
        let context = SnapshotContext {
            boundary: progress.synchronized,
            publication,
        };
        let mut rotation_elapsed = std::time::Duration::ZERO;
        let selected = if self.selected.checkpoint.is_some() {
            let (file, selected, elapsed) =
                crate::control::logical::publish_rotation(&epoch, &self.selected, body, context)?;
            rotation_elapsed = elapsed;
            epoch
                .directory()
                .check_fault("rotation.handle_swap")
                .map_err(|source| {
                    crate::PersistError::Control(crate::ControlError::PublicationUncertain {
                        source: Box::new(source),
                    })
                })?;
            self.file = file;
            let base = selected.base();
            self.progress.written = base;
            self.progress.synchronized = base;
            self.progress.published = Some(base);
            // Rotation records no transaction and manufactures no acknowledgment.
            selected
        } else {
            crate::control::logical::publish_checkpoint(&epoch, &self.selected, body, context)?
        };
        self.selected = selected;
        self.fenced = false;
        let mut info = self.checkpoint_info().expect("selected snapshot");
        info.rotation_elapsed = rotation_elapsed;
        Ok(info)
    }

    /// Last selected checkpoint, if this stream has a full self-contained image.
    pub fn checkpoint_info(&self) -> Option<CheckpointInfo> {
        let s = self.selected.checkpoint.as_ref()?;
        Some(CheckpointInfo {
            generation: self.selected.metadata.generation().get(),
            name: s.name.clone(),
            bytes: s.bytes,
            digest: s.digest,
            boundary: s.boundary,
            publication: s.publication,
            rotation_elapsed: std::time::Duration::ZERO,
        })
    }
}
