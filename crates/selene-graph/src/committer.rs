//! Sole in-memory snapshot publisher. Seal order, not queue arrival, is authority.
//!
//! Callers build under the write lock, allocate a sequence only after fallible
//! preparation, release the lock, then submit. The committer never takes that
//! lock. Maintenance and commits share this same ordered queue. No WAL, audit,
//! provider voting, group flush, or checkpoint authority exists here.

use crate::{
    GraphError, GraphResult, IndexProvider, SeleneGraph,
    write_txn::{CommitOutcome, SealedCommit},
};
use arc_swap::ArcSwap;
use std::{
    collections::BTreeMap,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread::JoinHandle,
};

const WORK_CHANNEL_CAPACITY: usize = 1024;

pub(crate) enum Work {
    Commit {
        sealed: SealedCommit,
        reply: SyncSender<GraphResult<CommitOutcome>>,
    },
    Compact {
        seal_seq: u64,
        dense: Arc<SeleneGraph>,
        report: crate::CompactionReport,
        reply: SyncSender<GraphResult<crate::CompactionReport>>,
    },
    VectorIndexRebuild {
        seal_seq: u64,
        rebuilt: Arc<SeleneGraph>,
        report: crate::VectorIndexRebuildReport,
        reply: SyncSender<GraphResult<crate::VectorIndexRebuildReport>>,
    },
}
impl Work {
    pub(crate) fn seal_seq(&self) -> u64 {
        match self {
            Self::Commit { sealed, .. } => sealed.seal_seq,
            Self::Compact { seal_seq, .. } | Self::VectorIndexRebuild { seal_seq, .. } => *seal_seq,
        }
    }
}

pub(crate) struct CommitterHandles {
    pub(crate) snapshot: Arc<ArcSwap<SeleneGraph>>,
    pub(crate) schema_version: Arc<AtomicU64>,
    pub(crate) providers: Arc<[Arc<dyn IndexProvider>]>,
}

#[derive(Clone)]
pub(crate) struct Committer {
    sender: SyncSender<Work>,
    poisoned: Arc<AtomicBool>,
    next_seal_seq: Arc<AtomicU64>,
}
pub(crate) struct CommitterThread {
    sender: Option<SyncSender<Work>>,
    poisoned: Arc<AtomicBool>,
    next_seal_seq: Arc<AtomicU64>,
    join: Mutex<Option<JoinHandle<()>>>,
}
impl CommitterThread {
    pub(crate) fn spawn(handles: CommitterHandles) -> Self {
        let (sender, receiver) = sync_channel(WORK_CHANNEL_CAPACITY);
        let poisoned = Arc::new(AtomicBool::new(false));
        let next_seal_seq = Arc::new(AtomicU64::new(0));
        let thread_poisoned = Arc::clone(&poisoned);
        let join = std::thread::Builder::new()
            .name("selene-committer".into())
            .spawn(move || run_committer(receiver, handles, &thread_poisoned))
            .expect("committer thread spawns");
        Self {
            sender: Some(sender),
            poisoned,
            next_seal_seq,
            join: Mutex::new(Some(join)),
        }
    }
    pub(crate) fn handle(&self) -> Committer {
        Committer {
            sender: self.sender.clone().expect("committer sender live"),
            poisoned: Arc::clone(&self.poisoned),
            next_seal_seq: Arc::clone(&self.next_seal_seq),
        }
    }
}
impl Drop for CommitterThread {
    fn drop(&mut self) {
        // WriteTxn handles borrow SharedGraph and are gone before its drop.
        self.sender = None;
        if let Some(join) = self.join.lock().expect("committer join lock").take() {
            let _ = join.join();
        }
    }
}
impl Committer {
    // Called under the graph write lock; that lock orders seals, not this atomic.
    pub(crate) fn next_seal_seq(&self) -> u64 {
        self.next_seal_seq.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn submit_commit(&self, sealed: SealedCommit) -> GraphResult<CommitOutcome> {
        self.enqueue_commit(sealed)?
            .recv()
            .map_err(|_| committer_dead())?
    }
    fn enqueue_commit(
        &self,
        sealed: SealedCommit,
    ) -> GraphResult<Receiver<GraphResult<CommitOutcome>>> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(committer_dead());
        }
        let (reply, receiver) = sync_channel(1);
        self.sender
            .send(Work::Commit { sealed, reply })
            .map_err(|_| committer_dead())?;
        Ok(receiver)
    }
    #[cfg(test)]
    pub(crate) fn submit_commit_async_for_test(
        &self,
        sealed: SealedCommit,
    ) -> GraphResult<Receiver<GraphResult<CommitOutcome>>> {
        self.enqueue_commit(sealed)
    }
    pub(crate) fn submit_compact(
        &self,
        seal_seq: u64,
        dense: Arc<SeleneGraph>,
        report: crate::CompactionReport,
    ) -> GraphResult<crate::CompactionReport> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(committer_dead());
        }
        let (reply, receiver) = sync_channel(1);
        self.sender
            .send(Work::Compact {
                seal_seq,
                dense,
                report,
                reply,
            })
            .map_err(|_| committer_dead())?;
        receiver.recv().map_err(|_| committer_dead())?
    }
    pub(crate) fn submit_vector_index_rebuild(
        &self,
        seal_seq: u64,
        rebuilt: Arc<SeleneGraph>,
        report: crate::VectorIndexRebuildReport,
    ) -> GraphResult<crate::VectorIndexRebuildReport> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(committer_dead());
        }
        let (reply, receiver) = sync_channel(1);
        self.sender
            .send(Work::VectorIndexRebuild {
                seal_seq,
                rebuilt,
                report,
                reply,
            })
            .map_err(|_| committer_dead())?;
        receiver.recv().map_err(|_| committer_dead())?
    }
}

