//! Predicate expression evaluation.
//!
//! Implements residual predicate forms that are not lowered into scan access:
//! `IS` sub-kinds, graph predicate functions, `ALL_DIFFERENT`, `SAME`, and
//! `PROPERTY_EXISTS`. Predicate negation preserves `NULL` as unknown.

use selene_core::{DbString, EdgeId, NodeId, Value};

use crate::{
    IsCheckKind, LabelExpr, SourceSpan, TruthValue, ValueExpr,
    runtime::{Binding, BindingTableSchema, DataExceptionSubclass, EvalCtx, ExecutorError},
};

use super::{
    binary_ops::{data_exception, data_exception_with, string_slice},
    evaluate, property_access, string_fns,
};
use crate::runtime::scan;

pub(super) fn eval_is_check(
    operand: &ValueExpr,
    kind: &IsCheckKind,
    negated: bool,
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    let operand = evaluate(operand, binding, schema, ctx)?;
    let result = match kind {
        IsCheckKind::Null => Value::Bool(matches!(operand, Value::Null)),
        IsCheckKind::Directed => eval_is_directed(operand, span, ctx)?,
        IsCheckKind::Labeled(label_expr) => eval_is_labeled(operand, label_expr, span, ctx)?,
        IsCheckKind::TruthValue(truth_value) => eval_is_truth_value(operand, *truth_value, span)?,
        IsCheckKind::Typed(ty) => Value::Bool(
            crate::runtime::value_type_match::value_matches_gql_type(&operand, ty),
        ),
        IsCheckKind::SourceOf(value) => {
            eval_is_endpoint(operand, value, true, span, binding, schema, ctx)?
        }
        IsCheckKind::DestinationOf(value) => {
            eval_is_endpoint(operand, value, false, span, binding, schema, ctx)?
        }
        IsCheckKind::Normalized(form) => eval_is_normalized(operand, *form, span)?,
    };
    negate_predicate(result, negated)
}

fn eval_is_normalized(
    operand: Value,
    form: crate::NormalForm,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    if matches!(operand, Value::Null) {
        return Ok(Value::Null);
    }
    let Some(value) = string_slice(&operand) else {
        return data_exception("IS NORMALIZED operand is not a string", span);
    };
    Ok(Value::Bool(string_fns::is_normalized(value, form)))
}

pub(super) fn eval_all_different(
    items: &[ValueExpr],
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    let references = evaluate_graph_references("ALL_DIFFERENT", items, span, binding, schema, ctx)?;
    for (left_index, lhs) in references.iter().enumerate() {
        for rhs in &references[left_index + 1..] {
            match (*lhs, *rhs) {
                (GraphReference::Node(left), GraphReference::Node(right)) if left == right => {
                    return Ok(Value::Bool(false));
                }
                (GraphReference::Edge(left), GraphReference::Edge(right)) if left == right => {
                    return Ok(Value::Bool(false));
                }
                (GraphReference::Node(_), GraphReference::Node(_))
                | (GraphReference::Edge(_), GraphReference::Edge(_)) => {}
                _ => return values_not_comparable("ALL_DIFFERENT", span),
            }
        }
    }
    Ok(Value::Bool(true))
}

pub(super) fn eval_same(
    items: &[ValueExpr],
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    let references = evaluate_graph_references("SAME", items, span, binding, schema, ctx)?;
    let Some(first) = references.first().copied() else {
        return Ok(Value::Bool(true));
    };
    for value in &references[1..] {
        match (first, *value) {
            (GraphReference::Node(left), GraphReference::Node(right)) if left == right => {}
            (GraphReference::Edge(left), GraphReference::Edge(right)) if left == right => {}
            (GraphReference::Node(_), GraphReference::Node(_))
            | (GraphReference::Edge(_), GraphReference::Edge(_)) => return Ok(Value::Bool(false)),
            _ => return values_not_comparable("SAME", span),
        }
    }
    Ok(Value::Bool(true))
}

#[derive(Clone, Copy)]
enum GraphReference {
    Node(NodeId),
    Edge(EdgeId),
}

