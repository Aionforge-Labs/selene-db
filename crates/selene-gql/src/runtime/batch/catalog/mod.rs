//! Single-shot physical catalog operation over the borrowed transaction.
//!
//! DDL is statement-scoped, never repeated per input batch. The mutator owns
//! graph-type and index validation; selected database-catalog commands instead
//! travel through the prepared request to the facade's detached CatalogStager.

mod alter_edge_type;
mod alter_node_type;
mod compose;
mod drop_cascade;
mod endpoints;
mod index_ddl;
mod procedure;
mod property;
mod render_type;

use selene_core::{DbString, LabelSet, Value, db_string};
use selene_graph::{
    EdgeEndpointDef, EdgeTypeDef, GraphError, GraphTypeDef, NodeTypeDef, PropertyTypeDef,
    TypedIndexKind, ValidationMode as GraphValidationMode,
};
use smallvec::SmallVec;

use self::{
    compose::{compose_edge_properties, compose_node_properties},
    endpoints::resolve_endpoints,
    index_ddl::{IndexPath, create_index_plan, resolve_drop_index},
    procedure::{procedure_row, render_procedure_name},
    property::{property_defs, render_property_default_value, render_property_value_type},
    render_type::render_gql_type,
};
use super::catalog_index::{
    DropTarget, inline_index_specs, render_index_kind, render_index_name, render_vector_index_kind,
    render_vector_index_name, validate_index_name_collisions,
};
use crate::{
    AnalyzedType, BindingTableColumn, BindingTableSchema, CatalogOp, GqlType, SourceSpan,
    ast::format_ident::fmt_ident,
    runtime::{Binding, BindingTable, ExecutorError, TxContext},
};

const DATABASE_CATALOG_DETAIL: &str = "database catalog statements require the database facade; a bare engine session owns one graph and no catalog";
const OPEN_GRAPH_CATALOG_DDL: &str =
    "open graph (GG01) does not support catalog type DDL -- use a closed graph (GG02)";

