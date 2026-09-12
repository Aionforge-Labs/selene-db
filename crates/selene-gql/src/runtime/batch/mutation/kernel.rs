//! Mutation kernels over one bounded input, using the shared graph mutator.

use std::collections::BTreeSet;

use selene_core::{
    DbString, EdgeId, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value,
};

use crate::{
    BindingTableColumn, BindingTableSchema, DeleteMode, DeleteTargetPlan, EdgeDirection,
    ElementKind, InsertEndpointRef, LabelExpr, MutationOp, ProjectExpr, PropertyInit, SourceSpan,
    SubqueryRegistry,
    analyze::ExprIdLookup,
    runtime::{
        Binding, BindingTable, DataExceptionSubclass, EvalCtx, ExecutorError, TxContext, evaluator,
    },
};

mod helpers;
use helpers::*;

pub(super) fn execute(
    op: &MutationOp,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    {
        let _mutator = ctx.mutator()?;
    }
    match op {
        MutationOp::InsertNode {
            site_id,
            label_expr,
            property_inits,
            output_column_index,
            output_column,
            span,
            ..
        } => execute_insert_node(
            *site_id,
            label_expr.as_ref(),
            property_inits,
            *output_column_index,
            output_column.as_ref(),
            *span,
            table,
            ctx,
            expr_ids,
            subqueries,
        ),
        MutationOp::InsertEdge {
            label_expr,
            left,
            right,
            direction,
            property_inits,
            output_column_index,
            output_column,
            span,
            ..
        } => execute_insert_edge(
            label_expr.as_ref(),
            *left,
            *right,
            *direction,
            property_inits,
            *output_column_index,
            output_column.as_ref(),
            *span,
            table,
            ctx,
            expr_ids,
            subqueries,
        ),
        MutationOp::SetProperty {
            element,
            target_column_index,
            key,
            value,
            span,
            ..
        } => execute_set_property(
            *element,
            *target_column_index,
            key.clone(),
            value,
            *span,
            table,
            ctx,
            expr_ids,
            subqueries,
        ),
        MutationOp::SetLabel {
            element,
            target_column_index,
            label,
            span,
            ..
        } => execute_set_label(
            *element,
            *target_column_index,
            label.clone(),
            *span,
            table,
            ctx,
        ),
        MutationOp::RemoveProperty {
            element,
            target_column_index,
            key,
            span,
            ..
        } => execute_remove_property(
            *element,
            *target_column_index,
            key.clone(),
            *span,
            table,
            ctx,
        ),
        MutationOp::RemoveLabel {
            element,
            target_column_index,
            label,
            span,
            ..
        } => execute_remove_label(
            *element,
            *target_column_index,
            label.clone(),
            *span,
            table,
            ctx,
        ),
        MutationOp::DeleteTargets {
            targets,
            mode,
            span,
            ..
        } => execute_delete_targets(targets, *mode, *span, table, ctx),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_insert_node(
    site_id: crate::InsertSiteId,
    label_expr: Option<&LabelExpr>,
    property_inits: &[PropertyInit],
    output_column_index: Option<u32>,
    output_column: Option<&BindingTableColumn>,
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    let labels = node_labels(label_expr, span)?;
    let (mut schema, rows) = table.into_parts();
    let input_schema = schema.clone();
    extend_schema(&mut schema, output_column_index, output_column)?;
    let mut output = Vec::with_capacity(rows.len());
    let mut rows_since_check = 0;
    for row in rows {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let props = {
            let eval_ctx = EvalCtx {
                tx: ctx,
                expr_ids,
                subqueries,
            };
            property_map(
                property_inits,
                PropertyMapTarget::Node,
                &row,
                &input_schema,
                &eval_ctx,
            )?
        };
        let node_id = {
            let mut mutator = ctx.mutator()?;
            mutator
                .create_node(labels.clone(), props)
                .map_err(|source| graph_mutation(source, span))?
        };
        let mut values = row.cloned_values();
        if let Some(index) = output_column_index {
            set_output_value(&mut values, index, Value::NodeRef(node_id));
        }
        let mut insert_sites = row.cloned_insert_sites();
        record_insert_site(&mut insert_sites, site_id, node_id);
        output.push(Binding::from_parts(values, insert_sites));
    }
    Ok(BindingTable::new(schema, output))
}

#[allow(clippy::too_many_arguments)]
fn execute_insert_edge(
    label_expr: Option<&LabelExpr>,
    left: InsertEndpointRef,
    right: InsertEndpointRef,
    direction: EdgeDirection,
    property_inits: &[PropertyInit],
    output_column_index: Option<u32>,
    output_column: Option<&BindingTableColumn>,
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    let label = edge_label(label_expr, span)?;
    let (mut schema, rows) = table.into_parts();
    let input_schema = schema.clone();
    extend_schema(&mut schema, output_column_index, output_column)?;
    let mut output = Vec::with_capacity(rows.len());
    let mut rows_since_check = 0;
    for row in rows {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let left = endpoint_node(&row, left, span)?;
        let right = endpoint_node(&row, right, span)?;
        let (source, target) = edge_endpoints(left, right, direction, span)?;
        let props = {
            let eval_ctx = EvalCtx {
                tx: ctx,
                expr_ids,
                subqueries,
            };
            property_map(
                property_inits,
                PropertyMapTarget::Edge,
                &row,
                &input_schema,
                &eval_ctx,
            )?
        };
        let edge_id = {
            let mut mutator = ctx.mutator()?;
            mutator
                .create_mixed_edge(
                    label.clone(),
                    source,
                    target,
                    if direction == EdgeDirection::Undirected {
                        selene_core::EdgeDirectionality::Undirected
                    } else {
                        selene_core::EdgeDirectionality::Directed
                    },
                    props,
                )
                .map_err(|source| graph_mutation(source, span))?
        };
        let mut values = row.cloned_values();
        if let Some(index) = output_column_index {
            set_output_value(&mut values, index, Value::EdgeRef(edge_id));
        }
        output.push(Binding::from_parts(values, row.cloned_insert_sites()));
    }
    Ok(BindingTable::new(schema, output))
}

#[allow(clippy::too_many_arguments)]
fn execute_set_property(
    element: ElementKind,
    target_column_index: u32,
    key: DbString,
    value: &ProjectExpr,
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    let (schema, rows) = table.into_parts();
    let mut rows_since_check = 0;
    for row in &rows {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let value = {
            let eval_ctx = EvalCtx {
                tx: ctx,
                expr_ids,
                subqueries,
            };
            evaluator::evaluate(&value.expr, row, &schema, &eval_ctx)?
        };
        let diff = property_diff([(key.clone(), value)], [], span)?;
        match element {
            ElementKind::Node => {
                if let Some(id) = target_node(row, target_column_index, span)? {
                    ctx.mutator()?
                        .update_node(id, label_diff([], [], span)?, diff)
                        .map_err(|source| graph_mutation(source, span))?;
                }
            }
            ElementKind::Edge => {
                if let Some(id) = target_edge(row, target_column_index, span)? {
                    ctx.mutator()?
                        .update_edge(id, diff)
                        .map_err(|source| graph_mutation(source, span))?;
                }
            }
            ElementKind::Path | ElementKind::Alias => {
                return Err(unsupported_target(element));
            }
        }
    }
    Ok(BindingTable::new(schema, rows))
}

fn execute_set_label(
    element: ElementKind,
    target_column_index: u32,
    label: DbString,
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let diff = label_diff([label], [], span)?;
    let mut rows_since_check = 0;
    for row in table.rows() {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        match element {
            ElementKind::Node => {
                if let Some(id) = target_node(row, target_column_index, span)? {
                    ctx.mutator()?
                        .update_node(id, diff.clone(), property_diff([], [], span)?)
                        .map_err(|source| graph_mutation(source, span))?;
                }
            }
            ElementKind::Edge | ElementKind::Path | ElementKind::Alias => {
                return Err(unsupported_target(element));
            }
        }
    }
    Ok(table)
}

fn execute_remove_property(
    element: ElementKind,
    target_column_index: u32,
    key: DbString,
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let mut rows_since_check = 0;
    for row in table.rows() {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        match element {
            ElementKind::Node => {
                if let Some(id) = target_node(row, target_column_index, span)? {
                    ctx.mutator()?
                        .remove_node_property(id, key.clone())
                        .map_err(|source| graph_mutation(source, span))?;
                }
            }
            ElementKind::Edge => {
                if let Some(id) = target_edge(row, target_column_index, span)? {
                    ctx.mutator()?
                        .remove_edge_property(id, key.clone())
                        .map_err(|source| graph_mutation(source, span))?;
                }
            }
            ElementKind::Path | ElementKind::Alias => {
                return Err(unsupported_target(element));
            }
        }
    }
    Ok(table)
}

fn execute_remove_label(
    element: ElementKind,
    target_column_index: u32,
    label: DbString,
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let mut rows_since_check = 0;
    for row in table.rows() {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        match element {
            ElementKind::Node => {
                if let Some(id) = target_node(row, target_column_index, span)? {
                    ctx.mutator()?
                        .remove_node_label(id, label.clone())
                        .map_err(|source| graph_mutation(source, span))?;
                }
            }
            ElementKind::Edge | ElementKind::Path | ElementKind::Alias => {
                return Err(unsupported_target(element));
            }
        }
    }
    Ok(table)
}

fn execute_delete_targets(
    targets: &[DeleteTargetPlan],
    mode: DeleteMode,
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let mut nodes = BTreeSet::new();
    let mut edges = BTreeSet::new();
    let mut rows_since_check = 0;
    for row in table.rows() {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        for target in targets {
            match target.element {
                ElementKind::Node => {
                    if let Some(id) = target_node(row, target.target_column_index, span)?
                        && ctx.snapshot().is_node_alive(id)
                    {
                        nodes.insert(id);
                        if matches!(mode, DeleteMode::Detach) {
                            add_incident_edges(ctx, id, &mut edges);
                        }
                    }
                }
                ElementKind::Edge => {
                    if let Some(id) = target_edge(row, target.target_column_index, span)?
                        && ctx.snapshot().is_edge_alive(id)
                    {
                        edges.insert(id);
                    }
                }
                ElementKind::Path | ElementKind::Alias => {
                    return Err(unsupported_target(target.element));
                }
            }
        }
    }
    if matches!(mode, DeleteMode::Bare | DeleteMode::NoDetach) {
        ensure_delete_nodes_detached(&nodes, &edges, span, ctx)?;
    }
    ctx.mutator()?
        .delete_elements(nodes, edges)
        .map_err(|source| graph_mutation(source, span))?;
    Ok(table)
}

fn add_incident_edges(ctx: &TxContext<'_, '_>, node: NodeId, edges: &mut BTreeSet<EdgeId>) {
    if let Some(entry) = ctx.snapshot().outgoing_edges(node) {
        edges.extend(entry.iter().map(|edge| edge.edge_id));
    }
    if let Some(entry) = ctx.snapshot().incoming_edges(node) {
        edges.extend(entry.iter().map(|edge| edge.edge_id));
    }
}

fn ensure_delete_nodes_detached(
    nodes: &BTreeSet<NodeId>,
    edges: &BTreeSet<EdgeId>,
    span: SourceSpan,
    ctx: &TxContext<'_, '_>,
) -> Result<(), ExecutorError> {
    for node in nodes {
        if incident_edge_outside_delete_set(ctx, *node, edges) {
            return Err(ExecutorError::DependentObjectStillExists {
                message: "cannot delete node with incident edges (use DETACH DELETE)".to_owned(),
                span,
            });
        }
    }
    Ok(())
}

fn incident_edge_outside_delete_set(
    ctx: &TxContext<'_, '_>,
    node: NodeId,
    edges: &BTreeSet<EdgeId>,
) -> bool {
    ctx.snapshot()
        .outgoing_edges(node)
        .is_some_and(|entry| entry.iter().any(|edge| !edges.contains(&edge.edge_id)))
        || ctx
            .snapshot()
            .incoming_edges(node)
            .is_some_and(|entry| entry.iter().any(|edge| !edges.contains(&edge.edge_id)))
}
