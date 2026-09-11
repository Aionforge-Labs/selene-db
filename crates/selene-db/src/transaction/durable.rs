//! Private format-2 composition. Public durable construction/reopen belongs to PR05.

use super::*;
use selene_graph::logical_transaction::ReplayState;
use selene_persist::{
    logical_frame::Compression,
    logical_stream::{
        CommitFailure, CommitPhase, Durability, LogicalWal, PreparedGroup, StreamError,
    },
};

/// Derived semantic preflight, never a second publication authority or provider voter.
pub(super) struct DurableAuthority {
    pub(super) wal: LogicalWal,
    pub(super) replay: ReplayState,
}

impl DurableAuthority {
    pub(super) fn progress(&self) -> selene_persist::logical_stream::Progress {
        self.wal.progress()
    }
    pub(super) fn is_fenced(&self) -> bool {
        self.wal.is_fenced()
    }
}

pub(super) struct PreparedCommit {
    pub(super) group: PreparedGroup,
    pub(super) replay: ReplayState,
}

pub(super) fn canceled(error: Error, wal: &DurableAuthority) -> Error {
    Error::durable_failure(Box::new(CommitFailure {
        phase: CommitPhase::Prepare,
        durability: Durability::Canceled,
        progress: wal.progress(),
        candidate: None,
        source: StreamError::Preparation(Box::new(error)),
        cleanup: None,
    }))
}

pub(super) fn prepare(
    authority: &DurableAuthority,
    draft: &DatabaseDraft,
    base: &Arc<DatabaseState>,
) -> Result<PreparedCommit> {
    let bytes = draft
        .logical_transaction(base)
        .and_then(|tx| tx.encode(selene_core::logical::Limits::default()))
        .map_err(Error::invalid_graph_type_source)?;
    // Encoding alone does not prove semantic replay or its cumulative resource bounds.
    let replay = authority
        .replay
        .apply_body(&bytes, selene_core::logical::Limits::default())
        .map_err(Error::invalid_graph_type_source)?;
    let group = authority
        .wal
        .prepare(
            &[&bytes],
            Compression::Auto,
            selene_core::logical::Limits::default().bytes,
        )
        .map_err(Error::invalid_graph_type_source)?;
    Ok(PreparedCommit { group, replay })
}

impl MutationCoordinator {
    #[allow(
        dead_code,
        reason = "private construction seam consumed by F02-PR05; exercised by real-file tests"
    )]
    pub(crate) fn with_wal(wal: LogicalWal, replay: ReplayState) -> Self {
        Self {
            writer: Mutex::new(()),
            durable: Mutex::new(Some(DurableAuthority { wal, replay })),
        }
    }
}

#[cfg(test)]
#[path = "durable_tests.rs"]
mod tests;
