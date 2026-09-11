//! Facade-owned explicit retention reports; no lower storage handles are exposed.
use super::*;

/// Native artifact length captured by explicit maintenance, not resident memory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageArtifact {
    /// Directory-relative diagnostic name. This is not a reader lease.
    pub name: String,
    /// Actual regular-file bytes at planning time.
    pub bytes: u64,
}

/// Controlling retention reason, strongest first when dependencies are shared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionReason {
    /// Required by CURRENT's complete recovery state.
    Selected,
    /// Required by the previous completed checkpoint.
    History,
    /// Required by a live same-process or cross-process artifact reader.
    Reader,
    /// Permanent coordination state.
    Coordination,
    /// Not proved obsolete or not proved durably removed; cleanup debt.
    Deferred,
}

/// Retained bytes with the controlling reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedArtifact {
    /// Artifact accounting.
    pub artifact: StorageArtifact,
    /// Why maintenance has not durably removed this artifact.
    pub reason: RetentionReason,
}

/// Explicit maintenance evidence. A cleanup failure does not undo publication
/// or fence otherwise healthy commits. Deferred entries can include an unlink
/// whose directory synchronization failed; those bytes are not claimed reclaimed.
#[derive(Debug)]
pub struct PruneOutcome {
    /// Artifacts unlinked with successful directory synchronization.
    pub removed: Vec<StorageArtifact>,
    /// Required, unclassified, or not durably removed artifacts.
    pub retained: Vec<RetainedArtifact>,
    /// Concrete cleanup failure, if planning succeeded but cleanup did not finish.
    pub cleanup_error: Option<StorageError>,
}

impl From<selene_persist::logical_stream::PruneReport> for PruneOutcome {
    fn from(report: selene_persist::logical_stream::PruneReport) -> Self {
        let artifact = |a: selene_persist::logical_stream::ArtifactBytes| StorageArtifact {
            name: a.name,
            bytes: a.bytes,
        };
        Self {
            removed: report.removed.into_iter().map(artifact).collect(),
            retained: report
                .retained
                .into_iter()
                .map(|a| {
                    use selene_persist::logical_stream::RetentionReason as R;
                    RetainedArtifact {
                        artifact: artifact(a.artifact),
                        reason: match a.reason {
                            R::Selected => RetentionReason::Selected,
                            R::History => RetentionReason::History,
                            R::Reader => RetentionReason::Reader,
                            R::Coordination => RetentionReason::Coordination,
                            R::Deferred => RetentionReason::Deferred,
                        },
                    }
                })
                .collect(),
            cleanup_error: report
                .cleanup_error
                .map(|e| StorageError::stream(StoragePhase::Prune, e)),
        }
    }
}

impl Database {
    /// Explicitly reclaim obsolete durable artifacts. Retains CURRENT, one
    /// previous completed checkpoint, all their WAL/snapshot dependencies and
    /// every active artifact lease. Checkpoint never calls this automatically.
    ///
    /// Validation or durability-establishment errors occur before any deletion.
    /// Cleanup failures instead return partial progress and debt in the outcome.
    /// No corruption repair, history fallback, age/PID guessing or implicit retry.
    pub fn prune(&self) -> StorageResult<PruneOutcome> {
        self.inner.prune()
    }
}
