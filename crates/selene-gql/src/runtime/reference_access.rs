//! Liveness checks for native procedures which access graph referents.

use crate::ProcedureError;
use selene_core::NodeId;
use selene_graph::SeleneGraph;

pub(crate) fn require_live_nodes<'a>(
    graph: &SeleneGraph,
    nodes: impl Iterator<Item = &'a NodeId>,
) -> Result<(), ProcedureError> {
    for node in nodes {
        if !graph.is_node_alive(*node) {
            return Err(ProcedureError::InvalidReferenceValue);
        }
    }
    Ok(())
}
