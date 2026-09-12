//! Runtime helpers for named property-index DDL.

use std::collections::BTreeSet;

use selene_core::{DbString, PropertyValueType};
use selene_graph::{GraphTypeDef, NodeTypeDef, TypedIndexKind};

use super::runtime_db_string;
use crate::{
    SourceSpan,
    runtime::{ExecutorError, TxContext},
};

use super::super::catalog_index::{
    DropTarget, lookup_index_entries, render_drop_target, resolve_drop_index_matches,
};

pub(super) enum IndexPath {
    Single {
        property: DbString,
        kind: TypedIndexKind,
    },
    Composite {
        properties: Vec<DbString>,
        kinds: Vec<TypedIndexKind>,
    },
}

pub(super) fn create_index_plan(
    ctx: &TxContext<'_, '_>,
    name: DbString,
    label: DbString,
    properties: &[DbString],
    if_not_exists: bool,
    span: SourceSpan,
) -> Result<Option<IndexPath>, ExecutorError> {
    let graph = ctx.snapshot();
    let graph_type = graph
        .meta
        .bound_type
        .as_deref()
        .ok_or_else(|| ExecutorError::GraphTypeViolation {
            message:
                "CREATE INDEX requires a bound graph type; use CALL selene.create_index(...) on open graphs"
                    .to_owned(),
            span,
        })?;
    let node_type = index_node_type(graph_type, label.clone(), span)?;
    let path = dispatch_index_properties(node_type, label.clone(), properties, span)?;
    match path {
        IndexPath::Single { property, kind } => {
            create_single_index_plan(graph, name, label, property, kind, if_not_exists, span)
        }
        IndexPath::Composite { properties, kinds } => {
            create_composite_index_plan(graph, name, label, properties, kinds, if_not_exists, span)
        }
    }
}

fn create_single_index_plan(
    graph: &selene_graph::SeleneGraph,
    name: DbString,
    label: DbString,
    property: DbString,
    kind: TypedIndexKind,
    if_not_exists: bool,
    span: SourceSpan,
) -> Result<Option<IndexPath>, ExecutorError> {
    let report = lookup_index_entries(graph, name.clone(), label, std::slice::from_ref(&property));
    if !report.other_name_matches.is_empty() {
        return Err(ExecutorError::DuplicateObject {
            kind: "index",
            name,
            span,
        });
    }
    if let Some(existing_name) = report.same_pair_name {
        if if_not_exists {
            return Ok(None);
        }
        return Err(ExecutorError::DuplicateObject {
            kind: "index",
            name: runtime_db_string(&existing_name)?,
            span,
        });
    }
    Ok(Some(IndexPath::Single { property, kind }))
}

