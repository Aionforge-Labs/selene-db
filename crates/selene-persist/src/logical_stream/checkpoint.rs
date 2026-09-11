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
}

impl LogicalWal {
    /// Snapshot the caller's pinned semantic image at the established live boundary.
    /// The caller retains its serial publication reservation for the entire call.
    /// This keeps the original segment and all old artifacts; no rotation or prune.
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
        let selected = crate::control::logical::publish_checkpoint(
            &epoch,
            &self.selected,
            body,
            SnapshotContext {
                boundary: progress.synchronized,
                publication,
            },
        )?;
        self.selected = selected;
        self.fenced = false;
        Ok(self.checkpoint_info().expect("selected snapshot"))
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
        })
    }
}
