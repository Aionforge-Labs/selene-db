//! Audit mixed-edge incidence independently of the maintained adjacency maps.

use std::collections::BTreeSet;

use selene_core::{DbString, EdgeDirectionality, EdgeId, NodeId};
use selene_graph::SeleneGraph;

use super::CheckResult;

const OUT: usize = 0;
const IN: usize = 1;
const UNDIRECTED: usize = 2;

// Include the map kind and stable identity, not just the endpoint pair: parallel
// siblings cannot stand in for each other, nor can a wrong-kind incidence.
type Incidence = (usize, NodeId, EdgeId, NodeId, DbString);

pub(super) fn check_adjacency_symmetry(snapshot: &SeleneGraph) -> CheckResult {
    let mut issues = 0;
    let mut expected = BTreeSet::<Incidence>::new();
    let mut live_edges = 0;
    if let Ok(candidates) = snapshot.live_edge_candidates() {
        for id in candidates.iter() {
            live_edges += 1;
            let (Some((first, second)), Some(label), Some(kind)) = (
                snapshot.edge_endpoints(id),
                snapshot.edge_label(id),
                snapshot.edge_directionality(id),
            ) else {
                issues += 1;
                continue;
            };
            if !snapshot.is_node_alive(first) || !snapshot.is_node_alive(second) {
                issues += 1;
            }
            match kind {
                EdgeDirectionality::Directed => {
                    expected.insert((OUT, first, id, second, label.clone()));
                    expected.insert((IN, second, id, first, label.clone()));
                }
                EdgeDirectionality::Undirected => {
                    if first > second {
                        issues += 1;
                    }
                    expected.insert((UNDIRECTED, first, id, second, label.clone()));
                    // An undirected self-loop has one incidence, not two.
                    if first != second {
                        expected.insert((UNDIRECTED, second, id, first, label.clone()));
                    }
                }
            }
        }
    } else {
        issues += 1;
    }
    let mut expected_counts = [0_usize; 3];
    for (kind, ..) in &expected {
        expected_counts[*kind] += 1;
    }
    let mut observed_counts = [0_usize; 3];
    for (kind, map) in [
        &snapshot.adjacency_out,
        &snapshot.adjacency_in,
        &snapshot.adjacency_undirected,
    ]
    .into_iter()
    .enumerate()
    {
        for (node, entry) in map {
            if entry.is_empty() {
                issues += 1;
            }
            for edge in entry.iter() {
                observed_counts[kind] += 1;
                if !expected.remove(&(kind, *node, edge.edge_id, edge.neighbor, edge.label.clone()))
                {
                    // Includes duplicates: each expected incidence is consumed
                    // exactly once, even if a duplicate balances a missing sibling.
                    issues += 1;
                }
            }
        }
    }
    issues += expected.len();
    let [outgoing, incoming, undirected] = observed_counts;
    CheckResult::new(
        issues,
        format!(
            "live edges={live_edges}; outgoing adjacency edges={outgoing}; incoming adjacency edges={incoming}; undirected incidences={undirected}; expected out/in/undirected={expected_counts:?}; issues={issues}"
        ),
    )
}
