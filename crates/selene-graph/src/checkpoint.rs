//! Ordered graph checkpointing through the per-graph committer.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;

use selene_persist::{
    PersistError, SectionCompression, SnapshotBuilder, SnapshotConfig, WalRotationOutcome,
    snapshot_path,
};

use crate::core_provider::CoreProvider;
use crate::index_provider::{IndexProvider, ProviderError};
use crate::{GraphError, GraphResult};

/// Configuration for one coordinated graph checkpoint.
///
/// The WAL directory, snapshot sequence, and durability barriers are owned by
/// the live graph's persistence protocol. Callers choose only the section
/// compression mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointConfig {
    /// Compression applied independently to each snapshot section.
    pub compression: SectionCompression,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            compression: SectionCompression::DEFAULT,
        }
    }
}

/// Result of a successful coordinated graph checkpoint.
///
/// The paths identify the epoch published by this call; they are not retention
/// leases. Before opening or copying them later, acquire a
/// [`selene_persist::PersistenceReadGuard`], re-read its authoritative
/// MANIFEST, and verify both the canonical directory and snapshot sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointOutcome {
    /// Physical checkpoint epoch covered by the durable snapshot.
    ///
    /// Coordinated checkpoints reserve this fresh WAL sequence with a typed
    /// watermark; it is independent of logical graph generation.
    pub snapshot_sequence: u64,
    /// Final path of the durable snapshot.
    pub snapshot_path: PathBuf,
    /// Detail from the MANIFEST-backed WAL operation.
    ///
    /// Coordinated checkpoints create a fresh epoch and therefore return
    /// [`WalRotationOutcome::Rotated`]. `AlreadyCurrent` remains part of the
    /// lower-level rotation type for exact crash retries.
    pub rotation: WalRotationOutcome,
}

/// Result plus the committer-health decision for one checkpoint work item.
pub(crate) struct CheckpointExecution {
    pub(crate) result: GraphResult<CheckpointOutcome>,
    pub(crate) poison_committer: bool,
}

/// Encode every provider, then append a physical watermark and rotate the WAL.
///
/// Preparation failures happen before the MANIFEST protocol begins and are
/// therefore safe to report without consuming a WAL sequence or poisoning the
/// committer. The combined watermark/rotation phase normally has an ambiguous
/// writer/commit-point state on error and requires reopen; a typed pre-commit
/// artifact collision leaves the active epoch unchanged and the ordered writer
/// usable at its newly reserved physical sequence.
pub(crate) fn execute(
    core: &CoreProvider,
    providers: &[Arc<dyn IndexProvider>],
    generation: u64,
    config: CheckpointConfig,
) -> CheckpointExecution {
    let prepared = std::panic::catch_unwind(AssertUnwindSafe(|| {
        prepare(core, providers, generation, config)
    }));
    let (builder, snapshot_sequence, final_snapshot_path) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => {
            return CheckpointExecution {
                result: Err(error),
                poison_committer: false,
            };
        }
        Err(payload) => {
            return CheckpointExecution {
                result: Err(provider_panic("checkpoint preparation", &payload)),
                poison_committer: false,
            };
        }
    };

    let rotated = std::panic::catch_unwind(AssertUnwindSafe(|| {
        core.rotate_checkpoint_with_watermark(builder)
    }));
    match rotated {
        Ok(Ok(rotation)) => CheckpointExecution {
            result: Ok(CheckpointOutcome {
                snapshot_sequence,
                snapshot_path: final_snapshot_path,
                rotation,
            }),
            poison_committer: false,
        },
        Ok(Err(error)) => {
            let poison_committer = rotation_error_requires_reopen(&error);
            CheckpointExecution {
                result: Err(error),
                poison_committer,
            }
        }
        // Built as the indeterminate outcome directly rather than left for
        // `publish_solo` to reclassify: a panic here has no source error whose
        // text is worth composing, and the variant's own message already tells
        // the caller to reopen and reconcile.
        Err(payload) => CheckpointExecution {
            result: Err(GraphError::IndeterminateOutcome {
                reason: format!(
                    "checkpoint WAL watermark/rotation panicked: {}",
                    crate::panic_payload::describe(&payload)
                ),
            }),
            poison_committer: true,
        },
    }
}

fn rotation_error_requires_reopen(error: &GraphError) -> bool {
    !matches!(
        error,
        GraphError::Persist(PersistError::ArtifactIdentityMismatch { .. })
    )
}

fn prepare(
    core: &CoreProvider,
    providers: &[Arc<dyn IndexProvider>],
    generation: u64,
    config: CheckpointConfig,
) -> GraphResult<(SnapshotBuilder, u64, PathBuf)> {
    let target = core.checkpoint_target()?;
    let snapshot_sequence =
        target
            .sequence
            .checked_add(1)
            .ok_or(PersistError::WalSequenceExhausted {
                last_sequence: target.sequence,
            })?;
    let final_snapshot_path = snapshot_path(&target.dir, snapshot_sequence);
    let mut builder = SnapshotBuilder::new_in(
        &target.directory,
        SnapshotConfig {
            dir: target.dir,
            sequence: snapshot_sequence,
            compression: config.compression,
            fsync: true,
        },
    );
    let _callback_guard = crate::reentry::FanoutGuard::enter();
    for provider in providers {
        let provider_tag = provider.provider_tag();
        for sub_tag in provider.declared_sub_tags() {
            let bytes = provider.write_section_at_generation(*sub_tag, generation)?;
            builder.add_section(provider_tag.0, sub_tag.0, bytes)?;
        }
    }
    Ok((builder, snapshot_sequence, final_snapshot_path))
}

fn provider_panic(phase: &str, payload: &Box<dyn std::any::Any + Send>) -> GraphError {
    GraphError::Provider(ProviderError::SerializationFailed {
        reason: format!(
            "{phase} panicked: {}",
            crate::panic_payload::describe(payload)
        ),
    })
}

#[cfg(test)]
#[path = "checkpoint/tests.rs"]
mod tests;