pub(crate) fn execute(
    op: &CatalogOp,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    match op {
        // Database-catalog statements are intercepted by the database facade.
        // A bare engine session has no catalog to mutate and must not silently
        // reinterpret or ignore the command.
        CatalogOp::DatabaseCatalog(_) => Err(ExecutorError::ImplementationDefined {
            detail: DATABASE_CATALOG_DETAIL,
        }),
        CatalogOp::CreateNodeType {
            label,
            key_labels,
            or_replace,
            if_not_exists,
            extends,
            properties,
            validation_mode,
            span,
        } => {
            reject_or_replace(*or_replace)?;
            ctx.ensure_write_txn("catalog op invoked without write transaction", *span)?;
            if node_type_exists(ctx.snapshot().meta.bound_type.as_deref(), label.clone()) {
                if *if_not_exists {
                    return Ok(table);
                }
                return Err(ExecutorError::DuplicateObject {
                    kind: "node type",
                    name: label.clone(),
                    span: *span,
                });
            }
            let indexes = inline_index_specs(properties)?;
            validate_index_name_collisions(label.clone(), &indexes, ctx.snapshot())?;
            let properties = property_defs(properties, true)?;
            let properties = if let Some(parent) = extends {
                compose_node_properties(
                    &closed_graph_type(ctx.snapshot(), *span)?,
                    label.clone(),
                    parent.clone(),
                    properties,
                    *span,
                )?
            } else {
                properties
            };
            {
                let mut mutator =
                    ctx.mutator_with_span("catalog op invoked without write transaction", *span)?;
                mutator
                    .create_node_type(
                        label.clone(),
                        key_label_set(key_labels, label.clone()),
                        properties,
                        graph_validation_mode(*validation_mode),
                    )
                    .map_err(|source| catalog_graph_error(source, *span))?;
                for index in indexes {
                    mutator
                        .create_property_index_named(
                            label.clone(),
                            index.property,
                            index.kind,
                            index.name,
                        )
                        .map_err(|source| catalog_graph_error(source, index.span))?;
                }
            }
            Ok(table)
        }
        CatalogOp::CreateEdgeType {
            label,
            key_labels,
            or_replace,
            if_not_exists,
            extends,
            endpoints,
            properties,
            validation_mode,
            span,
        } => {
            reject_or_replace(*or_replace)?;
            ctx.ensure_write_txn("catalog op invoked without write transaction", *span)?;
            if edge_type_exists(ctx.snapshot().meta.bound_type.as_deref(), label.clone()) {
                if *if_not_exists {
                    return Ok(table);
                }
                return Err(ExecutorError::DuplicateObject {
                    kind: "edge type",
                    name: label.clone(),
                    span: *span,
                });
            }
            let graph_type = closed_graph_type(ctx.snapshot(), *span)?;
            let (source, target) = endpoints
                .as_ref()
                .map(|endpoints| resolve_endpoints(endpoints, &graph_type, *span))
                .transpose()?
                .unwrap_or((EdgeEndpointDef::Any, EdgeEndpointDef::Any));
            let properties = property_defs(properties, false)?;
            let properties = if let Some(parent) = extends {
                compose_edge_properties(
                    &graph_type,
                    label.clone(),
                    parent.clone(),
                    properties,
                    *span,
                )?
            } else {
                properties
            };
            ctx.mutator_with_span("catalog op invoked without write transaction", *span)?
                .create_edge_type(
                    label.clone(),
                    edge_key_label(key_labels, label.clone()),
                    source,
                    target,
                    properties,
                    graph_validation_mode(*validation_mode),
                )
                .map_err(|source| catalog_graph_error(source, *span))?;
            Ok(table)
        }
        CatalogOp::AlterNodeType {
            label,
            properties,
            span,
        } => alter_node_type::execute(label.clone(), properties, *span, table, ctx),
        CatalogOp::AlterEdgeType {
            label,
            endpoints,
            properties,
            span,
        } => alter_edge_type::execute(
            label.clone(),
            endpoints.as_ref(),
            properties,
            *span,
            table,
            ctx,
        ),
        CatalogOp::DropNodeType {
            label,
            if_exists,
            behavior,
            span,
        } => drop_cascade::execute_drop_node_type(
            label.clone(),
            *if_exists,
            *behavior,
            *span,
            table,
            ctx,
        ),
        CatalogOp::DropEdgeType {
            label,
            if_exists,
            behavior,
            span,
        } => drop_cascade::execute_drop_edge_type(
            label.clone(),
            *if_exists,
            *behavior,
            *span,
            table,
            ctx,
        ),
        CatalogOp::TruncateNodeType { label, span } => {
            // TRUNCATE removes instances by label (IM_TRUNCATE, audit Item 11).
            // It is valid on both open (GG01) and closed (GG02) graphs — unlike
            // DROP NODE TYPE it never touches the catalog, so no closed-graph
            // gate. An absent label is a clean no-op inside the mutator.
            ctx.ensure_write_txn("catalog op invoked without write transaction", *span)?;
            ctx.mutator_with_span("catalog op invoked without write transaction", *span)?
                .truncate_node_type(label.clone())
                .map_err(|source| catalog_graph_error(source, *span))?;
            Ok(table)
        }
        CatalogOp::TruncateEdgeType { label, span } => {
            ctx.ensure_write_txn("catalog op invoked without write transaction", *span)?;
            ctx.mutator_with_span("catalog op invoked without write transaction", *span)?
                .truncate_edge_type(label.clone())
                .map_err(|source| catalog_graph_error(source, *span))?;
            Ok(table)
        }
        CatalogOp::CreateIndex {
            name,
            label,
            properties,
            if_not_exists,
            span,
        } => {
            ctx.ensure_write_txn("catalog op invoked without write transaction", *span)?;
            let Some(index_path) = create_index_plan(
                ctx,
                name.clone(),
                label.clone(),
                properties,
                *if_not_exists,
                *span,
            )?
            else {
                return Ok(table);
            };
            match index_path {
                IndexPath::Single { property, kind } => {
                    ctx.mutator_with_span("catalog op invoked without write transaction", *span)?
                        .create_property_index_named(
                            label.clone(),
                            property,
                            kind,
                            Some(name.clone()),
                        )
                        .map_err(|source| catalog_graph_error(source, *span))?;
                }
                IndexPath::Composite { properties, kinds } => {
                    let properties = properties.into_iter().collect::<SmallVec<[DbString; 4]>>();
                    let kinds = kinds.into_iter().collect::<SmallVec<[TypedIndexKind; 4]>>();
                    ctx.mutator_with_span("catalog op invoked without write transaction", *span)?
                        .create_composite_property_index_named(
                            label.clone(),
                            properties,
                            kinds,
                            Some(name.clone()),
                        )
                        .map_err(|source| catalog_graph_error(source, *span))?;
                }
            }
            Ok(table)
        }
        CatalogOp::DropIndex {
            name,
            if_exists,
            span,
        } => {
            ctx.ensure_write_txn("catalog op invoked without write transaction", *span)?;
            let Some(target) = resolve_drop_index(ctx.snapshot(), name.clone(), *if_exists, *span)?
            else {
                return Ok(table);
            };
            match target {
                DropTarget::Single { label, property } => {
                    ctx.mutator_with_span("catalog op invoked without write transaction", *span)?
                        .drop_property_index(label, property)
                        .map_err(|source| catalog_graph_error(source, *span))?;
                }
                DropTarget::Composite { label, properties } => {
                    ctx.mutator_with_span("catalog op invoked without write transaction", *span)?
                        .drop_composite_property_index(label, properties)
                        .map_err(|source| catalog_graph_error(source, *span))?;
                }
            }
            Ok(table)
        }
        CatalogOp::ShowNodeTypes(_) => show_node_types(ctx),
        CatalogOp::ShowEdgeTypes(_) => show_edge_types(ctx),
        CatalogOp::ShowIndexes(_) => show_indexes(ctx),
        CatalogOp::ShowProcedures(_) => show_procedures(ctx),
    }
}

