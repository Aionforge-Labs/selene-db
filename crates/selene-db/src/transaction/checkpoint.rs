use super::*;
use crate::{CheckpointOutcome, DurableStatus, StorageError, StorageErrorKind, StoragePhase};
use selene_core::logical::Limits;

impl DatabaseInner {
    pub(crate) fn prune(&self) -> std::result::Result<crate::PruneOutcome, StorageError> {
        self.with_mutation_reservation(|_reservation| {
            let mut durable = self.transactions.durable.lock();
            let authority = durable.as_mut().ok_or_else(|| {
                StorageError::new(
                    StoragePhase::Prune,
                    StorageErrorKind::InMemory,
                    std::io::Error::other("memory-only database"),
                )
            })?;
            authority
                .wal
                .prune()
                .map(Into::into)
                .map_err(|e| StorageError::stream(StoragePhase::Prune, e))
        })
    }
    pub(super) fn lock_writer(&self) -> MutexGuard<'_, ()> {
        #[cfg(test)]
        if let Some(guard) = self.transactions.writer.try_lock() {
            return guard;
        }
        #[cfg(test)]
        if let Some(blocked) = self.mutation_blocked.lock().take() {
            blocked.send(()).unwrap();
        }
        self.transactions.writer.lock()
    }
    pub(crate) fn durable_status(&self) -> Option<DurableStatus> {
        let authority = self.transactions.durable.lock();
        let authority = authority.as_ref()?;
        let position = authority.progress().synchronized;
        Some(DurableStatus {
            position: position.into(),
            digest: position.digest,
            fenced: authority.is_fenced(),
        })
    }

    pub(crate) fn checkpoint(&self) -> std::result::Result<CheckpointOutcome, StorageError> {
        self.with_mutation_reservation(|_reservation| {
            let started = std::time::Instant::now();
            let result = {
                let mut durable = self.transactions.durable.lock();
                let authority = durable.as_mut().ok_or_else(|| {
                    StorageError::new(
                        StoragePhase::Checkpoint,
                        StorageErrorKind::InMemory,
                        std::io::Error::other("memory-only database"),
                    )
                })?;
                if authority.is_fenced() {
                    return Err(StorageError::new(
                        StoragePhase::Checkpoint,
                        StorageErrorKind::Fenced,
                        std::io::Error::other("durable owner fenced"),
                    ));
                }
                let state = self.state.load_full();
                #[cfg(test)]
                if let Some((pinned, resume)) = self.checkpoint_pause.lock().take() {
                    pinned.send(state.publication).unwrap();
                    resume.recv().unwrap();
                }
                let progress = authority.progress();
                if state.publication != progress.synchronized.sequence {
                    return Err(StorageError::invalid(
                        StoragePhase::Checkpoint,
                        "live publication and durable sequence differ",
                    ));
                }
                let catalog = crate::CatalogReadSnapshot {
                    state: Arc::clone(&state),
                }
                .logical_catalog()
                .map_err(|e| {
                    StorageError::new(StoragePhase::Checkpoint, StorageErrorKind::InvalidState, e)
                })?;
                let graphs: Vec<_> = state.graphs.values().map(|g| g.graph.read()).collect();
                let views: Vec<_> = graphs.iter().map(AsRef::as_ref).collect();
                let body = selene_graph::logical_transaction::encode_checkpoint(
                    &catalog,
                    &state.graph_types,
                    &views,
                    Limits::default(),
                )
                .map_err(|e| StorageError::codec(StoragePhase::Checkpoint, e))?;
                // Validate the actual pinned image, not a possibly stale derived candidate.
                let _validated = selene_graph::logical_transaction::ReplayState::from_checkpoint(
                    &body,
                    Limits::default(),
                )
                .map_err(|e| StorageError::codec(StoragePhase::Checkpoint, e))?;
                authority
                    .wal
                    .checkpoint(&body, state.publication)
                    .map(CheckpointOutcome::from)
                    .map_err(|e| StorageError::stream(StoragePhase::Checkpoint, e))
            };
            // Include destruction of temporary images and views in the held region.
            result.map(|mut outcome| {
                outcome.write_reservation_elapsed = started.elapsed();
                outcome
            })
        })
    }
}
