//! First-party catalog attachment. No runtime is published or callback exposed
//! while primary values are being rebuilt. Failed preparation drops private state.

use std::sync::Arc;

use selene_catalog::{CatalogPayload, DeclarationState, NativeBinding};
use selene_core::db_string;

use crate::{
    CandidateStateSpec, GraphError, GraphResult, IndexProvider, MaintainedCandidateStateProvider,
    SeleneGraph,
};

pub(super) fn prepare(graph: &SeleneGraph) -> GraphResult<Option<Arc<dyn IndexProvider>>> {
    let mut specs = Vec::new();
    for descriptor in graph.catalog_declarations() {
        let CatalogPayload::Procedure(native) = descriptor.payload() else {
            continue;
        };
        let NativeBinding::CandidateState(state) = &native.binding else {
            continue;
        };
        if native.metadata.state != DeclarationState::Ready {
            continue;
        }
        let convert = |value: &str| {
            db_string(value).map_err(|_| GraphError::Inconsistent {
                reason: "invalid catalog candidate-state name or label".into(),
            })
        };
        let labels = |values: &[String]| {
            values
                .iter()
                .map(|v| convert(v))
                .collect::<GraphResult<Vec<_>>>()
        };
        specs.push(CandidateStateSpec {
            name: convert(descriptor.name().display())?,
            required_label: state.required_label.as_deref().map(convert).transpose()?,
            require_outgoing: labels(&state.require_outgoing)?,
            require_incoming: labels(&state.require_incoming)?,
            exclude_outgoing: labels(&state.exclude_outgoing)?,
            exclude_incoming: labels(&state.exclude_incoming)?,
        });
    }
    if specs.is_empty() {
        return Ok(None);
    }
    // Ownership is reserved by this private instance before rebuilding. Never
    // borrow a live provider from the prior publication: equal generations in
    // different graphs or detached transactions are not interchangeable.
    let provider = MaintainedCandidateStateProvider::new(specs)?;
    provider.rebuild_from_graph(graph)?;
    Ok(Some(Arc::new(provider)))
}
