//! Inline property-index helpers for catalog DDL.

use selene_core::{DbString, HnswIndexConfig, IvfIndexConfig};
use selene_graph::{TypedIndexKind, VectorIndexKind};
use smallvec::SmallVec;

use super::catalog::runtime_db_string_owned;
use crate::{
    ExecutorError, GqlType, PlannedTypePropertyConstraint, PlannedTypePropertyDef, SourceSpan,
};

pub(super) struct InlineIndexSpec {
    pub(super) property: DbString,
    pub(super) kind: TypedIndexKind,
    pub(super) name: Option<DbString>,
    pub(super) span: SourceSpan,
}

pub(super) struct IndexConflictReport {
    pub(super) same_pair_name: Option<String>,
    pub(super) other_name_matches: Vec<DropTarget>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DropTarget {
    Single {
        label: DbString,
        property: DbString,
    },
    Composite {
        label: DbString,
        properties: SmallVec<[DbString; 4]>,
    },
}

pub(super) fn inline_index_specs(
    properties: &[PlannedTypePropertyDef],
) -> Result<Vec<InlineIndexSpec>, ExecutorError> {
    let mut indexes = Vec::new();
    for property in properties {
        for constraint in &property.constraints {
            if let PlannedTypePropertyConstraint::Indexed { name, span } = constraint {
                indexes.push(InlineIndexSpec {
                    property: property.name.clone(),
                    kind: gql_type_to_index_kind(&property.gql_type, *span)?,
                    name: name.clone(),
                    span: *span,
                });
            }
        }
    }
    Ok(indexes)
}

pub(super) fn validate_index_name_collisions(
    label: DbString,
    indexes: &[InlineIndexSpec],
    graph: &selene_graph::SeleneGraph,
) -> Result<(), ExecutorError> {
    let mut used = graph
        .iter_property_index_entries()
        .map(|(label, property, _, name)| render_index_name(label, property, name))
        .collect::<Vec<_>>();
    used.extend(
        graph
            .iter_composite_property_index_entries()
            .map(|(label, properties, _, name)| {
                render_composite_index_name(label, &properties, name)
            }),
    );
    used.extend(
        graph
            .iter_vector_index_entries()
            .map(|(label, property, _, _, _, _, name)| {
                render_vector_index_name(label, property, name)
            }),
    );
    for index in indexes {
        let rendered = render_index_name(label.clone(), index.property.clone(), index.name.clone());
        if used.iter().any(|name| name == &rendered) {
            let name = index
                .name
                .clone()
                .unwrap_or(runtime_db_string_owned(rendered)?);
            return Err(ExecutorError::DuplicateObject {
                kind: "index",
                name,
                span: index.span,
            });
        }
        used.push(rendered);
    }
    Ok(())
}

pub(super) fn lookup_index_entries(
    graph: &selene_graph::SeleneGraph,
    ident: DbString,
    label: DbString,
    properties: &[DbString],
) -> IndexConflictReport {
    let mut same_pair_name = None;
    let mut other_name_matches = Vec::new();
    for (entry_label, entry_property, _, entry_name) in graph.iter_property_index_entries() {
        if entry_label == label && properties == [entry_property.clone()] {
            same_pair_name = Some(render_index_name(entry_label, entry_property, entry_name));
            continue;
        }
        if render_index_name(entry_label.clone(), entry_property.clone(), entry_name)
            == ident.as_str()
        {
            other_name_matches.push(DropTarget::Single {
                label: entry_label,
                property: entry_property,
            });
        }
    }
    for (entry_label, entry_properties, _, entry_name) in
        graph.iter_composite_property_index_entries()
    {
        if entry_label == label && same_property_set(&entry_properties, properties) {
            same_pair_name = Some(render_composite_index_name(
                entry_label,
                &entry_properties,
                entry_name,
            ));
            continue;
        }
        if render_composite_index_name(entry_label.clone(), &entry_properties, entry_name)
            == ident.as_str()
        {
            other_name_matches.push(DropTarget::Composite {
                label: entry_label,
                properties: entry_properties,
            });
        }
    }
    IndexConflictReport {
        same_pair_name,
        other_name_matches,
    }
}

pub(super) fn resolve_drop_index_matches(
    graph: &selene_graph::SeleneGraph,
    ident: DbString,
) -> Vec<DropTarget> {
    let mut matches = graph
        .iter_property_index_entries()
        .filter_map(|(label, property, _, name)| {
            (render_index_name(label.clone(), property.clone(), name) == ident.as_str())
                .then_some(DropTarget::Single { label, property })
        })
        .collect::<Vec<_>>();
    matches.extend(graph.iter_composite_property_index_entries().filter_map(
        |(label, properties, _, name)| {
            (render_composite_index_name(label.clone(), &properties, name) == ident.as_str())
                .then_some(DropTarget::Composite { label, properties })
        },
    ));
    matches.sort_by_key(render_drop_target);
    matches
}

fn gql_type_to_index_kind(
    gql_type: &GqlType,
    span: SourceSpan,
) -> Result<TypedIndexKind, ExecutorError> {
    match gql_type {
        GqlType::Boolean => Ok(TypedIndexKind::Bool),
        GqlType::String | GqlType::CharacterString(_) => Ok(TypedIndexKind::String),
        GqlType::Uuid => Ok(TypedIndexKind::Uuid),
        GqlType::Integer
        | GqlType::Int8
        | GqlType::Int16
        | GqlType::Int32
        | GqlType::Int64
        | GqlType::SmallInt
        | GqlType::BigInt => Ok(TypedIndexKind::I64),
        GqlType::Uint8
        | GqlType::Uint16
        | GqlType::Uint32
        | GqlType::Uint64
        | GqlType::USmallInt
        | GqlType::Uint
        | GqlType::UBigInt => Ok(TypedIndexKind::U64),
        GqlType::Int128 => Ok(TypedIndexKind::I128),
        GqlType::Uint128 => Ok(TypedIndexKind::U128),
        GqlType::Decimal | GqlType::DecimalExact(_) => Ok(TypedIndexKind::Decimal),
        GqlType::Float32 | GqlType::Real => Ok(TypedIndexKind::F32),
        GqlType::Float64 | GqlType::Double => Ok(TypedIndexKind::F64),
        GqlType::Date => Ok(TypedIndexKind::Date),
        GqlType::LocalDateTime => Ok(TypedIndexKind::LocalDateTime),
        GqlType::ZonedDateTime => Ok(TypedIndexKind::ZonedDateTime),
        GqlType::LocalTime => Ok(TypedIndexKind::LocalTime),
        GqlType::ZonedTime => Ok(TypedIndexKind::ZonedTime),
        GqlType::Duration | GqlType::DurationYearToMonth | GqlType::DurationDayToSecond => {
            Ok(TypedIndexKind::Duration)
        }
        _ => Err(ExecutorError::FeatureNotSupportedYet {
            feature: "inline INDEXED for this GQL type",
            span,
        }),
    }
}

pub(crate) fn render_index_name(
    label: DbString,
    property: DbString,
    explicit: Option<DbString>,
) -> String {
    explicit
        .map(|name| name.as_str().to_owned())
        .unwrap_or_else(|| render_auto_index_name(label, property))
}

pub(crate) fn render_composite_index_name(
    label: DbString,
    properties: &[DbString],
    explicit: Option<DbString>,
) -> String {
    explicit
        .map(|name| name.as_str().to_owned())
        .unwrap_or_else(|| render_composite_auto_index_name(label, properties))
}

pub(super) fn render_vector_index_name(
    label: DbString,
    property: DbString,
    explicit: Option<DbString>,
) -> String {
    explicit
        .map(|name| name.as_str().to_owned())
        .unwrap_or_else(|| render_vector_auto_index_name(label, property))
}

fn render_auto_index_name(label: DbString, property: DbString) -> String {
    selene_catalog::generated_index_name(
        selene_catalog::IndexFamily::Property,
        label.as_str(),
        std::iter::once(property.as_str()),
    )
}

fn render_vector_auto_index_name(label: DbString, property: DbString) -> String {
    selene_catalog::generated_index_name(
        selene_catalog::IndexFamily::Vector,
        label.as_str(),
        std::iter::once(property.as_str()),
    )
}

fn render_composite_auto_index_name(label: DbString, properties: &[DbString]) -> String {
    selene_catalog::generated_composite_index_name(
        label.as_str(),
        properties.iter().map(DbString::as_str),
    )
}

pub(super) fn render_drop_target(target: &DropTarget) -> String {
    match target {
        DropTarget::Single { label, property } => {
            format!(":{}({})", label.as_str(), property.as_str())
        }
        DropTarget::Composite { label, properties } => format!(
            ":{}({})",
            label.as_str(),
            properties
                .iter()
                .map(|property| property.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn same_property_set(lhs: &[DbString], rhs: &[DbString]) -> bool {
    if lhs.len() != rhs.len() {
        return false;
    }
    let mut lhs = lhs.to_vec();
    let mut rhs = rhs.to_vec();
    lhs.sort_unstable();
    rhs.sort_unstable();
    lhs == rhs
}

pub(crate) fn render_index_kind(kind: TypedIndexKind) -> &'static str {
    match kind {
        TypedIndexKind::Bool => "bool",
        TypedIndexKind::I64 => "i64",
        TypedIndexKind::U64 => "u64",
        TypedIndexKind::I128 => "i128",
        TypedIndexKind::U128 => "u128",
        TypedIndexKind::Decimal => "decimal",
        TypedIndexKind::F32 => "f32",
        TypedIndexKind::F64 => "f64",
        TypedIndexKind::String => "string",
        TypedIndexKind::Date => "date",
        TypedIndexKind::LocalDateTime => "local_datetime",
        TypedIndexKind::ZonedDateTime => "zoned_datetime",
        TypedIndexKind::LocalTime => "local_time",
        TypedIndexKind::ZonedTime => "zoned_time",
        TypedIndexKind::Duration => "duration",
        TypedIndexKind::Uuid => "uuid",
    }
}

pub(super) fn render_vector_index_kind(
    kind: VectorIndexKind,
    dimension: u32,
    hnsw_config: Option<HnswIndexConfig>,
    ivf_config: Option<IvfIndexConfig>,
) -> String {
    match kind {
        VectorIndexKind::Flat => format!("vector_flat({dimension})"),
        VectorIndexKind::HnswSquaredEuclidean => {
            render_hnsw_kind("vector_hnsw_squared_euclidean", dimension, hnsw_config)
        }
        VectorIndexKind::HnswCosine => {
            render_hnsw_kind("vector_hnsw_cosine", dimension, hnsw_config)
        }
        VectorIndexKind::HnswNegativeInnerProduct => {
            render_hnsw_kind("vector_hnsw_negative_inner_product", dimension, hnsw_config)
        }
        VectorIndexKind::IvfSquaredEuclidean => {
            render_ivf_kind("vector_ivf_squared_euclidean", dimension, ivf_config)
        }
        VectorIndexKind::IvfCosine => render_ivf_kind("vector_ivf_cosine", dimension, ivf_config),
        VectorIndexKind::IvfNegativeInnerProduct => {
            render_ivf_kind("vector_ivf_negative_inner_product", dimension, ivf_config)
        }
        VectorIndexKind::TurboQuantCosine => format!("vector_turbo_quant_cosine({dimension})"),
    }
}

fn render_hnsw_kind(
    name: &'static str,
    dimension: u32,
    hnsw_config: Option<HnswIndexConfig>,
) -> String {
    let config = hnsw_config.unwrap_or_default();
    if config.is_default() {
        format!("{name}({dimension})")
    } else {
        format!(
            "{name}({dimension},m={},ef_construction={})",
            config.max_neighbors, config.ef_construction
        )
    }
}

fn render_ivf_kind(
    name: &'static str,
    dimension: u32,
    ivf_config: Option<IvfIndexConfig>,
) -> String {
    if let Some(config) = ivf_config {
        format!(
            "{name}({dimension},target_centroids={})",
            config.target_centroids
        )
    } else {
        format!("{name}({dimension})")
    }
}
