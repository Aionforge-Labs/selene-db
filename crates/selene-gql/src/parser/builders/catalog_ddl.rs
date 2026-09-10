//! Database-catalog DDL builders (ISO/IEC 39075:2024 §12.2–§12.7).
//!
//! These builders produce only the forms the database facade executes. Every
//! clause the facade cannot honor is rejected here with the owning feature ID
//! or a not-implemented diagnostic, so an unsupported clause can never be
//! dropped on the floor by a later stage.

use pest::iterators::Pair;
use selene_profile::FeatureId;

use crate::{
    ast::{
        CatalogGraphTypeDefinition, CatalogNodeTypeDefinition, DdlStatement,
        catalog_ref::{CatalogObjectReference, CatalogPathSegment, IdentifierForm},
    },
    error::ParserError,
};

use super::{Rule, db_string_pair, span, unexpected_pair, unsupported_feature};

const GRAPH_TYPE_BOUNDARY: &str = "the executable subset accepts property-free named node types with implied singleton key labels; complete closed graph types still require properties, edge types, endpoints, explicit key labels, and the other graph-type sources";

pub(super) fn build_create_schema(pair: Pair<'_, Rule>) -> Result<DdlStatement, ParserError> {
    let source_span = span(&pair);
    let mut reference = None;
    let mut if_not_exists = false;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::if_not_exists => if_not_exists = true,
            Rule::absolute_catalog_path => reference = Some(build_absolute_path(child)?),
            Rule::ddl_create_kw | Rule::ddl_schema_kw => {}
            _ => return Err(unexpected_pair(child, "unexpected CREATE SCHEMA child")),
        }
    }
    Ok(DdlStatement::CreateSchema {
        reference: reference.ok_or_else(|| {
            ParserError::syntax(
                "CREATE SCHEMA is missing its schema path",
                source_span,
                None,
            )
        })?,
        if_not_exists,
        span: source_span,
    })
}

pub(super) fn build_drop_schema(pair: Pair<'_, Rule>) -> Result<DdlStatement, ParserError> {
    let source_span = span(&pair);
    let mut reference = None;
    let mut if_exists = false;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::if_exists => if_exists = true,
            Rule::absolute_catalog_path => reference = Some(build_absolute_path(child)?),
            Rule::ddl_drop_kw | Rule::ddl_schema_kw => {}
            _ => return Err(unexpected_pair(child, "unexpected DROP SCHEMA child")),
        }
    }
    Ok(DdlStatement::DropSchema {
        reference: reference.ok_or_else(|| {
            ParserError::syntax("DROP SCHEMA is missing its schema path", source_span, None)
        })?,
        if_exists,
        span: source_span,
    })
}

/// Build `CREATE GRAPH`, admitting open and named closed graph types.
///
/// Rejections, in the order they are detected: a `<graph source>` cites GG05;
/// `LIKE` cites GG04; an inline `<nested graph type specification>` cites
/// GG03; and a statement with no type clause at all is a syntax error because
/// ISO §12.4 makes the clause mandatory.
pub(super) fn build_create_graph(pair: Pair<'_, Rule>) -> Result<DdlStatement, ParserError> {
    let source_span = span(&pair);
    let mut reference = None;
    let mut or_replace = false;
    let mut if_not_exists = false;
    let mut open_type = false;
    let mut graph_type = None;
    let mut rejection = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::or_replace => or_replace = true,
            Rule::if_not_exists => if_not_exists = true,
            Rule::graph_reference => reference = Some(build_graph_reference(child)?),
            Rule::create_graph_type_clause => {
                let clause = super::first_child(child)?;
                match clause.as_rule() {
                    Rule::open_graph_type => open_type = true,
                    Rule::graph_type_like_graph => {
                        rejection.get_or_insert(unsupported_feature(&clause, FeatureId::GG04));
                    }
                    Rule::graph_type_inline => {
                        rejection.get_or_insert(unsupported_feature(&clause, FeatureId::GG03));
                    }
                    Rule::graph_type_reference => {
                        graph_type = Some(build_graph_type_reference(clause)?);
                    }
                    _ => return Err(unexpected_pair(clause, "unexpected graph type clause")),
                }
            }
            // A graph source is the most specific diagnostic even when the
            // type clause is also missing or unsupported.
            Rule::graph_source => rejection = Some(unsupported_feature(&child, FeatureId::GG05)),
            Rule::ddl_create_kw | Rule::ddl_property_kw | Rule::ddl_graph_kw => {}
            _ => return Err(unexpected_pair(child, "unexpected CREATE GRAPH child")),
        }
    }
    if let Some(error) = rejection {
        return Err(error);
    }
    let reference = reference.ok_or_else(|| {
        ParserError::syntax(
            "CREATE GRAPH is missing its graph reference",
            source_span,
            None,
        )
    })?;
    if !open_type && graph_type.is_none() {
        return Err(ParserError::syntax(
            "CREATE GRAPH requires an <open graph type> or <of graph type> clause (ISO/IEC 39075:2024 section 12.4)",
            source_span,
            Some("write `CREATE GRAPH name ANY` to create a graph with an open graph type".into()),
        ));
    }
    Ok(DdlStatement::CreateGraph {
        reference,
        or_replace,
        if_not_exists,
        graph_type,
        span: source_span,
    })
}

