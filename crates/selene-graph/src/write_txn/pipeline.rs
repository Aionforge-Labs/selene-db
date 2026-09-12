//! In-memory publication and post-publication observer delivery.
use crate::{
    GraphResult, IndexProvider, SeleneGraph,
    write_txn::{CommitOutcome, SealedCommit},
};
use arc_swap::ArcSwap;
use selene_core::{Change, metrics};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

pub(crate) fn publish_sealed(
    sealed: SealedCommit,
    snapshot: &ArcSwap<SeleneGraph>,
    schema_version: &AtomicU64,
    providers: &[Arc<dyn IndexProvider>],
) -> GraphResult<CommitOutcome> {
    #[cfg(test)]
    assert!(!sealed.fail_publish, "synthetic memory publication failure");
    let started = Instant::now();
    let SealedCommit {
        seal_seq: _,
        next_snapshot,
        changes,
        fanout_changes,
        principal,
        schema_changed,
        generation,
        next_node_id,
        next_edge_id,
        warnings,
        ..
    } = sealed;
    #[cfg(debug_assertions)]
    if let Err(reason) = next_snapshot.assert_indexes_consistent() {
        panic!("selene-graph: pre-publish index consistency violation: {reason}");
    }
    snapshot.store(Arc::clone(&next_snapshot));
    // Store before epoch: seeing the new epoch must imply the new snapshot.
    if schema_changed {
        schema_version.fetch_add(1, Ordering::AcqRel);
    }
    let fanout = fanout_changes.as_deref().unwrap_or(&changes);
    {
        let _fanout_guard = crate::reentry::FanoutGuard::enter();
        crate::provider_fanout::notify_providers(providers, generation, fanout);
    }
    metrics::counter_inc(metrics::COMMITS_TOTAL);
    metrics::histogram_record(
        metrics::COMMIT_DURATION_SECONDS,
        started.elapsed().as_secs_f64(),
    );
    metrics::gauge_set(metrics::GRAPH_NODES, next_snapshot.node_count() as f64);
    metrics::gauge_set(metrics::GRAPH_EDGES, next_snapshot.edge_count() as f64);
    Ok(CommitOutcome {
        generation,
        changes,
        principal,
        durable_at: None,
        next_node_id,
        next_edge_id,
        warnings,
    })
}

pub(super) fn expand_truncates_for_fanout(
    changes: &[Change],
    expansions: &[(usize, Vec<Change>)],
) -> Option<Vec<Change>> {
    if expansions.is_empty() {
        return None;
    }
    let mut view = Vec::with_capacity(changes.len());
    for (index, change) in changes.iter().enumerate() {
        match change {
            Change::NodesOfTypeTruncated { .. }
            | Change::EdgesOfTypeTruncated { .. }
            | Change::GraphReset { .. } => {
                if let Some((_, expansion)) = expansions.iter().find(|(staged, _)| *staged == index)
                {
                    view.extend(expansion.iter().cloned());
                }
            }
            other => view.push(other.clone()),
        }
    }
    Some(view)
}