pub(crate) fn committer_dead() -> GraphError {
    GraphError::IndeterminateOutcome { reason: "memory graph publication failed; discard this runtime and inspect its last published snapshot".into() }
}
fn run_committer(receiver: Receiver<Work>, handles: CommitterHandles, poisoned: &Arc<AtomicBool>) {
    let mut next_publish_seq = 0;
    let mut reorder = BTreeMap::new();
    while let Ok(work) = receiver.recv() {
        reorder.insert(work.seal_seq(), work);
        while let Some(work) = reorder.remove(&next_publish_seq) {
            match work {
                Work::Commit { sealed, reply } => {
                    let result = run_protected(|| {
                        crate::write_txn::publish_sealed(
                            sealed,
                            &handles.snapshot,
                            &handles.schema_version,
                            &handles.providers,
                        )
                    });
                    let _ = reply.send(unwrap_protected(result, poisoned));
                }
                Work::Compact {
                    dense,
                    report,
                    reply,
                    ..
                } => {
                    let result = run_protected(|| {
                        handles.snapshot.store(dense);
                        Ok(report)
                    });
                    let _ = reply.send(unwrap_protected(result, poisoned));
                }
                Work::VectorIndexRebuild {
                    rebuilt,
                    report,
                    reply,
                    ..
                } => {
                    let result = run_protected(|| {
                        handles.snapshot.store(rebuilt);
                        Ok(report)
                    });
                    let _ = reply.send(unwrap_protected(result, poisoned));
                }
            }
            next_publish_seq += 1;
            if poisoned.load(Ordering::Acquire) {
                // Dropping queued reply senders releases every waiter with an
                // indeterminate result. No later sealed snapshot may publish.
                return;
            }
        }
    }
}
pub(crate) fn run_protected<T>(
    body: impl FnOnce() -> GraphResult<T>,
) -> Result<GraphResult<T>, Box<dyn std::any::Any + Send>> {
    std::panic::catch_unwind(AssertUnwindSafe(body))
}
pub(crate) fn unwrap_protected<T>(
    result: Result<GraphResult<T>, Box<dyn std::any::Any + Send>>,
    poisoned: &Arc<AtomicBool>,
) -> GraphResult<T> {
    match result {
        Ok(Ok(value)) => Ok(value),
        failed => {
            // A later seal may already contain this mutation. Never attempt a
            // selective rollback or publish a later divergent snapshot.
            poisoned.store(true, Ordering::Release);
            let reason = match failed {
                Ok(Err(error)) => error.to_string(),
                Err(payload) => crate::panic_payload::describe(&payload),
                Ok(Ok(_)) => unreachable!(),
            };
            tracing::error!(%reason, "memory graph publication failed; runtime fenced");
            Err(GraphError::IndeterminateOutcome { reason })
        }
    }
}

#[cfg(test)]
mod tests;
