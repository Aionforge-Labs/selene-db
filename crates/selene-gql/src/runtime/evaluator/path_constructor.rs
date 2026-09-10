//! Evaluation for ISO `PATH[...]` value construction.

use selene_core::{EdgeDirection, EdgeDirectionality, Path, PathSegment, Value};
use smallvec::SmallVec;

use crate::{
    SourceSpan, ValueExpr,
    runtime::{Binding, BindingTableSchema, DataExceptionSubclass, EvalCtx, ExecutorError},
};

use super::evaluate;

pub(super) fn eval_path_constructor(
    elements: &[ValueExpr],
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    let values = elements
        .iter()
        .map(|element| evaluate(element, binding, schema, ctx))
        .collect::<Result<Vec<_>, _>>()?;
    construct_path(values, span, ctx)
}

fn construct_path(
    values: Vec<Value>,
    span: SourceSpan,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    for value in &values {
        super::require_live_referent(value, span, ctx)?;
    }
    if values.is_empty() || values.len().is_multiple_of(2) {
        return malformed_path(
            "PATH constructor requires node, edge, node, ... elements",
            span,
        );
    }
    let Some(Value::NodeRef(start)) = values.first().cloned() else {
        return malformed_path(
            "PATH constructor must start with a live node reference",
            span,
        );
    };
    if ctx.tx.snapshot().node_labels(start).is_none() {
        return malformed_path("PATH constructor start node is not live", span);
    }
    let mut current = start;
    let mut segments = SmallVec::<[PathSegment; 4]>::new();
    for pair in values[1..].chunks_exact(2) {
        let (Value::EdgeRef(edge), Value::NodeRef(node)) = (&pair[0], &pair[1]) else {
            return malformed_path("PATH constructor elements must alternate edge, node", span);
        };
        if ctx.tx.snapshot().node_labels(*node).is_none() {
            return malformed_path("PATH constructor target node is not live", span);
        }
        let Some((source, target)) = ctx.tx.snapshot().edge_endpoints(*edge) else {
            return malformed_path("PATH constructor edge is not live", span);
        };
        let direction = if ctx.tx.snapshot().edge_directionality(*edge)
            == Some(EdgeDirectionality::Undirected)
            && ((source == current && target == *node) || (target == current && source == *node))
        {
            EdgeDirection::Undirected
        } else if source == current && target == *node {
            EdgeDirection::Outgoing
        } else if target == current && source == *node {
            EdgeDirection::Incoming
        } else {
            return malformed_path("PATH constructor elements do not identify a path", span);
        };
        segments.push(PathSegment {
            edge: *edge,
            direction,
            node: *node,
        });
        current = *node;
    }
    Ok(Value::Path(Box::new(Path {
        graph: ctx.tx.snapshot().graph_id(),
        start,
        segments,
    })))
}

fn malformed_path<T>(message: &'static str, span: SourceSpan) -> Result<T, ExecutorError> {
    Err(ExecutorError::data_exception(
        DataExceptionSubclass::MalformedPath,
        message,
        span,
    ))
}
