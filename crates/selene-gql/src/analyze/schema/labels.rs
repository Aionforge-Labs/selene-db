use selene_core::{DbString, LabelSet};
use selene_graph::{GraphTypeDef, NodeTypeDef};

use crate::{
    EdgeDirection, EdgePattern, GraphPattern, LabelExpr, NodePattern, PatternElement, SourceSpan,
    analyze::{
        ast::AnalyzedStatement,
        binding::{BindingDeclKind, BindingId, BindingUseKind},
        error::{AnalysisError, InvalidLabelForm},
    },
};

pub(super) fn static_label_set(expr: &LabelExpr) -> Result<LabelSet, InvalidLabelForm> {
    match expr {
        LabelExpr::Single(label) => Ok(LabelSet::single(label.clone())),
        LabelExpr::Conjunction(parts) => {
            let mut set = LabelSet::new();
            for part in parts {
                let LabelExpr::Single(label) = part else {
                    return Err(match part {
                        LabelExpr::Disjunction(_) => InvalidLabelForm::Disjunction,
                        LabelExpr::Negation(_) => InvalidLabelForm::Negation,
                        LabelExpr::Wildcard => InvalidLabelForm::Wildcard,
                        LabelExpr::Conjunction(_) | LabelExpr::Single(_) => {
                            InvalidLabelForm::Disjunction
                        }
                    });
                };
                set.insert(label.clone());
            }
            Ok(set)
        }
        LabelExpr::Disjunction(_) => Err(InvalidLabelForm::Disjunction),
        LabelExpr::Negation(_) => Err(InvalidLabelForm::Negation),
        LabelExpr::Wildcard => Err(InvalidLabelForm::Wildcard),
    }
}

pub(super) fn insert_label_set(
    expr: Option<&LabelExpr>,
    span: SourceSpan,
) -> Result<LabelSet, AnalysisError> {
    let Some(expr) = expr else {
        return Err(AnalysisError::SchemaInvalidInsertLabelExpr {
            form: InvalidLabelForm::Missing,
            span,
        });
    };
    static_label_set(expr)
        .map_err(|form| AnalysisError::SchemaInvalidInsertLabelExpr { form, span })
}

pub(super) fn insert_edge_label(edge: &EdgePattern) -> Result<DbString, AnalysisError> {
    let Some(expr) = edge.label_expr.as_ref() else {
        return Err(AnalysisError::SchemaInvalidInsertLabelExpr {
            form: InvalidLabelForm::Missing,
            span: edge.span,
        });
    };
    match expr {
        LabelExpr::Single(label) => Ok(label.clone()),
        LabelExpr::Negation(_) => Err(AnalysisError::SchemaInvalidInsertLabelExpr {
            form: InvalidLabelForm::Negation,
            span: edge.span,
        }),
        LabelExpr::Wildcard => Err(AnalysisError::SchemaInvalidInsertLabelExpr {
            form: InvalidLabelForm::Wildcard,
            span: edge.span,
        }),
        LabelExpr::Conjunction(_) | LabelExpr::Disjunction(_) => {
            Err(AnalysisError::SchemaInvalidInsertLabelExpr {
                form: InvalidLabelForm::Disjunction,
                span: edge.span,
            })
        }
    }
}

pub(super) fn exact_node_type<'a>(
    graph_type: &'a GraphTypeDef,
    labels: &LabelSet,
) -> Option<(u32, &'a NodeTypeDef)> {
    graph_type
        .find_node_type_index(labels)
        .map(|index| (index, &graph_type.node_types[index as usize]))
}

pub(super) fn candidate_node_types<'a>(
    graph_type: &'a GraphTypeDef,
    predicate: &LabelSet,
) -> Vec<&'a NodeTypeDef> {
    graph_type
        .node_types
        .iter()
        .filter(|node_type| {
            predicate
                .iter()
                .all(|label| node_type.key_labels.contains(label))
        })
        .collect()
}