/// Element-type DDL is not an ISO statement; its `OR REPLACE` modifier is
/// rejected by the Flagger and, for directly constructed plans, here.
fn reject_or_replace(or_replace: bool) -> Result<(), ExecutorError> {
    if or_replace {
        return Err(ExecutorError::ImplementationDefined {
            detail: "OR REPLACE is not implemented for element-type DDL",
        });
    }
    Ok(())
}

const fn graph_validation_mode(mode: Option<crate::ValidationMode>) -> GraphValidationMode {
    match mode {
        Some(crate::ValidationMode::Warn) => GraphValidationMode::Warn,
        Some(crate::ValidationMode::Strict) | None => GraphValidationMode::Strict,
    }
}

/// Build the node-type key label set from the planned key labels (Feature
/// GG21), falling back to the implied singleton `:label` when no explicit key
/// label set was written (Feature GG20, ISO/IEC 39075:2024 §18.2 SR5c).
///
/// The planner has already enforced the IL003 cardinality cap, so a non-empty
/// `key_labels` is a validated singleton equal to the type-name label; the
/// resulting `LabelSet` is byte-identical to the pre-GG21 `LabelSet::single`
/// (no snapshot/WAL format change — content only).
fn key_label_set(key_labels: &[DbString], label: DbString) -> LabelSet {
    if key_labels.is_empty() {
        LabelSet::single(label)
    } else {
        LabelSet::from_iter(key_labels.iter().cloned())
    }
}

/// Pick the edge-type key label (the single `EdgeTypeDef.label` discriminator)
/// from the planned key labels, falling back to the implied type-name label.
/// Under the IL003 singleton cap a non-empty `key_labels` carries exactly one
/// label equal to the type name.
fn edge_key_label(key_labels: &[DbString], label: DbString) -> DbString {
    key_labels.first().cloned().unwrap_or(label)
}

fn node_type_exists(graph_type: Option<&GraphTypeDef>, label: DbString) -> bool {
    graph_type
        .map(|graph_type| {
            graph_type
                .node_types
                .iter()
                .any(|node_type| node_type.name == label)
        })
        .unwrap_or(false)
}

fn edge_type_exists(graph_type: Option<&GraphTypeDef>, label: DbString) -> bool {
    graph_type
        .map(|graph_type| {
            graph_type
                .edge_types
                .iter()
                .any(|edge_type| edge_type.name == label)
        })
        .unwrap_or(false)
}

pub(super) fn closed_graph_type(
    graph: &selene_graph::SeleneGraph,
    span: SourceSpan,
) -> Result<GraphTypeDef, ExecutorError> {
    graph
        .meta
        .bound_type
        .as_deref()
        .cloned()
        .ok_or_else(|| ExecutorError::GraphTypeViolation {
            message: OPEN_GRAPH_CATALOG_DDL.to_owned(),
            span,
        })
}

