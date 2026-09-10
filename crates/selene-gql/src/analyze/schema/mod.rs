//! Closed-graph static schema validation for analyzed mutation statements.

mod labels;
mod properties;

use selene_core::DbString;
use selene_graph::{EdgeEndpointDef, EdgeTypeDef, GraphTypeDef, NodeTypeDef};

use self::{
    labels::{
        candidate_node_types, endpoint_indices, endpoint_type, exact_node_type, fresh_binding,
        insert_edge_label, insert_label_set, is_fresh_edge, is_fresh_node, node_at,
        static_label_set,
    },
    properties::{
        PropertyAgreement, RequiredPropertyCheck, property_agreement, set_value_index,
        undeclared_property, validate_declared_property, validate_node_label_transition,
        validate_one_property_value, validate_property_values, validate_required_properties,
    },
};
use std::collections::HashMap;

use crate::{
    EdgeDirection, EdgePattern, GraphPattern, LabelExpr, MutationPipeline, MutationStatement,
    NodePattern, PatternElement, SourceSpan, ValueExpr,
    analyze::{
        ast::AnalyzedStatement,
        binding::{BindingDecl, BindingDeclKind, BindingId},
        error::AnalysisError,
        write_set::{ElementKind, WriteKind, WriteSetEntry},
    },
};

pub(crate) fn validate(
    analyzed: &AnalyzedStatement,
    graph_type: &GraphTypeDef,
) -> Result<(), AnalysisError> {
    let crate::Statement::Mutate(pipeline) = analyzed.source() else {
        return Ok(());
    };
    validate_inserts(pipeline, analyzed, graph_type)?;
    if let Some(write_set) = analyzed.write_set.as_ref() {
        // Index SET values by span once: each SetProperty entry resolves its
        // value in O(1) instead of rescanning every SET item (was O(W×S)).
        let set_values = set_value_index(analyzed);
        for entry in &write_set.entries {
            if !matches!(
                entry.kind,
                WriteKind::InsertNode { .. } | WriteKind::InsertEdge { .. }
            ) {
                validate_non_insert_entry(entry, analyzed, graph_type, &set_values)?;
            }
        }
    }
    Ok(())
}

fn validate_inserts(
    pipeline: &MutationPipeline,
    analyzed: &AnalyzedStatement,
    graph_type: &GraphTypeDef,
) -> Result<(), AnalysisError> {
    for (stmt_index, statement) in pipeline.statements.iter().enumerate() {
        let MutationStatement::Insert(insert) = statement else {
            continue;
        };
        for pattern in &insert.patterns {
            for (index, element) in pattern.elements.iter().enumerate() {
                match element {
                    PatternElement::Node(node) if is_fresh_node(node, analyzed) => {
                        validate_insert_node(node, stmt_index, analyzed, graph_type)?;
                    }
                    PatternElement::Edge(edge) if is_fresh_edge(edge, analyzed) => {
                        validate_insert_edge(
                            edge, pattern, index, stmt_index, analyzed, graph_type,
                        )?;
                    }
                    PatternElement::Node(_) | PatternElement::Edge(_) => {}
                }
            }
        }
    }
    Ok(())
}

fn validate_insert_node(
    node: &NodePattern,
    stmt_index: usize,
    analyzed: &AnalyzedStatement,
    graph_type: &GraphTypeDef,
) -> Result<(), AnalysisError> {
    let labels = insert_label_set(node.label_expr.as_ref(), node.span)?;
    let Some(node_type) = graph_type.find_node_type(&labels) else {
        return Err(AnalysisError::SchemaUnknownNodeType {
            labels,
            graph_type: graph_type.name.clone(),
            span: node.span,
        });
    };
    let binding = fresh_binding(
        node.binding.clone(),
        node.span,
        BindingDeclKind::InsertNode,
        analyzed,
    );
    validate_property_values(
        graph_type,
        node_type.name.clone(),
        &node_type.properties,
        &node.properties,
        analyzed,
    )?;
    validate_required_properties(RequiredPropertyCheck {
        declared_in: node_type.name.clone(),
        declarations: &node_type.properties,
        properties: &node.properties,
        stmt_index,
        binding: binding.flatten(),
        span: node.span,
        analyzed,
    })
}

