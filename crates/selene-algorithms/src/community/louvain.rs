//! Louvain modularity optimization for community detection (single-pass).
//!
//! Each node initially in its own community. Each iteration: for each node u,
//! evaluate modularity gain `Δ` from joining each neighbor community; move u
//! to the candidate with **strictly-largest** gain. Repeat until no node
//! moves, or caller-supplied `max_iter` reached.
//!
//! ## Current scope
//!
//! - Single-pass at `level = 0` (no hierarchical contraction; that ships in
//!   a future multi-level Louvain — the `u32 level` is reserved for forward
//!   compatibility per spec 16 §E27).
//! - Uses projection edge weights (§E04). Defaults to unit weights when the
//!   projection is unweighted.
//! - State arrays sized by live count via `RowIndex` (§E26).
//!
//! ## Determinism (spec 16 §E30)
//!
//! Candidate communities are iterated in **sorted order by community ID** to
//! defuse `HashMap` iteration-order non-determinism. The movement rule is
//! strict `delta > best_delta` (initial `best_delta = 0.0`), so equal-delta
//! candidates do NOT replace — but the sorted iteration makes the algorithm
//! robust to a future `>=` change.

// Integer-keyed hot-path maps use FxHashMap to avoid SipHash overhead.
use rustc_hash::FxHashMap as HashMap;
use selene_core::{CancellationChecker, NodeId};
use smallvec::SmallVec;

use crate::error::{AlgorithmAborted, check_algorithm, check_algorithm_stride};
use crate::projection::GraphProjection;

const INLINE_COMMUNITY_WEIGHT_CAP: usize = 16;

/// Compute community assignments via single-pass Louvain modularity.
///
/// Returns `(NodeId, community_id, level)` triples sorted **ASC by NodeId**
/// per spec 16 §E27. `level` is always `0` in the current single-pass
/// implementation. Empty projection → `vec![]`. `max_iter == 0` returns the
/// initial state (each node in its own community).
///
/// Edge weights are projected via `ProjNeighbor::weight` per spec 16 §E04
/// (unit weights when the projection is unweighted).
#[must_use]
pub fn louvain(proj: &GraphProjection, max_iter: usize) -> Vec<(NodeId, u64, u32)> {
    louvain_with_checker(proj, max_iter, CancellationChecker::disabled())
        .expect("disabled cancellation checker never aborts")
}

/// Compute Louvain communities with cooperative cancellation checkpoints.
pub fn louvain_with_checker(
    proj: &GraphProjection,
    max_iter: usize,
    checker: CancellationChecker<'_>,
) -> Result<Vec<(NodeId, u64, u32)>, AlgorithmAborted> {
    check_algorithm(checker)?;
    let idx = proj.row_index();
    if idx.is_empty() {
        return Ok(Vec::new());
    }
    let n = idx.len();

    // Half the logical-incidence weight sum maintains Σ d_i = 2·total_weight.
    // A self-loop has one incidence, including in this normalization.
    let mut total_weight: f64 = 0.0;
    let mut rows_since_check = 0usize;
    for d in 0..n as u32 {
        check_algorithm_stride(checker, &mut rows_since_check)?;
        for nb in proj.incident_neighbors_dense(d) {
            total_weight += nb.weight / 2.0;
        }
    }
    if total_weight == 0.0 {
        // Avoid div-by-zero in the gain formula. No edges → no movement
        // possible anyway; the value is observationally irrelevant.
        total_weight = 1.0;
    }
    // community[d] = current community ID (encoded as a dense index initially).
    let mut community: Vec<u32> = (0..n as u32).collect();

    // Distinct parallel edges retain their weights. A reciprocal CSR view of
    // the same undirected identity does not double its contribution.
    let mut weighted_degree: Vec<f64> = vec![0.0; n];
    rows_since_check = 0;
    for d in 0..n as u32 {
        check_algorithm_stride(checker, &mut rows_since_check)?;
        let mut deg = 0.0;
        for nb in proj.incident_neighbors_dense(d) {
            deg += nb.weight;
        }
        weighted_degree[d as usize] = deg;
    }

    // comm_degree_sum[community_id] = Σ weighted_degree[v] for v ∈ community.
    // Community IDs are the dense seed rows from 0..n and remain in that range
    // for this single-level implementation, so a vector avoids hot-loop hash
    // lookups while still allowing empty community slots after moves.
    let mut comm_degree_sum = vec![0.0; n];
    for d in 0..n as u32 {
        let c = community[d as usize];
        comm_degree_sum[c as usize] += weighted_degree[d as usize];
    }

    // Reused per-node scratch.
    let mut comm_weights = CommunityWeightScratch::default();
    // Sorted-candidate buffer for §E30 determinism. Built fresh per node.
    let mut sorted_candidates: Vec<(u32, f64)> = Vec::new();

    let mut improved = true;
    let mut iter = 0usize;
    while improved && iter < max_iter {
        check_algorithm(checker)?;
        improved = false;
        iter += 1;

        rows_since_check = 0;
        for d in 0..n as u32 {
            check_algorithm_stride(checker, &mut rows_since_check)?;
            let idx_d = d as usize;
            let current_comm = community[idx_d];
            let ki = weighted_degree[idx_d];

            comm_weights.clear();
            for nb in proj.incident_neighbors_dense(d) {
                let nb_comm = community[nb.dense as usize];
                comm_weights.add(nb_comm, nb.weight);
            }

            let ki_in_current = comm_weights.get(current_comm).unwrap_or(0.0);
            let sigma_current = comm_degree_sum[current_comm as usize] - ki;

            // §E30 determinism: iterate candidate communities in sorted order
            // by community ID, not raw HashMap order.
            comm_weights.sorted_candidates(&mut sorted_candidates);

            let mut best_comm = current_comm;
            let mut best_delta = 0.0f64;

            for &(candidate_comm, ki_in_candidate) in &sorted_candidates {
                if candidate_comm == current_comm {
                    continue;
                }
                let sigma_candidate = comm_degree_sum[candidate_comm as usize];

                let delta =
                    compute_modularity_delta(total_weight, ki_in_candidate, ki, sigma_candidate)
                        - compute_modularity_delta(total_weight, ki_in_current, ki, sigma_current);

                if delta > best_delta {
                    best_delta = delta;
                    best_comm = candidate_comm;
                }
            }

            if best_comm != current_comm {
                comm_degree_sum[current_comm as usize] -= ki;
                comm_degree_sum[best_comm as usize] += ki;
                community[idx_d] = best_comm;
                improved = true;
            }
        }
    }

    // Final result: `community[d]` holds the dense index of the community's
    // seed node (the singleton each merge chain folded into). We emit that
    // seed's external NodeId — `idx.node_id_of(community[d]).get()` — as the
    // community label. This is a stable identifier within a single run, NOT
    // the smallest NodeId in the community: it deliberately differs from the
    // min-NodeId canonicalization used by WCC / SCC / label_propagation
    // (§E12), because Louvain communities are not connectivity components.
    // Smallest-NodeId-in-community canonicalization, if a future brief wants
    // it, is a separate post-pass (no information loss).
    let mut result: Vec<(NodeId, u64, u32)> = (0..n as u32)
        .map(|d| {
            (
                idx.node_id_of(d),
                idx.node_id_of(community[d as usize]).get(),
                0u32,
            )
        })
        .collect();
    result.sort_by_key(|&(nid, _, _)| nid.get());
    Ok(result)
}