fn evaluate_graph_references(
    predicate: &'static str,
    items: &[ValueExpr],
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<GraphReference>, ExecutorError> {
    evaluate_items(items, binding, schema, ctx)?
        .iter()
        .map(|value| graph_reference(predicate, value, span))
        .collect()
}

fn graph_reference(
    predicate: &'static str,
    value: &Value,
    span: SourceSpan,
) -> Result<GraphReference, ExecutorError> {
    match value {
        Value::NodeRef(id) => Ok(GraphReference::Node(*id)),
        Value::EdgeRef(id) => Ok(GraphReference::Edge(*id)),
        Value::Null => data_exception_with(
            DataExceptionSubclass::NullValueNotAllowed,
            format!("{predicate} operands cannot be NULL"),
            span,
        ),
        _ => data_exception(
            format!("{predicate} operands must be graph element references"),
            span,
        ),
    }
}

fn values_not_comparable<T>(predicate: &'static str, span: SourceSpan) -> Result<T, ExecutorError> {
    data_exception_with(
        DataExceptionSubclass::ValuesNotComparable,
        format!("{predicate} operands are not comparable graph element references"),
        span,
    )
}

pub(super) fn eval_property_exists(
    target: &ValueExpr,
    key: DbString,
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    let target = evaluate(target, binding, schema, ctx)?;
    if matches!(target, Value::Null) {
        return Ok(Value::Null);
    }
    if !matches!(target, Value::NodeRef(_) | Value::EdgeRef(_)) {
        return data_exception("PROPERTY_EXISTS target is not a node or edge", span);
    }
    let value = property_access(&target, key, span, ctx)?;
    Ok(Value::Bool(!matches!(value, Value::Null)))
}

fn evaluate_items(
    items: &[ValueExpr],
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Value>, ExecutorError> {
    items
        .iter()
        .map(|item| evaluate(item, binding, schema, ctx))
        .collect()
}

fn eval_is_directed(
    value: Value,
    span: SourceSpan,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    super::require_live_referent(&value, span, ctx)?;
    match value {
        Value::Null => Ok(Value::Null),
        Value::EdgeRef(id) => Ok(Value::Bool(
            ctx.tx.snapshot().edge_directionality(id)
                == Some(selene_core::EdgeDirectionality::Directed),
        )),
        Value::NodeRef(_) => data_exception("IS DIRECTED operand is not an edge", span),
        _ => data_exception("IS DIRECTED operand is not a graph element", span),
    }
}

fn eval_is_labeled(
    value: Value,
    label_expr: &LabelExpr,
    span: SourceSpan,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    super::require_live_referent(&value, span, ctx)?;
    match value {
        Value::Null => Ok(Value::Null),
        Value::NodeRef(id) => {
            Ok(Value::Bool(ctx.tx.snapshot().node_labels(id).is_some_and(
                |labels| scan::label_matches_node(label_expr, labels),
            )))
        }
        Value::EdgeRef(id) => {
            Ok(Value::Bool(ctx.tx.snapshot().edge_label(id).is_some_and(
                |label| scan::label_matches_edge(label_expr, label),
            )))
        }
        _ => data_exception("IS LABELED operand is not a graph element", span),
    }
}

fn eval_is_truth_value(
    value: Value,
    truth_value: TruthValue,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    // Per ISO/IEC 39075:2024 §19, `<value> IS [NOT] {TRUE|FALSE|UNKNOWN}` takes a
    // boolean-valued operand; NULL maps to UNKNOWN. A non-boolean operand
    // (e.g. `5 IS TRUE`) is a data exception (22G03), not a silent FALSE.
    let matches_truth = match (value, truth_value) {
        (Value::Bool(true), TruthValue::True)
        | (Value::Bool(false), TruthValue::False)
        | (Value::Null, TruthValue::Unknown) => true,
        (Value::Bool(_), _) | (Value::Null, _) => false,
        _ => return data_exception("IS <truth value> operand is not boolean", span),
    };
    Ok(Value::Bool(matches_truth))
}

fn eval_is_endpoint(
    operand: Value,
    value: &ValueExpr,
    source: bool,
    span: SourceSpan,
    binding: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    let value = evaluate(value, binding, schema, ctx)?;
    super::require_live_referent(&operand, span, ctx)?;
    super::require_live_referent(&value, span, ctx)?;
    if matches!(operand, Value::Null) || matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::NodeRef(node_id) = operand else {
        return data_exception("endpoint predicate operand is not a node", span);
    };
    let Value::EdgeRef(edge_id) = value else {
        return data_exception("endpoint predicate value is not an edge", span);
    };
    if ctx.tx.snapshot().edge_directionality(edge_id)
        != Some(selene_core::EdgeDirectionality::Directed)
    {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(
        ctx.tx
            .snapshot()
            .edge_endpoints(edge_id)
            .is_some_and(|(source_id, destination_id)| {
                if source {
                    source_id == node_id
                } else {
                    destination_id == node_id
                }
            }),
    ))
}

fn negate_predicate(value: Value, negated: bool) -> Result<Value, ExecutorError> {
    if !negated {
        return Ok(value);
    }
    Ok(match value {
        Value::Bool(value) => Value::Bool(!value),
        Value::Null => Value::Null,
        other => other,
    })
}
