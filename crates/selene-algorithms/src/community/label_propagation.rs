//! Raghavan-style label propagation for community detection.
//!
//! Each node initially carries its own NodeId as label. Each iteration visits
//! nodes in **ASC NodeId order** and updates each in turn — an
//! **asynchronous-deterministic** schedule (donor pattern). Each node adopts
//! the label most common among its undirected neighbors (out ∪ in); the
//! per-iteration update is immediately visible to later nodes within the same
//! iteration. Tie-break is smallest label ID (spec 16 §E30). Converges when
//! no labels change in an iteration, or `max_iter` reached.
//!
//! State arrays sized by live-node count via `RowIndex` (§E26). Only unit
//! weights are implemented; weighted variants are not supported.

use selene_core::{CancellationChecker, NodeId};

use crate::error::{AlgorithmAborted, check_algorithm, check_algorithm_stride};
use crate::projection::GraphProjection;

/// Compute community assignments via asynchronous-deterministic label
/// propagation.
///
/// Returns `(NodeId, community_id)` pairs sorted **ASC by NodeId** per spec 16
/// §E27. Empty projection → `vec![]`. `max_iter == 0` returns the initial
/// state (each node in its own community).
///
/// The community_id surfaces as the smallest **NodeId** that survived the
/// propagation (labels are NodeId values throughout). Isolated nodes retain
/// their initial label (donor / Raghavan: empty-neighbor → skip).
#[must_use]
pub fn label_propagation(proj: &GraphProjection, max_iter: usize) -> Vec<(NodeId, u64)> {
    label_propagation_with_checker(proj, max_iter, CancellationChecker::disabled())
        .expect("disabled cancellation checker never aborts")
}

/// Compute label propagation with cooperative cancellation checkpoints.
pub fn label_propagation_with_checker(
    proj: &GraphProjection,
    max_iter: usize,
    checker: CancellationChecker<'_>,
) -> Result<Vec<(NodeId, u64)>, AlgorithmAborted> {
    check_algorithm(checker)?;
    let idx = proj.row_index();
    if idx.is_empty() {
        return Ok(Vec::new());
    }
    let n = idx.len();

    // labels[d] = current label of dense node d, encoded as the dense row of
    // the node that supplied the label. `RowIndex` dense order is ASC NodeId,
    // so comparing dense labels preserves the §E30 smallest-NodeId tie-break.
    let mut labels: Vec<u32> = (0..n as u32).collect();

    // Reused per-node scratch; cleared at the top of each node iteration to
    // avoid reallocation in the hot loop.
    let mut label_counts = vec![0usize; n];
    let mut touched_labels: Vec<u32> = Vec::new();

    for _ in 0..max_iter {
        check_algorithm(checker)?;
        let mut changed = false;
        let mut rows_since_check = 0usize;

        for d in 0..n as u32 {
            check_algorithm_stride(checker, &mut rows_since_check)?;
            touched_labels.clear();

            // Count logical incidence once; distinct parallel IDs still vote.
            for nb in proj.incident_neighbors_dense(d) {
                bump_label_count(
                    labels[nb.dense as usize],
                    &mut label_counts,
                    &mut touched_labels,
                );
            }

            if touched_labels.is_empty() {
                continue;
            }

            // Pick the most common label; on ties, smallest label ID wins
            // (§E30). Dense labels are assigned ASC by NodeId, so `label <`
            // means the same tie-break as comparing the external label NodeId.
            let best_label = select_best_label(&touched_labels, &label_counts);

            if labels[d as usize] != best_label {
                labels[d as usize] = best_label;
                changed = true;
            }

            for label in touched_labels.iter().copied() {
                label_counts[label as usize] = 0;
            }
        }

        if !changed {
            break;
        }
    }

    let mut result: Vec<(NodeId, u64)> = (0..n as u32)
        .map(|d| (idx.node_id_of(d), idx.node_id_of(labels[d as usize]).get()))
        .collect();
    result.sort_by_key(|&(nid, _)| nid.get());
    Ok(result)
}

fn bump_label_count(label: u32, label_counts: &mut [usize], touched_labels: &mut Vec<u32>) {
    let count = &mut label_counts[label as usize];
    if *count == 0 {
        touched_labels.push(label);
    }
    *count += 1;
}

fn select_best_label(touched_labels: &[u32], label_counts: &[usize]) -> u32 {
    let mut best_label = touched_labels[0];
    let mut best_count = label_counts[best_label as usize];
    for label in touched_labels.iter().copied().skip(1) {
        let count = label_counts[label as usize];
        if count > best_count || (count == best_count && label < best_label) {
            best_label = label;
            best_count = count;
        }
    }
    best_label
}