pub(super) fn endpoint_type(
    node: &NodePattern,
    analyzed: &AnalyzedStatement,
    graph_type: &GraphTypeDef,
) -> Result<Option<(u32, LabelSet)>, AnalysisError> {
    if is_fresh_node(node, analyzed) {
        let labels = match insert_label_set(node.label_expr.as_ref(), node.span) {
            Ok(labels) => labels,
            Err(_) => return Ok(None),
        };
        return Ok(exact_node_type(graph_type, &labels).map(|(index, _)| (index, labels)));
    }
    let Some(binding) = reused_binding(node.binding.clone(), node.span, analyzed) else {
        return Ok(None);
    };
    let Some(decl) = analyzed.scopes.declaration(binding) else {
        return Ok(None);
    };
    let Some(expr) = decl.label_expr() else {
        return Ok(None);
    };
    let Ok(labels) = static_label_set(expr) else {
        return Ok(None);
    };
    if decl.kind() == BindingDeclKind::InsertNode {
        return match exact_node_type(graph_type, &labels) {
            Some((index, _)) => Ok(Some((index, labels))),
            None => Err(AnalysisError::SchemaUnknownNodeType {
                labels,
                graph_type: graph_type.name.clone(),
                span: decl.span(),
            }),
        };
    }
    let candidates = candidate_node_types(graph_type, &labels);
    if candidates.is_empty() {
        return Err(AnalysisError::SchemaUnknownNodeType {
            labels,
            graph_type: graph_type.name.clone(),
            span: decl.span(),
        });
    }
    if candidates.len() == 1 {
        let index = graph_type
            .find_node_type_index(&candidates[0].key_labels)
            .expect("candidate node type has an index");
        Ok(Some((index, candidates[0].key_labels.clone())))
    } else {
        Ok(None)
    }
}

pub(super) fn endpoint_indices(
    pattern: &GraphPattern,
    index: usize,
    direction: EdgeDirection,
) -> Option<(usize, usize)> {
    match direction {
        EdgeDirection::Right => index.checked_sub(1).map(|source| (source, index + 1)),
        EdgeDirection::Left => index.checked_sub(1).map(|target| (index + 1, target)),
        EdgeDirection::Undirected
        | EdgeDirection::Any
        | EdgeDirection::LeftOrRight
        | EdgeDirection::LeftOrUndirected
        | EdgeDirection::UndirectedOrRight => None,
    }
    .filter(|(_, target)| *target < pattern.elements.len())
}

pub(super) fn node_at(pattern: &GraphPattern, index: usize) -> Option<&NodePattern> {
    match pattern.elements.get(index)? {
        PatternElement::Node(node) => Some(node),
        PatternElement::Edge(_) => None,
    }
}

pub(super) fn is_fresh_node(node: &NodePattern, analyzed: &AnalyzedStatement) -> bool {
    node.binding.is_none()
        || fresh_binding(
            node.binding.clone(),
            node.span,
            BindingDeclKind::InsertNode,
            analyzed,
        )
        .is_some_and(|binding| binding.is_some())
}

pub(super) fn is_fresh_edge(edge: &EdgePattern, analyzed: &AnalyzedStatement) -> bool {
    edge.binding.is_none()
        || fresh_binding(
            edge.binding.clone(),
            edge.span,
            BindingDeclKind::InsertEdge,
            analyzed,
        )
        .is_some_and(|binding| binding.is_some())
}

pub(super) fn fresh_binding(
    name: Option<DbString>,
    span: SourceSpan,
    kind: BindingDeclKind,
    analyzed: &AnalyzedStatement,
) -> Option<Option<BindingId>> {
    if name.is_none() {
        return Some(None);
    }
    let write_set = analyzed.write_set.as_ref()?;
    write_set.entries.iter().find_map(|entry| {
        if entry.span != span {
            return None;
        }
        match (kind, &entry.kind) {
            (
                BindingDeclKind::InsertNode,
                crate::analyze::WriteKind::InsertNode { binding, .. },
            )
            | (
                BindingDeclKind::InsertEdge,
                crate::analyze::WriteKind::InsertEdge { binding, .. },
            ) => Some(*binding),
            _ => None,
        }
    })
}

pub(super) fn reused_binding(
    name: Option<DbString>,
    span: SourceSpan,
    analyzed: &AnalyzedStatement,
) -> Option<BindingId> {
    let name = name?;
    analyzed
        .references
        .iter()
        .find(|reference| {
            reference.name == name
                && reference.span == span
                && reference.kind == BindingUseKind::PatternReuse
        })
        .map(|reference| reference.binding)
}