fn validate_insert_edge(
    edge: &EdgePattern,
    pattern: &GraphPattern,
    index: usize,
    stmt_index: usize,
    analyzed: &AnalyzedStatement,
    graph_type: &GraphTypeDef,
) -> Result<(), AnalysisError> {
    let label = insert_edge_label(edge)?;
    if graph_type
        .first_edge_type_with_label(label.clone())
        .is_none()
    {
        return Err(AnalysisError::SchemaUnknownEdgeType {
            label,
            graph_type: graph_type.name.clone(),
            span: edge.span,
        });
    }

    // Intrinsic undirected INSERT accepts either endpoint declaration order;
    // canonical storage ordering must not select its property schema.
    let endpoint_direction = if edge.direction == EdgeDirection::Undirected {
        EdgeDirection::Right
    } else {
        edge.direction
    };
    let Some((source_index, target_index)) = endpoint_indices(pattern, index, endpoint_direction)
    else {
        return Ok(());
    };
    let Some(source) = node_at(pattern, source_index) else {
        return Ok(());
    };
    let Some(target) = node_at(pattern, target_index) else {
        return Ok(());
    };
    let Some((source_type, source_labels)) = endpoint_type(source, analyzed, graph_type)? else {
        return Ok(());
    };
    let Some((target_type, target_labels)) = endpoint_type(target, analyzed, graph_type)? else {
        return Ok(());
    };

    let Some(edge_type) = graph_type.find_mixed_edge_type(
        label.clone(),
        source_type,
        target_type,
        if edge.direction == EdgeDirection::Undirected {
            selene_core::EdgeDirectionality::Undirected
        } else {
            selene_core::EdgeDirectionality::Directed
        },
    ) else {
        let expected = graph_type
            .first_edge_type_with_label(label.clone())
            .expect("edge label existence was checked before endpoint validation");
        return Err(AnalysisError::SchemaEdgeEndpointMismatch {
            label,
            expected_source: endpoint_name(graph_type, &expected.source_node_type),
            expected_target: endpoint_name(graph_type, &expected.target_node_type),
            observed_source: Box::new(source_labels),
            observed_target: Box::new(target_labels),
            span: edge.span,
        });
    };
    validate_insert_edge_properties(edge, edge_type, graph_type, stmt_index, analyzed)
}