pub(super) fn build_drop_graph(pair: Pair<'_, Rule>) -> Result<DdlStatement, ParserError> {
    let source_span = span(&pair);
    let mut reference = None;
    let mut if_exists = false;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::if_exists => if_exists = true,
            Rule::graph_reference => reference = Some(build_graph_reference(child)?),
            Rule::ddl_drop_kw | Rule::ddl_property_kw | Rule::ddl_graph_kw => {}
            _ => return Err(unexpected_pair(child, "unexpected DROP GRAPH child")),
        }
    }
    Ok(DdlStatement::DropGraph {
        reference: reference.ok_or_else(|| {
            ParserError::syntax(
                "DROP GRAPH is missing its graph reference",
                source_span,
                None,
            )
        })?,
        if_exists,
        span: source_span,
    })
}

pub(super) fn build_create_graph_type(pair: Pair<'_, Rule>) -> Result<DdlStatement, ParserError> {
    let source_span = span(&pair);
    let mut reference = None;
    let mut definition = None;
    let mut or_replace = false;
    let mut if_not_exists = false;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::or_replace => or_replace = true,
            Rule::if_not_exists => if_not_exists = true,
            Rule::graph_reference => reference = Some(build_graph_reference(child)?),
            Rule::graph_type_source => {
                let source = super::first_child(child)?;
                match source.as_rule() {
                    Rule::graph_type_copy_source => {
                        return Err(unsupported_graph_type_source(
                            &source,
                            "COPY OF graph-type sources",
                        ));
                    }
                    Rule::graph_type_like_graph => {
                        return Err(unsupported_feature(&source, FeatureId::GG04));
                    }
                    Rule::graph_type_nested_source => {
                        let nested = source
                            .into_inner()
                            .find(|part| {
                                part.as_rule() == Rule::nested_graph_type_specification
                            })
                            .ok_or_else(|| {
                                ParserError::syntax(
                                    "CREATE GRAPH TYPE is missing its nested graph-type specification",
                                    source_span,
                                    None,
                                )
                            })?;
                        definition = Some(build_graph_type_definition(nested)?);
                    }
                    _ => return Err(unexpected_pair(source, "unexpected graph-type source")),
                }
            }
            Rule::ddl_create_kw
            | Rule::ddl_property_kw
            | Rule::ddl_graph_kw
            | Rule::ddl_type_kw => {}
            _ => return Err(unexpected_pair(child, "unexpected CREATE GRAPH TYPE child")),
        }
    }
    Ok(DdlStatement::CreateGraphType {
        reference: reference.ok_or_else(|| {
            ParserError::syntax(
                "CREATE GRAPH TYPE is missing its graph-type reference",
                source_span,
                None,
            )
        })?,
        definition: definition.ok_or_else(|| {
            ParserError::syntax(
                "CREATE GRAPH TYPE is missing its graph-type source",
                source_span,
                None,
            )
        })?,
        or_replace,
        if_not_exists,
        span: source_span,
    })
}

