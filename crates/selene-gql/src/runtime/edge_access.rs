//! Edge-index candidate helpers shared by expand executors.

use selene_core::{EdgeDirectionality, EdgeId, NodeId};
use selene_graph::{AdjacencyEdge, CandidateSet, Edge, SeleneGraph};

use crate::{EdgeDirection, EdgeMatch, NodeOrEdgeScan, ScanAccess, ScanKind};

use super::{EvalCtx, ExecutorError, scan};

/// Traverse the selected incidence lists without scanning unrelated edges or
/// allocating a dedup set. Only a directed loop can occur in both directed
/// lists; suppress its incoming copy when the outgoing list is selected.
pub(super) fn adjacent_edges(
    graph: &SeleneGraph,
    node: NodeId,
    direction: EdgeDirection,
) -> impl Iterator<Item = &AdjacencyEdge> {
    let outgoing = direction
        .includes_right()
        .then(|| graph.outgoing_edges(node))
        .flatten();
    let incoming = direction
        .includes_left()
        .then(|| graph.incoming_edges(node))
        .flatten();
    let undirected = direction
        .includes_undirected()
        .then(|| graph.undirected_edges(node))
        .flatten();
    outgoing
        .into_iter()
        .flat_map(|e| e.iter())
        .chain(
            incoming
                .into_iter()
                .flat_map(|e| e.iter())
                .filter(move |e| !direction.includes_right() || e.neighbor != node),
        )
        .chain(undirected.into_iter().flat_map(|e| e.iter()))
}

/// Stable-ID counterpart for indexed expansion and path reconstruction.
pub(super) fn next_node(
    graph: &SeleneGraph,
    edge: EdgeId,
    current: NodeId,
    direction: EdgeDirection,
) -> Option<NodeId> {
    let (first, second) = graph.edge_endpoints(edge)?;
    match graph.edge_directionality(edge)? {
        EdgeDirectionality::Directed => {
            if direction.includes_right() && current == first {
                Some(second)
            } else if direction.includes_left() && current == second {
                Some(first)
            } else {
                None
            }
        }
        EdgeDirectionality::Undirected if direction.includes_undirected() => {
            if current == first {
                Some(second)
            } else if current == second {
                Some(first)
            } else {
                None
            }
        }
        EdgeDirectionality::Undirected => None,
    }
}

pub(super) fn candidate_edge_filter(
    edge: &EdgeMatch,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Option<CandidateSet<Edge>>, ExecutorError> {
    match &edge.access {
        ScanAccess::Linear
        | ScanAccess::LabelIndex { .. }
        | ScanAccess::ExpressionLookup { .. } => Ok(None),
        ScanAccess::TypedIndexRange { .. }
        | ScanAccess::BitmapUnion { .. }
        | ScanAccess::CompositeLookup { .. } => {
            let scan = NodeOrEdgeScan {
                binding: edge.binding,
                hidden_binding: edge.hidden_binding,
                kind: ScanKind::Edge,
                label_predicate: edge.label_predicate.clone(),
                property_predicates: edge.property_predicates.clone(),
                access: edge.access.clone(),
                span: edge.span,
            };
            Ok(Some(scan::candidate_edge_set(&scan, ctx)?))
        }
    }
}

pub(super) fn edge_filter_matches(filter: Option<&CandidateSet<Edge>>, edge_id: EdgeId) -> bool {
    let Some(candidates) = filter else {
        return true;
    };
    candidates.contains(edge_id)
}
