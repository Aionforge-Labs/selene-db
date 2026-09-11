//! Explicit cleanup evidence, separate from checkpoint publication.

use super::*;

/// Why an artifact remains named after explicit maintenance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionReason {
    /// Required by CURRENT.
    Selected,
    /// Required by the previous completed checkpoint.
    History,
    /// Required by an active artifact reader in this or another process.
    Reader,
    /// Coordination entry; never deleted.
    Coordination,
    /// Not proved obsolete, or cleanup has not durably completed.
    Deferred,
}

/// Actual named artifact bytes, not logical data size or resident memory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactBytes {
    /// Directory-relative diagnostic name, not a lease.
    pub name: String,
    /// Native regular-file length at maintenance selection.
    pub bytes: u64,
}

/// Named retained artifact and its controlling reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedArtifact {
    /// Native artifact accounting.
    pub artifact: ArtifactBytes,
    /// Strongest reason (selected, history, reader, coordination, deferred).
    pub reason: RetentionReason,
}

/// Explicit cleanup result. Publication is independent of cleanup debt.
#[derive(Debug, Default)]
pub struct PruneReport {
    /// Successfully unlinked artifacts whose directory removals were synchronized.
    pub removed: Vec<ArtifactBytes>,
    /// Artifacts still required, deferred, or not proved durably removed.
    pub retained: Vec<RetainedArtifact>,
    /// Cleanup failure after planning; inspect retained/deferred evidence before retry.
    pub cleanup_error: Option<StreamError>,
}

impl LogicalWal {
    /// Explicitly retain the latest two completed checkpoints and every reader
    /// lease, then reclaim only proved obsolete artifacts. Never runs automatically.
    pub fn prune(&mut self) -> Result<PruneReport, StreamError> {
        if self.fenced {
            return Err(crate::PersistError::Control(crate::ControlError::RequiresReopen).into());
        }
        let epoch = ManifestEpochGuard::acquire(&self.authority)?;
        crate::control::logical::prune(&epoch, &self.selected)
    }
}