pub(super) fn build_drop_graph_type(pair: Pair<'_, Rule>) -> Result<DdlStatement, ParserError> {
    let source_span = span(&pair);
    let mut reference = None;
    let mut if_exists = false;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::if_exists => if_exists = true,
            Rule::graph_reference => reference = Some(build_graph_reference(child)?),
            Rule::ddl_drop_kw | Rule::ddl_property_kw | Rule::ddl_graph_kw | Rule::ddl_type_kw => {}
            _ => return Err(unexpected_pair(child, "unexpected DROP GRAPH TYPE child")),
        }
    }
    Ok(DdlStatement::DropGraphType {
        reference: reference.ok_or_else(|| {
            ParserError::syntax(
                "DROP GRAPH TYPE is missing its graph-type reference",
                source_span,
                None,
            )
        })?,
        if_exists,
        span: source_span,
    })
}

/// Reject a `NEXT`-composed `<linear catalog-modifying statement>` (§12.1).
pub(super) fn reject_catalog_statement_chain(pair: &Pair<'_, Rule>) -> ParserError {
    ParserError::not_implemented(
        "NEXT-composed catalog-modifying statements (ISO/IEC 39075:2024 section 12.1) are not implemented",
        span(pair),
        Some("execute each CREATE/DROP SCHEMA, GRAPH, or GRAPH TYPE statement as its own request"),
    )
}

pub(super) fn build_absolute_path(
    pair: Pair<'_, Rule>,
) -> Result<CatalogObjectReference, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::absolute_catalog_path);
    let source_span = span(&pair);
    let segments = pair
        .into_inner()
        .filter(|child| child.as_rule() == Rule::ident)
        .map(catalog_segment)
        .collect::<Result<Vec<_>, _>>()?;
    if segments.is_empty() {
        return Err(ParserError::syntax(
            "catalog path has no segments",
            source_span,
            None,
        ));
    }
    Ok(CatalogObjectReference {
        absolute: true,
        segments,
        span: source_span,
    })
}

pub(super) fn build_graph_reference(
    pair: Pair<'_, Rule>,
) -> Result<CatalogObjectReference, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::graph_reference);
    let source_span = span(&pair);
    let inner = super::first_child(pair)?;
    match inner.as_rule() {
        Rule::absolute_catalog_path => build_absolute_path(inner),
        Rule::ident => Ok(CatalogObjectReference {
            absolute: false,
            segments: vec![catalog_segment(inner)?],
            span: source_span,
        }),
        _ => Err(unexpected_pair(inner, "unexpected graph reference child")),
    }
}

fn build_graph_type_reference(pair: Pair<'_, Rule>) -> Result<CatalogObjectReference, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::graph_type_reference);
    let source_span = span(&pair);
    let reference = pair
        .into_inner()
        .find(|child| child.as_rule() == Rule::graph_reference)
        .ok_or_else(|| {
            ParserError::syntax(
                "named graph type clause is missing its reference",
                source_span,
                None,
            )
        })?;
    build_graph_reference(reference)
}

fn build_graph_type_definition(
    pair: Pair<'_, Rule>,
) -> Result<CatalogGraphTypeDefinition, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::nested_graph_type_specification);
    let source_span = span(&pair);
    let mut node_types = Vec::new();
    for element in pair.into_inner() {
        debug_assert_eq!(element.as_rule(), Rule::graph_type_element_specification);
        let specification = super::first_child(element)?;
        match specification.as_rule() {
            Rule::graph_type_node_specification => {
                node_types.push(build_graph_type_node(specification)?);
            }
            Rule::graph_type_edge_specification => {
                return Err(ParserError::not_implemented(
                    "edge types and endpoint declarations require complete closed graph-type support (Feature GG02)",
                    span(&specification),
                    Some(GRAPH_TYPE_BOUNDARY),
                ));
            }
            _ => {
                return Err(unexpected_pair(
                    specification,
                    "unexpected graph-type element specification",
                ));
            }
        }
    }
    Ok(CatalogGraphTypeDefinition {
        node_types,
        span: source_span,
    })
}