struct CommunityWeightScratch {
    inline: SmallVec<[(u32, f64); INLINE_COMMUNITY_WEIGHT_CAP]>,
    spill: HashMap<u32, f64>,
    spilled: bool,
}

impl Default for CommunityWeightScratch {
    fn default() -> Self {
        Self {
            inline: SmallVec::new(),
            spill: HashMap::default(),
            spilled: false,
        }
    }
}

impl CommunityWeightScratch {
    fn clear(&mut self) {
        self.inline.clear();
        if self.spilled {
            self.spill.clear();
            self.spilled = false;
        }
    }

    fn add(&mut self, community: u32, weight: f64) {
        if self.spilled {
            *self.spill.entry(community).or_insert(0.0) += weight;
            return;
        }

        if let Some((_, total)) = self
            .inline
            .iter_mut()
            .find(|(candidate, _)| *candidate == community)
        {
            *total += weight;
            return;
        }

        if self.inline.len() < INLINE_COMMUNITY_WEIGHT_CAP {
            self.inline.push((community, weight));
            return;
        }

        self.spill.clear();
        self.spill.extend(self.inline.drain(..));
        self.spilled = true;
        *self.spill.entry(community).or_insert(0.0) += weight;
    }

    fn get(&self, community: u32) -> Option<f64> {
        if self.spilled {
            self.spill.get(&community).copied()
        } else {
            self.inline
                .iter()
                .find_map(|(candidate, weight)| (*candidate == community).then_some(*weight))
        }
    }

    fn sorted_candidates(&self, out: &mut Vec<(u32, f64)>) {
        out.clear();
        if self.spilled {
            out.extend(
                self.spill
                    .iter()
                    .map(|(&community, &weight)| (community, weight)),
            );
        } else {
            out.extend(self.inline.iter().copied());
        }
        out.sort_by_key(|&(community, _)| community);
    }
}

/// Compute the scalar Louvain gain term for moving a node into one community.
///
/// This pins the formula used by the movement loop:
/// `k_i,in / m - k_i * sigma_tot / (2m^2)`, where `m` is the projection's
/// total directed edge weight, `k_i,in` is the node's incident weight into the
/// candidate community, `k_i` is the node's weighted degree, and `sigma_tot`
/// is the target community's weighted-degree sum. The movement delta is the
/// candidate term minus the current-community term.
fn compute_modularity_delta(total_weight: f64, ki_in: f64, ki: f64, sigma_tot: f64) -> f64 {
    ki_in / total_weight - (ki * sigma_tot) / (2.0 * total_weight * total_weight)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_modularity_delta_known_answer() {
        let delta = compute_modularity_delta(5.0, 1.0, 2.0, 4.0);

        assert!(
            (delta - 0.04).abs() <= 1.0e-12,
            "expected 0.04, observed {delta}"
        );
    }

    #[test]
    fn community_weight_scratch_spills_and_sorts_candidates() {
        let mut weights = CommunityWeightScratch::default();

        for community in (0..INLINE_COMMUNITY_WEIGHT_CAP as u32 + 2).rev() {
            weights.add(community, 1.0);
        }
        weights.add(3, 2.5);

        let mut candidates = Vec::new();
        weights.sorted_candidates(&mut candidates);

        assert_eq!(weights.get(3), Some(3.5));
        assert_eq!(candidates.len(), INLINE_COMMUNITY_WEIGHT_CAP + 2);
        assert!(candidates.windows(2).all(|pair| pair[0].0 < pair[1].0));

        weights.clear();
        weights.add(2, 1.25);
        weights.add(2, 0.75);
        weights.sorted_candidates(&mut candidates);

        assert_eq!(weights.get(2), Some(2.0));
        assert_eq!(candidates, vec![(2, 2.0)]);
    }
}