fn show_node_types(ctx: &TxContext<'_, '_>) -> Result<BindingTable, ExecutorError> {
    let rows = ctx
        .snapshot()
        .meta
        .bound_type
        .as_deref()
        .map(|graph_type| {
            graph_type
                .node_types
                .iter()
                .map(|node_type| {
                    let label = render_node_label_name(&node_type.key_labels);
                    show_row(&label, &render_node_type_def(node_type)?)
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(BindingTable::new(show_schema()?, rows))
}

fn show_edge_types(ctx: &TxContext<'_, '_>) -> Result<BindingTable, ExecutorError> {
    let rows = ctx
        .snapshot()
        .meta
        .bound_type
        .as_deref()
        .map(|graph_type| {
            graph_type
                .edge_types
                .iter()
                .map(|edge_type| {
                    show_row(
                        edge_type.label.as_str(),
                        &render_edge_type_def(graph_type, edge_type)?,
                    )
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(BindingTable::new(show_schema()?, rows))
}

fn show_indexes(ctx: &TxContext<'_, '_>) -> Result<BindingTable, ExecutorError> {
    let mut indexes = ctx
        .snapshot()
        .iter_property_index_entries()
        .map(|(label, property, kind, name)| {
            (
                render_index_name(label.clone(), property.clone(), name),
                label,
                property,
                render_index_kind(kind).to_owned(),
            )
        })
        .collect::<Vec<_>>();
    indexes.extend(ctx.snapshot().iter_vector_index_entries().map(
        |(label, property, kind, dimension, hnsw_config, ivf_config, name)| {
            (
                render_vector_index_name(label.clone(), property.clone(), name),
                label,
                property,
                render_vector_index_kind(kind, dimension, hnsw_config, ivf_config),
            )
        },
    ));
    indexes.sort_by(
        |(_, left_label, left_property, left_kind),
         (_, right_label, right_property, right_kind)| {
            left_label
                .as_str()
                .cmp(right_label.as_str())
                .then_with(|| left_property.as_str().cmp(right_property.as_str()))
                .then_with(|| left_kind.cmp(right_kind))
        },
    );
    let rows = indexes
        .into_iter()
        .map(|(name, label, property, kind)| {
            Ok(Binding::new([
                Value::String(runtime_db_string_owned(name)?),
                Value::String(label),
                Value::String(property),
                Value::String(runtime_db_string_owned(kind)?),
            ]))
        })
        .collect::<Result<Vec<_>, ExecutorError>>()?;
    Ok(BindingTable::new(
        string_schema(&["name", "label", "property", "kind"])?,
        rows,
    ))
}

fn show_procedures(ctx: &TxContext<'_, '_>) -> Result<BindingTable, ExecutorError> {
    let mut procedures = ctx.registry().iter_handles().collect::<Vec<_>>();
    procedures.sort_by(|(left, _), (right, _)| {
        render_procedure_name(left).cmp(&render_procedure_name(right))
    });
    let rows = procedures
        .into_iter()
        .map(|(name, metadata)| procedure_row(&name, &metadata))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BindingTable::new(
        string_schema(&[
            "name",
            "tier",
            "mutability",
            "signature",
            "description",
            "since_version",
        ])?,
        rows,
    ))
}

fn show_row(label: &str, definition: &str) -> Result<Binding, ExecutorError> {
    Ok(Binding::new([
        Value::String(runtime_db_string(label)?),
        Value::String(runtime_db_string(definition)?),
    ]))
}

fn show_schema() -> Result<BindingTableSchema, ExecutorError> {
    Ok(BindingTableSchema {
        columns: vec![
            BindingTableColumn {
                name: Some(runtime_db_string("label")?),
                hidden: None,
                ty: AnalyzedType::Resolved(GqlType::String),
            },
            BindingTableColumn {
                name: Some(runtime_db_string("definition")?),
                hidden: None,
                ty: AnalyzedType::DYNAMIC,
            },
        ],
    })
}

fn string_schema(names: &[&str]) -> Result<BindingTableSchema, ExecutorError> {
    let columns = names
        .iter()
        .map(|name| {
            Ok(BindingTableColumn {
                name: Some(runtime_db_string(name)?),
                hidden: None,
                ty: AnalyzedType::Resolved(GqlType::String),
            })
        })
        .collect::<Result<Vec<_>, ExecutorError>>()?;
    Ok(BindingTableSchema { columns })
}

pub(super) fn runtime_db_string(value: &str) -> Result<DbString, ExecutorError> {
    db_string(value).map_err(|_err| ExecutorError::ImplementationDefined {
        detail: "string construction failed during catalog rendering",
    })
}

pub(super) fn runtime_db_string_owned(value: String) -> Result<DbString, ExecutorError> {
    DbString::from_string(value).map_err(|_err| ExecutorError::ImplementationDefined {
        detail: "string construction failed during catalog rendering",
    })
}

fn render_node_type_def(node_type: &NodeTypeDef) -> Result<String, ExecutorError> {
    Ok(format!(
        "CREATE NODE TYPE {} ({})",
        render_node_label_set(&node_type.key_labels),
        render_properties(&node_type.properties)?
    ))
}

fn render_edge_type_def(
    graph_type: &GraphTypeDef,
    edge_type: &EdgeTypeDef,
) -> Result<String, ExecutorError> {
    let endpoint_clause = render_edge_endpoint_clause(graph_type, edge_type);
    let properties = render_properties(&edge_type.properties)?;
    let body = match (endpoint_clause.is_empty(), properties.is_empty()) {
        (true, true) => String::new(),
        (true, false) => properties,
        (false, true) => endpoint_clause,
        (false, false) => format!("{endpoint_clause}, {properties}"),
    };
    Ok(format!(
        "CREATE EDGE TYPE :{} ({body})",
        fmt_ident(edge_type.label.clone())
    ))
}

fn render_edge_endpoint_clause(graph_type: &GraphTypeDef, edge_type: &EdgeTypeDef) -> String {
    if edge_type.source_node_type == EdgeEndpointDef::Any
        || edge_type.target_node_type == EdgeEndpointDef::Any
    {
        // Partial Any has no DDL syntax; keep SHOW output parseable.
        // OneOf endpoints ARE renderable (their member labels form a valid
        // enumerated-FROM/TO clause), so they fall through to render_endpoint.
        return String::new();
    }
    let source = render_endpoint(graph_type, &edge_type.source_node_type);
    let target = render_endpoint(graph_type, &edge_type.target_node_type);
    format!("FROM {source} TO {target}")
}

fn render_endpoint(graph_type: &GraphTypeDef, endpoint: &EdgeEndpointDef) -> String {
    match endpoint {
        EdgeEndpointDef::Any => "ANY".to_owned(),
        EdgeEndpointDef::NodeType(index) => {
            render_endpoint_label_set(&graph_type.node_types[*index as usize].key_labels)
        }
        EdgeEndpointDef::OneOf(indices) => indices
            .iter()
            .map(|index| {
                render_endpoint_label_set(&graph_type.node_types[*index as usize].key_labels)
            })
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn render_endpoint_label_set(labels: &LabelSet) -> String {
    labels
        .iter()
        .map(|label| format!(":{}", fmt_ident(label.clone())))
        .collect::<Vec<_>>()
        .join(",")
}

fn render_node_label_set(labels: &LabelSet) -> String {
    let label = render_node_label_name(labels);
    if label.is_empty() {
        String::new()
    } else {
        format!(":{label}")
    }
}

fn render_node_label_name(labels: &LabelSet) -> String {
    labels
        .iter()
        .map(|label| fmt_ident(label.clone()))
        .collect::<Vec<_>>()
        .join(":")
}

fn render_properties(properties: &[PropertyTypeDef]) -> Result<String, ExecutorError> {
    let rendered = properties
        .iter()
        .map(|property| {
            let nullability = if property.required { " NOT NULL" } else { "" };
            let default = match property.default.as_ref() {
                Some(value) => format!(" DEFAULT {}", render_property_default_value(value)?),
                None => String::new(),
            };
            let immutable = if property.immutable { " IMMUTABLE" } else { "" };
            let unique = if property.unique { " UNIQUE" } else { "" };
            Ok(format!(
                "{} :: {}{}{}{}{}",
                fmt_ident(property.name.clone()),
                render_property_value_type(
                    property.value_type,
                    property.list_element_type.as_ref(),
                    property.record_field_types.as_ref(),
                    property.decimal_type,
                    property.character_string_type,
                    property.byte_string_type,
                ),
                nullability,
                default,
                immutable,
                unique
            ))
        })
        .collect::<Result<Vec<_>, ExecutorError>>()?;
    Ok(rendered.join(", "))
}

pub(super) fn catalog_graph_error(source: GraphError, span: SourceSpan) -> ExecutorError {
    match source {
        GraphError::Inconsistent { reason } => ExecutorError::GraphTypeViolation {
            message: reason,
            span,
        },
        source => ExecutorError::GraphMutation { source, span },
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