fn build_graph_type_node(pair: Pair<'_, Rule>) -> Result<CatalogNodeTypeDefinition, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::graph_type_node_specification);
    let source_span = content_span(&pair);
    if let Some(explicit) = descendant(&pair, Rule::graph_type_node_key_label_set) {
        return Err(unsupported_feature(&explicit, FeatureId::GG21));
    }
    if let Some(properties) = descendant(&pair, Rule::graph_type_property_types) {
        return Err(ParserError::not_implemented(
            "node property types require complete closed graph-type support (Feature GG02)",
            span(&properties),
            Some(GRAPH_TYPE_BOUNDARY),
        ));
    }
    if let Some(labels) = descendant(&pair, Rule::graph_type_label_set_phrase) {
        return Err(ParserError::not_implemented(
            "explicit or additional node labels are outside the implied singleton-label subset (Feature GG02)",
            span(&labels),
            Some(GRAPH_TYPE_BOUNDARY),
        ));
    }
    if let Some(alias) = descendant(&pair, Rule::graph_type_local_alias) {
        return Err(ParserError::not_implemented(
            "local node-type aliases require complete closed graph-type support (Feature GG02)",
            span(&alias),
            Some(GRAPH_TYPE_BOUNDARY),
        ));
    }

    let specification = super::first_child(pair)?;
    let name = match specification.as_rule() {
        Rule::graph_type_node_pattern => specification
            .clone()
            .into_inner()
            .find(|child| child.as_rule() == Rule::ident),
        Rule::graph_type_node_phrase => specification
            .clone()
            .into_inner()
            .find(|child| child.as_rule() == Rule::graph_type_node_phrase_filler)
            .and_then(|filler| {
                filler
                    .into_inner()
                    .find(|child| child.as_rule() == Rule::ident)
            }),
        _ => {
            return Err(unexpected_pair(
                specification,
                "unexpected node-type specification",
            ));
        }
    }
    .ok_or_else(|| {
        ParserError::not_implemented(
            "anonymous node types require complete closed graph-type support (Feature GG02)",
            source_span,
            Some(GRAPH_TYPE_BOUNDARY),
        )
    })?;

    Ok(CatalogNodeTypeDefinition {
        name: catalog_segment(name)?,
        span: source_span,
    })
}

fn descendant<'i>(pair: &Pair<'i, Rule>, wanted: Rule) -> Option<Pair<'i, Rule>> {
    for child in pair.clone().into_inner() {
        if child.as_rule() == wanted {
            return Some(child);
        }
        if let Some(found) = descendant(&child, wanted) {
            return Some(found);
        }
    }
    None
}

fn content_span(pair: &Pair<'_, Rule>) -> crate::SourceSpan {
    let pair_span = span(pair);
    let byte_len = u32::try_from(pair.as_str().trim_end().len()).unwrap_or(pair_span.byte_len);
    crate::SourceSpan::new(pair_span.byte_offset, byte_len)
}

fn unsupported_graph_type_source(pair: &Pair<'_, Rule>, form: &str) -> ParserError {
    ParserError::not_implemented(
        format!("{form} require complete graph-type source support (Feature GG02)"),
        span(pair),
        Some(GRAPH_TYPE_BOUNDARY),
    )
}

/// Decode one `ident` while keeping its lexical form.
///
/// [`db_string_pair`] erases the form, which is fine for labels and property
/// keys but not for catalog paths: the catalog validates regular and delimited
/// names under different rules, and a delimited segment may legitimately spell
/// `/`, spaces, or backticks.
pub(super) fn catalog_segment(pair: Pair<'_, Rule>) -> Result<CatalogPathSegment, ParserError> {
    debug_assert_eq!(pair.as_rule(), Rule::ident);
    let form = if pair.as_str().starts_with(['"', '`']) {
        IdentifierForm::Delimited
    } else {
        IdentifierForm::Regular
    };
    Ok(CatalogPathSegment {
        name: db_string_pair(pair)?,
        form,
    })
}