fn endpoint_name(graph_type: &GraphTypeDef, endpoint: &EdgeEndpointDef) -> String {
    match endpoint {
        EdgeEndpointDef::Any => "Any".to_owned(),
        EdgeEndpointDef::NodeType(index) => graph_type.node_types[*index as usize]
            .name
            .as_str()
            .to_owned(),
        EdgeEndpointDef::OneOf(indices) => indices
            .iter()
            .map(|index| {
                graph_type.node_types[*index as usize]
                    .name
                    .as_str()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn validate_insert_edge_properties(
    edge: &EdgePattern,
    edge_type: &EdgeTypeDef,
    graph_type: &GraphTypeDef,
    stmt_index: usize,
    analyzed: &AnalyzedStatement,
) -> Result<(), AnalysisError> {
    let binding = fresh_binding(
        edge.binding.clone(),
        edge.span,
        BindingDeclKind::InsertEdge,
        analyzed,
    );
    validate_property_values(
        graph_type,
        edge_type.name.clone(),
        &edge_type.properties,
        &edge.properties,
        analyzed,
    )?;
    validate_required_properties(RequiredPropertyCheck {
        declared_in: edge_type.name.clone(),
        declarations: &edge_type.properties,
        properties: &edge.properties,
        stmt_index,
        binding: binding.flatten(),
        span: edge.span,
        analyzed,
    })
}

fn validate_non_insert_entry(
    entry: &WriteSetEntry,
    analyzed: &AnalyzedStatement,
    graph_type: &GraphTypeDef,
    set_values: &HashMap<SourceSpan, &ValueExpr>,
) -> Result<(), AnalysisError> {
    match &entry.kind {
        WriteKind::SetProperty {
            target,
            element,
            key,
            value_span,
        } => validate_set_property(
            *target,
            *element,
            key.clone(),
            *value_span,
            entry.span,
            analyzed,
            graph_type,
            set_values,
        ),
        WriteKind::SetLabel {
            target,
            element,
            label,
        } => validate_set_label(
            *target,
            *element,
            label.clone(),
            entry.span,
            analyzed,
            graph_type,
        ),
        WriteKind::RemoveProperty {
            target,
            element,
            key,
        } => validate_remove_property(
            *target,
            *element,
            key.clone(),
            entry.span,
            analyzed,
            graph_type,
        ),
        WriteKind::RemoveLabel {
            target,
            element,
            label,
        } => validate_remove_label(
            *target,
            *element,
            label.clone(),
            entry.span,
            analyzed,
            graph_type,
        ),
        WriteKind::DeleteTarget { .. }
        | WriteKind::InsertNode { .. }
        | WriteKind::InsertEdge { .. } => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_set_property(
    target: BindingId,
    element: ElementKind,
    key: DbString,
    value_span: SourceSpan,
    span: SourceSpan,
    analyzed: &AnalyzedStatement,
    graph_type: &GraphTypeDef,
    set_values: &HashMap<SourceSpan, &ValueExpr>,
) -> Result<(), AnalysisError> {
    let Some(value) = set_values.get(&value_span).copied() else {
        return Ok(());
    };
    match target_declaration(analyzed, target, element, graph_type)? {
        Some(TargetDeclaration::Node(types)) => match property_agreement(&types, key.clone()) {
            PropertyAgreement::Declared(decl, declared_in) => {
                validate_one_property_value(declared_in, decl, key, value, analyzed)
            }
            PropertyAgreement::Undeclared(declared_in) => {
                undeclared_property(key, declared_in, graph_type.name.clone(), span)
            }
            PropertyAgreement::Disagree => Ok(()),
        },
        Some(TargetDeclaration::Edge(edge_type)) => {
            validate_declared_property(edge_type, key.clone(), graph_type.name.clone(), span)
                .and_then(|decl| {
                    validate_one_property_value(edge_type.name.clone(), decl, key, value, analyzed)
                })
        }
        None => Ok(()),
    }
}

fn validate_remove_property(
    target: BindingId,
    element: ElementKind,
    key: DbString,
    span: SourceSpan,
    analyzed: &AnalyzedStatement,
    graph_type: &GraphTypeDef,
) -> Result<(), AnalysisError> {
    match target_declaration(analyzed, target, element, graph_type)? {
        Some(TargetDeclaration::Node(types)) => match property_agreement(&types, key.clone()) {
            PropertyAgreement::Declared(decl, declared_in) if decl.required => {
                Err(AnalysisError::SchemaRequiredPropertyRemoved {
                    property: key,
                    declared_in,
                    span,
                })
            }
            PropertyAgreement::Declared(..) | PropertyAgreement::Disagree => Ok(()),
            PropertyAgreement::Undeclared(declared_in) => {
                undeclared_property(key, declared_in, graph_type.name.clone(), span)
            }
        },
        Some(TargetDeclaration::Edge(edge_type)) => {
            let decl =
                validate_declared_property(edge_type, key.clone(), graph_type.name.clone(), span)?;
            if decl.required {
                Err(AnalysisError::SchemaRequiredPropertyRemoved {
                    property: key,
                    declared_in: edge_type.name.clone(),
                    span,
                })
            } else {
                Ok(())
            }
        }
        None => Ok(()),
    }
}

fn validate_set_label(
    target: BindingId,
    element: ElementKind,
    label: DbString,
    span: SourceSpan,
    analyzed: &AnalyzedStatement,
    graph_type: &GraphTypeDef,
) -> Result<(), AnalysisError> {
    match target_declaration(analyzed, target, element, graph_type)? {
        Some(TargetDeclaration::Node(types)) => {
            validate_node_label_transition(types, graph_type, span, |labels| {
                labels.insert(label.clone());
            })
        }
        Some(TargetDeclaration::Edge(_)) => {
            if graph_type
                .first_edge_type_with_label(label.clone())
                .is_none()
            {
                Err(AnalysisError::SchemaUnknownEdgeType {
                    label,
                    graph_type: graph_type.name.clone(),
                    span,
                })
            } else {
                Ok(())
            }
        }
        None => Ok(()),
    }
}

fn validate_remove_label(
    target: BindingId,
    element: ElementKind,
    label: DbString,
    span: SourceSpan,
    analyzed: &AnalyzedStatement,
    graph_type: &GraphTypeDef,
) -> Result<(), AnalysisError> {
    match target_declaration(analyzed, target, element, graph_type)? {
        Some(TargetDeclaration::Node(types)) => {
            validate_node_label_transition(types, graph_type, span, |labels| {
                labels.remove(&label);
            })
        }
        Some(TargetDeclaration::Edge(edge_type)) if label == edge_type.label => {
            Err(AnalysisError::SchemaRequiredEdgeLabelRemoved {
                label,
                declared_in: edge_type.name.clone(),
                span,
            })
        }
        Some(TargetDeclaration::Edge(_)) | None => Ok(()),
    }
}

enum TargetDeclaration<'a> {
    Node(Vec<&'a NodeTypeDef>),
    Edge(&'a EdgeTypeDef),
}

fn target_declaration<'a>(
    analyzed: &'a AnalyzedStatement,
    target: BindingId,
    element: ElementKind,
    graph_type: &'a GraphTypeDef,
) -> Result<Option<TargetDeclaration<'a>>, AnalysisError> {
    let Some(decl) = analyzed.scopes.declaration(target) else {
        return Ok(None);
    };
    match element {
        ElementKind::Node => node_types_for_decl(decl, graph_type).map(|types| {
            types.and_then(|types| (!types.is_empty()).then_some(TargetDeclaration::Node(types)))
        }),
        ElementKind::Edge => edge_type_for_decl(decl, graph_type)
            .map(|edge_type| edge_type.map(TargetDeclaration::Edge)),
        ElementKind::Path | ElementKind::Alias => Ok(None),
    }
}

fn node_types_for_decl<'a>(
    decl: &BindingDecl,
    graph_type: &'a GraphTypeDef,
) -> Result<Option<Vec<&'a NodeTypeDef>>, AnalysisError> {
    let Some(expr) = decl.label_expr() else {
        return Ok(None);
    };
    let Ok(labels) = static_label_set(expr) else {
        return Ok(None);
    };
    match decl.kind() {
        BindingDeclKind::InsertNode => {
            let Some((_, node_type)) = exact_node_type(graph_type, &labels) else {
                return Err(AnalysisError::SchemaUnknownNodeType {
                    labels,
                    graph_type: graph_type.name.clone(),
                    span: decl.span(),
                });
            };
            Ok(Some(vec![node_type]))
        }
        BindingDeclKind::NodePattern => {
            let candidates = candidate_node_types(graph_type, &labels);
            if candidates.is_empty() {
                Err(AnalysisError::SchemaUnknownNodeType {
                    labels,
                    graph_type: graph_type.name.clone(),
                    span: decl.span(),
                })
            } else {
                Ok(Some(candidates))
            }
        }
        _ => Ok(None),
    }
}

fn edge_type_for_decl<'a>(
    decl: &BindingDecl,
    graph_type: &'a GraphTypeDef,
) -> Result<Option<&'a EdgeTypeDef>, AnalysisError> {
    let Some(LabelExpr::Single(label)) = decl.label_expr() else {
        return Ok(None);
    };
    let mut matches = graph_type
        .edge_types
        .iter()
        .filter(|edge_type| edge_type.label == *label);
    let Some(first) = matches.next() else {
        return Err(AnalysisError::SchemaUnknownEdgeType {
            label: label.clone(),
            graph_type: graph_type.name.clone(),
            span: decl.span(),
        });
    };
    Ok(matches.next().is_none().then_some(first))
}
