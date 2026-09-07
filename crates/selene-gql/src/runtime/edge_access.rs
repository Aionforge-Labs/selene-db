//! Edge-index candidate helpers shared by expand executors.

use selene_core::EdgeId;
use selene_graph::{CandidateSet, Edge};

use crate::{EdgeMatch, NodeOrEdgeScan, ScanAccess, ScanKind};

use super::{EvalCtx, ExecutorError, scan};

pub(super) fn candidate_edge_filter(
    edge: &EdgeMatch,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Option<CandidateSet<Edge>>, ExecutorError> {
    match &edge.access {
        ScanAccess::Linear | ScanAccess::LabelIndex { .. } => Ok(None),
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
