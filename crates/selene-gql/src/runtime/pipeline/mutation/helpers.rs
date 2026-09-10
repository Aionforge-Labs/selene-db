//! Leaf helpers for the mutation pipeline operator: expression-to-value
//! materialization, label/endpoint resolution, schema extension, change-set
//! diffs, and error mapping. Split from the parent operator file to keep both
//! under the repository 700-LOC cap; reached only through `super::mutation`.

use smallvec::SmallVec;

use super::*;

#[derive(Clone, Copy)]
pub(super) enum PropertyMapTarget {
    Node,
    Edge,
}

pub(super) fn property_map(
    property_inits: &[PropertyInit],
    target: PropertyMapTarget,
    row: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<PropertyMap, ExecutorError> {
    let mut pairs = Vec::with_capacity(property_inits.len());
    let mut rows_since_check = 0;
    for init in property_inits {
        ctx.tx.check_cancellation_stride(&mut rows_since_check, 1)?;
        pairs.push((
            init.key.clone(),
            evaluator::evaluate(&init.value.expr, row, schema, ctx)?,
        ));
    }
    PropertyMap::from_pairs(pairs).map_err(|source| {
        let subclass = match (&source, target) {
            (selene_core::CoreError::StoredValue(_), _) => DataExceptionSubclass::InvalidValueType,
            (_, PropertyMapTarget::Node) => {
                DataExceptionSubclass::NodePropertiesExceedSupportedMaximum
            }
            (_, PropertyMapTarget::Edge) => {
                DataExceptionSubclass::EdgePropertiesExceedSupportedMaximum
            }
        };
        ExecutorError::data_exception(
            subclass,
            format!("property map construction failed: {source}"),
            property_inits
                .first()
                .map(|init| init.span)
                .unwrap_or_default(),
        )
    })
}

pub(super) fn node_labels(
    label_expr: Option<&LabelExpr>,
    span: SourceSpan,
) -> Result<LabelSet, ExecutorError> {
    match label_expr {
        None => Ok(LabelSet::new()),
        Some(LabelExpr::Single(label)) => Ok(LabelSet::single(label.clone())),
        Some(LabelExpr::Conjunction(parts)) => {
            let mut labels = LabelSet::new();
            for part in parts {
                let LabelExpr::Single(label) = part else {
                    return Err(unsupported_node_label_expr(span));
                };
                labels.insert(label.clone());
            }
            Ok(labels)
        }
        // Other ISO-legal label-expression forms are not yet implemented in the
        // mutation surface; 42N01, not an internal-invariant break.
        Some(_) => Err(unsupported_node_label_expr(span)),
    }
}

pub(super) fn unsupported_node_label_expr(span: SourceSpan) -> ExecutorError {
    ExecutorError::FeatureNotSupportedYet {
        feature: "INSERT label expression form",
        span,
    }
}

pub(super) fn edge_label(
    label_expr: Option<&LabelExpr>,
    span: SourceSpan,
) -> Result<DbString, ExecutorError> {
    match label_expr {
        Some(LabelExpr::Single(label)) => Ok(label.clone()),
        None => Err(ExecutorError::data_exception(
            DataExceptionSubclass::EdgeLabelsBelowSupportedMinimum,
            "INSERT edge requires one label under IL001",
            span,
        )),
        Some(LabelExpr::Conjunction(_)) => Err(ExecutorError::data_exception(
            DataExceptionSubclass::EdgeLabelsExceedSupportedMaximum,
            "INSERT edge supports exactly one label under IL001",
            span,
        )),
        // ISO-legal edge label-expression forms are not yet implemented; 42N01.
        Some(_) => Err(ExecutorError::FeatureNotSupportedYet {
            feature: "INSERT edge label expression form",
            span,
        }),
    }
}

pub(super) fn endpoint_node(
    row: &Binding,
    endpoint: InsertEndpointRef,
    span: SourceSpan,
) -> Result<NodeId, ExecutorError> {
    match endpoint {
        InsertEndpointRef::Binding { column_index, .. } => target_node(row, column_index, span)?
            .ok_or(ExecutorError::ImplementationDefined {
                detail: "insert edge endpoint is null",
            }),
        InsertEndpointRef::InsertedNode(site_id) => {
            row.inserted_node(site_id)
                .ok_or(ExecutorError::ImplementationDefined {
                    detail: "insert edge endpoint site missing",
                })
        }
    }
}

pub(super) fn edge_endpoints(
    left: NodeId,
    right: NodeId,
    direction: EdgeDirection,
    span: SourceSpan,
) -> Result<(NodeId, NodeId), ExecutorError> {
    match direction {
        EdgeDirection::Right | EdgeDirection::Undirected => Ok((left, right)),
        EdgeDirection::Left => Ok((right, left)),
        EdgeDirection::Any
        | EdgeDirection::LeftOrRight
        | EdgeDirection::LeftOrUndirected
        | EdgeDirection::UndirectedOrRight => Err(ExecutorError::FeatureNotSupportedYet {
            feature: "INSERT edge orientation union",
            span,
        }),
    }
}

pub(super) fn target_node(
    row: &Binding,
    column_index: u32,
    span: SourceSpan,
) -> Result<Option<NodeId>, ExecutorError> {
    match target_value(row, column_index, span)? {
        Value::NodeRef(id) => Ok(Some(id)),
        Value::Null => Ok(None),
        _ => Err(ExecutorError::ImplementationDefined {
            detail: "mutation target is not a node",
        }),
    }
}

pub(super) fn target_edge(
    row: &Binding,
    column_index: u32,
    span: SourceSpan,
) -> Result<Option<EdgeId>, ExecutorError> {
    match target_value(row, column_index, span)? {
        Value::EdgeRef(id) => Ok(Some(id)),
        Value::Null => Ok(None),
        _ => Err(ExecutorError::ImplementationDefined {
            detail: "mutation target is not an edge",
        }),
    }
}

pub(super) fn target_value(
    row: &Binding,
    column_index: u32,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    row.get(column_index as usize)
        .cloned()
        .ok_or_else(|| ExecutorError::InvalidReference {
            name: format!("binding column {column_index}"),
            span,
        })
}

pub(super) fn extend_schema(
    schema: &mut BindingTableSchema,
    output_column_index: Option<u32>,
    output_column: Option<&BindingTableColumn>,
) -> Result<(), ExecutorError> {
    let (Some(index), Some(column)) = (output_column_index, output_column) else {
        return Ok(());
    };
    let index = index as usize;
    if index != schema.columns.len() {
        return Err(ExecutorError::ImplementationDefined {
            detail: "mutation output column index is not append position",
        });
    }
    schema.columns.push(column.clone());
    Ok(())
}

pub(super) fn set_output_value(values: &mut SmallVec<[Value; 8]>, column_index: u32, value: Value) {
    let column_index = column_index as usize;
    values.resize(column_index + 1, Value::Null);
    values[column_index] = value;
}

pub(super) fn record_insert_site(
    sites: &mut SmallVec<[(crate::InsertSiteId, NodeId); 4]>,
    site_id: crate::InsertSiteId,
    node_id: NodeId,
) {
    if let Some((_, existing)) = sites.iter_mut().find(|(site, _)| *site == site_id) {
        *existing = node_id;
    } else {
        sites.push((site_id, node_id));
    }
}

pub(super) fn label_diff(
    added: impl IntoIterator<Item = DbString>,
    removed: impl IntoIterator<Item = DbString>,
    span: SourceSpan,
) -> Result<LabelDiff, ExecutorError> {
    LabelDiff::new(added, removed).map_err(|source| {
        ExecutorError::data_exception(
            DataExceptionSubclass::DataException,
            format!("label diff construction failed: {source}"),
            span,
        )
    })
}

pub(super) fn property_diff(
    set: impl IntoIterator<Item = (DbString, Value)>,
    removed: impl IntoIterator<Item = DbString>,
    span: SourceSpan,
) -> Result<PropertyDiff, ExecutorError> {
    PropertyDiff::new(set, removed).map_err(|source| {
        ExecutorError::data_exception(
            if matches!(source, selene_core::CoreError::StoredValue(_)) {
                DataExceptionSubclass::InvalidValueType
            } else {
                DataExceptionSubclass::MultipleAssignmentsToGraphElementProperty
            },
            format!("property diff construction failed: {source}"),
            span,
        )
    })
}

pub(super) fn graph_mutation(source: selene_graph::GraphError, span: SourceSpan) -> ExecutorError {
    if matches!(
        source,
        selene_graph::GraphError::NodeNotAlive { .. }
            | selene_graph::GraphError::EdgeNotAlive { .. }
    ) {
        return ExecutorError::data_exception(
            DataExceptionSubclass::InvalidReferenceValue,
            source.to_string(),
            span,
        );
    }
    ExecutorError::GraphMutation { source, span }
}

pub(super) fn unsupported_target(element: ElementKind) -> ExecutorError {
    match element {
        ElementKind::Path => ExecutorError::ImplementationDefined {
            detail: "path mutation target not implemented",
        },
        ElementKind::Alias => ExecutorError::ImplementationDefined {
            detail: "alias mutation target not supported",
        },
        ElementKind::Node | ElementKind::Edge => ExecutorError::ImplementationDefined {
            detail: "mutation target element kind not supported",
        },
    }
}
