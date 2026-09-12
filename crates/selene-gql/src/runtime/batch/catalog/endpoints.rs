//! Edge-endpoint helpers for catalog DDL.

use selene_core::{DbString, LabelSet};
use selene_graph::{EdgeEndpointDef, GraphTypeDef};

use crate::{EdgeEndpointSpec, ExecutorError, SourceSpan};

pub(super) fn resolve_endpoints(
    spec: &EdgeEndpointSpec,
    graph_type: &GraphTypeDef,
    span: SourceSpan,
) -> Result<(EdgeEndpointDef, EdgeEndpointDef), ExecutorError> {
    Ok((
        single_endpoint(&spec.from_labels, graph_type, span)?,
        single_endpoint(&spec.to_labels, graph_type, span)?,
    ))
}

fn single_endpoint(
    labels: &[DbString],
    graph_type: &GraphTypeDef,
    span: SourceSpan,
) -> Result<EdgeEndpointDef, ExecutorError> {
    // Cascade (§A.3 post-fold, 4 rows):
    //   row 1/2: exact `find_node_type_index(&label_set)` matches a single
    //            node type whose `key_labels` equals the input. Handles both
    //            single-label inputs and multi-label-for-single-type inputs.
    //   row 3:   on miss with len >= 2 inputs, try per-label exact-single-
    //            label resolution. Every label must resolve to a distinct
    //            single-label node type; the resulting indices flow through
    //            `EdgeEndpointDef::one_of` for sort + dedupe + singleton-
    //            collapse.
    //   row 4:   on any per-label miss, emit the existing GraphTypeViolation.
    // Single-label inputs never produce OneOf (per the cascade).
    let label_set = labels.iter().cloned().collect::<LabelSet>();
    if let Some(index) = graph_type.find_node_type_index(&label_set) {
        return Ok(EdgeEndpointDef::NodeType(index));
    }
    if labels.len() >= 2 {
        let mut resolved: Vec<u32> = Vec::with_capacity(labels.len());
        for label in labels {
            let single = LabelSet::single(label.clone());
            let Some(index) = graph_type.find_node_type_index(&single) else {
                return Err(unknown_endpoint_error(&label_set, span));
            };
            resolved.push(index);
        }
        return Ok(EdgeEndpointDef::one_of(resolved));
    }
    Err(unknown_endpoint_error(&label_set, span))
}

fn unknown_endpoint_error(label_set: &LabelSet, span: SourceSpan) -> ExecutorError {
    ExecutorError::GraphTypeViolation {
        message: format!(
            "edge endpoint references unknown node type label set {}",
            render_endpoint_labels(label_set)
        ),
        span,
    }
}

fn render_endpoint_labels(labels: &LabelSet) -> String {
    labels
        .iter()
        .map(|label| format!(":{label}"))
        .collect::<Vec<_>>()
        .join(",")
}
