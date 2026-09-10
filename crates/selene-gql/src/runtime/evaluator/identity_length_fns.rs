//! Identity and length scalar function evaluation.

use selene_core::{Path, Record, Value};

use crate::{
    SourceSpan,
    runtime::{DataExceptionSubclass, EvalCtx, ExecutorError},
};

use super::binary_ops::{data_exception_with, string_value};

pub(super) fn eval_element_id(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    match args.into_iter().next().expect("arity checked") {
        Value::Null => Ok(Value::Null),
        Value::NodeRef(id) => string_value(&id.to_string(), span),
        Value::EdgeRef(id) => string_value(&id.to_string(), span),
        Value::List(_) => data_exception_with(
            DataExceptionSubclass::InvalidValueType,
            "element_id argument is not a singleton element reference",
            span,
        ),
        _ => data_exception_with(
            DataExceptionSubclass::InvalidValueType,
            "element_id argument is not an element reference",
            span,
        ),
    }
}

pub(super) fn eval_cardinality(
    args: Vec<Value>,
    span: SourceSpan,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    match args.into_iter().next().expect("arity checked") {
        Value::Null => Ok(Value::Null),
        Value::List(values) => Ok(Value::Int(values.len() as i64)),
        Value::Path(path) => Ok(Value::Int((1 + 2 * path.segments.len()) as i64)),
        Value::Record(record) => {
            let len = match *record {
                Record::Open(fields) => fields.len(),
                _ => {
                    return data_exception_with(
                        DataExceptionSubclass::InvalidValueType,
                        "unsupported record shape",
                        span,
                    );
                }
            };
            Ok(Value::Int(len as i64))
        }
        Value::RecordTyped(record) => Ok(Value::Int(record.values.len() as i64)),
        Value::TableRef(id) => ctx
            .tx
            .binding_table_for(id)
            .map(|table| Value::Int(table.row_count() as i64))
            .map_err(|error| {
                super::binary_ops::data_exception_value_with(
                    DataExceptionSubclass::InvalidValueType,
                    format!("cardinality {error}"),
                    span,
                )
            }),
        _ => data_exception_with(
            DataExceptionSubclass::InvalidValueType,
            "cardinality argument is not a binding table, path, list, or record",
            span,
        ),
    }
}

pub(super) fn eval_path_length(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    match args.into_iter().next().expect("arity checked") {
        Value::Null => Ok(Value::Null),
        Value::Path(path) => Ok(Value::Int(path.segments.len() as i64)),
        _ => data_exception_with(
            DataExceptionSubclass::InvalidValueType,
            "path_length argument is not a path",
            span,
        ),
    }
}

pub(super) fn eval_elements(args: Vec<Value>, span: SourceSpan) -> Result<Value, ExecutorError> {
    match args.into_iter().next().expect("arity checked") {
        Value::Null => Ok(Value::Null),
        Value::Path(path) => Ok(Value::List(path_element_list(*path))),
        _ => data_exception_with(
            DataExceptionSubclass::InvalidValueType,
            "elements argument is not a path",
            span,
        ),
    }
}

pub(super) fn eval_labels(
    args: Vec<Value>,
    span: SourceSpan,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    let value = args.into_iter().next().expect("arity checked");
    super::require_live_referent(&value, span, ctx)?;
    match value {
        Value::Null => Ok(Value::Null),
        Value::NodeRef(id) => Ok(Value::List(
            ctx.tx
                .snapshot()
                .node_labels(id)
                .map(|labels| {
                    labels
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )),
        Value::EdgeRef(id) => Ok(Value::List(
            ctx.tx
                .snapshot()
                .edge_label(id)
                .map(|label| vec![Value::String(label.clone())])
                .unwrap_or_default(),
        )),
        _ => data_exception_with(
            DataExceptionSubclass::InvalidValueType,
            "labels argument is not a graph element",
            span,
        ),
    }
}

fn path_element_list(path: Path) -> Vec<Value> {
    let capacity = path
        .segments
        .len()
        .checked_mul(2)
        .and_then(|len| len.checked_add(1))
        .expect("path element list length fits in usize");
    let mut values = Vec::with_capacity(capacity);
    values.push(Value::NodeRef(path.start));
    for segment in path.segments {
        values.push(Value::EdgeRef(segment.edge));
        values.push(Value::NodeRef(segment.node));
    }
    values
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use selene_core::{BindingTableId, GraphId, RecordTypeId};
    use selene_graph::SeleneGraph;
    use smallvec::smallvec;

    use super::*;
    use crate::{
        EmptyProcedureRegistry, ImplDefinedCaps, SourceSpan, SubqueryRegistry,
        analyze::ExprIdLookup, runtime::TxContext,
    };

    fn eval(value: Value) -> Result<Value, ExecutorError> {
        let caps = ImplDefinedCaps::default();
        let graph = Arc::new(SeleneGraph::new(GraphId::new(9100)));
        let tx = TxContext::read_only(graph, &caps, &EmptyProcedureRegistry, &[]);
        let expr_ids = ExprIdLookup::default();
        let subqueries = SubqueryRegistry::default();
        let ctx = EvalCtx {
            tx: &tx,
            expr_ids: &expr_ids,
            subqueries: &subqueries,
        };
        eval_cardinality(vec![value], SourceSpan::default(), &ctx)
    }

    #[test]
    fn cardinality_counts_empty_open_record() {
        let value = Value::Record(Box::new(Record::Open(smallvec![])));

        assert_eq!(eval(value).expect("cardinality succeeds"), Value::Int(0));
    }

    #[test]
    fn cardinality_counts_typed_record_slots() {
        let value = Value::RecordTyped(Box::new(selene_core::RecordTyped {
            type_id: RecordTypeId::new(1),
            values: smallvec![Some(Value::Int(1)), None, Some(Value::Null)],
        }));

        assert_eq!(eval(value).expect("cardinality succeeds"), Value::Int(3));
    }

    #[test]
    fn cardinality_unknown_table_ref_is_invalid_value_type() {
        let err = eval(Value::TableRef(BindingTableId::new(999))).expect_err("unknown ID errors");

        assert_eq!(err.gqlstatus().as_str(), "22G03");
    }
}