fn dispatch_index_properties(
    node_type: &NodeTypeDef,
    label: DbString,
    properties: &[DbString],
    span: SourceSpan,
) -> Result<IndexPath, ExecutorError> {
    match properties {
        [] => Err(ExecutorError::GraphTypeViolation {
            message: "CREATE INDEX requires at least one property".to_owned(),
            span,
        }),
        [property] => Ok(IndexPath::Single {
            property: property.clone(),
            kind: index_kind_for_property(node_type, label, property.clone(), span)?,
        }),
        _ => {
            let kinds = properties
                .iter()
                .map(|property| {
                    index_kind_for_property(node_type, label.clone(), property.clone(), span)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(IndexPath::Composite {
                properties: properties.to_vec(),
                kinds,
            })
        }
    }
}

fn create_composite_index_plan(
    graph: &selene_graph::SeleneGraph,
    name: DbString,
    label: DbString,
    properties: Vec<DbString>,
    kinds: Vec<TypedIndexKind>,
    if_not_exists: bool,
    span: SourceSpan,
) -> Result<Option<IndexPath>, ExecutorError> {
    let duplicates = duplicate_properties(&properties);
    if !duplicates.is_empty() {
        return Err(ExecutorError::GraphTypeViolation {
            message: format!(
                "composite index property list contains duplicates: {}",
                duplicates
                    .iter()
                    .map(|property| property.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            span,
        });
    }
    let report = lookup_index_entries(graph, name.clone(), label, &properties);
    if !report.other_name_matches.is_empty() {
        return Err(ExecutorError::DuplicateObject {
            kind: "index",
            name,
            span,
        });
    }
    if let Some(existing_name) = report.same_pair_name {
        if if_not_exists {
            return Ok(None);
        }
        return Err(ExecutorError::DuplicateObject {
            kind: "index",
            name: runtime_db_string(&existing_name)?,
            span,
        });
    }
    Ok(Some(IndexPath::Composite { properties, kinds }))
}

fn index_node_type(
    graph_type: &GraphTypeDef,
    label: DbString,
    span: SourceSpan,
) -> Result<&NodeTypeDef, ExecutorError> {
    if let Some(index) = graph_type.node_type_index_for(label.clone()) {
        return Ok(&graph_type.node_types[index as usize]);
    }
    if graph_type.edge_type_index_for(label.clone()).is_some() {
        return Err(ExecutorError::GraphTypeViolation {
            message: format!(
                "CREATE INDEX on edge label ':{}' -- edge-property indexes ship in BRIEF-140c",
                label.as_str()
            ),
            span,
        });
    }
    Err(ExecutorError::GraphTypeViolation {
        message: format!("node type ':{}' is not declared", label.as_str()),
        span,
    })
}

fn index_kind_for_property(
    node_type: &NodeTypeDef,
    label: DbString,
    property: DbString,
    span: SourceSpan,
) -> Result<TypedIndexKind, ExecutorError> {
    let property_def = node_type
        .properties
        .iter()
        .find(|candidate| candidate.name == property)
        .ok_or_else(|| ExecutorError::GraphTypeViolation {
            message: format!(
                "property '{}' is not declared on type ':{}'",
                property.as_str(),
                label.as_str()
            ),
            span,
        })?;
    match property_def.value_type {
        PropertyValueType::Bool => Ok(TypedIndexKind::Bool),
        PropertyValueType::Int => Ok(TypedIndexKind::I64),
        PropertyValueType::Uint => Ok(TypedIndexKind::U64),
        PropertyValueType::Int128 => Ok(TypedIndexKind::I128),
        PropertyValueType::Uint128 => Ok(TypedIndexKind::U128),
        PropertyValueType::Decimal => Ok(TypedIndexKind::Decimal),
        PropertyValueType::Float32 => Ok(TypedIndexKind::F32),
        PropertyValueType::Float => Ok(TypedIndexKind::F64),
        PropertyValueType::String => Ok(TypedIndexKind::String),
        PropertyValueType::Date => Ok(TypedIndexKind::Date),
        PropertyValueType::LocalDateTime => Ok(TypedIndexKind::LocalDateTime),
        PropertyValueType::ZonedDateTime => Ok(TypedIndexKind::ZonedDateTime),
        PropertyValueType::LocalTime => Ok(TypedIndexKind::LocalTime),
        PropertyValueType::ZonedTime => Ok(TypedIndexKind::ZonedTime),
        PropertyValueType::Duration
        | PropertyValueType::DurationYearToMonth
        | PropertyValueType::DurationDayToSecond => Ok(TypedIndexKind::Duration),
        PropertyValueType::Uuid => Ok(TypedIndexKind::Uuid),
        value_type => Err(ExecutorError::GraphTypeViolation {
            message: format!(
                "property kind {} is not supported for property indexes",
                value_type.name()
            ),
            span,
        }),
    }
}

pub(super) fn resolve_drop_index(
    graph: &selene_graph::SeleneGraph,
    name: DbString,
    if_exists: bool,
    span: SourceSpan,
) -> Result<Option<DropTarget>, ExecutorError> {
    let matches = resolve_drop_index_matches(graph, name.clone());
    match matches.as_slice() {
        [] if if_exists => Ok(None),
        [] => Err(ExecutorError::GraphTypeViolation {
            message: format!("index '{}' does not exist", name.as_str()),
            span,
        }),
        [pair] => Ok(Some(pair.clone())),
        pairs => Err(ExecutorError::GraphTypeViolation {
            message: format!(
                "index '{}' is ambiguous: matches {} entries across pairs {}",
                name.as_str(),
                pairs.len(),
                render_index_pair_list(pairs)
            ),
            span,
        }),
    }
}

fn render_index_pair_list(pairs: &[DropTarget]) -> String {
    pairs
        .iter()
        .map(render_drop_target)
        .collect::<Vec<_>>()
        .join(", ")
}

fn duplicate_properties(properties: &[DbString]) -> Vec<DbString> {
    let mut seen = BTreeSet::new();
    let mut duplicates = Vec::new();
    for property in properties {
        if !seen.insert(property.clone()) && !duplicates.contains(property) {
            duplicates.push(property.clone());
        }
    }
    duplicates
}
