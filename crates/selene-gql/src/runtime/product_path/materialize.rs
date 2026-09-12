//! The native Path carrier is authoritative, not an endpoint/list adapter.

use super::state::SearchState;
use selene_core::{GraphId, PathSegment, Value};

pub(super) fn path_value(state: &SearchState, graph: GraphId) -> Value {
    debug_assert_eq!(state.nodes.len(), state.edges.len() + 1);
    debug_assert_eq!(state.directions.len(), state.edges.len());
    super::value::finish(
        graph,
        state.nodes[0],
        state
            .edges
            .iter()
            .zip(&state.directions)
            .zip(&state.nodes[1..])
            .map(|((&edge, &direction), &node)| PathSegment {
                edge,
                direction,
                node,
            })
            .collect(),
    )
}

pub(super) fn direction(
    graph: &selene_graph::SeleneGraph,
    edge: selene_core::EdgeId,
    from: selene_core::NodeId,
    declared: crate::EdgeDirection,
) -> selene_core::EdgeDirection {
    use selene_core::{EdgeDirection as D, EdgeDirectionality};
    if graph.edge_directionality(edge) == Some(EdgeDirectionality::Undirected) {
        D::Undirected
    } else if declared.includes_right()
        && graph.edge_endpoints(edge).is_some_and(|(s, _)| s == from)
    {
        D::Outgoing
    } else {
        D::Incoming
    }
}
