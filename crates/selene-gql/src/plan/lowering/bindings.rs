//! MATCH lowering binding resolution.

use std::collections::BTreeSet;

use selene_core::DbString;

use crate::{
    EdgePattern, NodePattern, SourceSpan,
    analyze::{AnalyzedStatement, BindingDecl, BindingDeclKind, BindingId, BindingUseKind},
    plan::{BindingDef, BindingElement, HiddenBindingId, PlannerError},
};

#[derive(Default)]
pub(super) struct HiddenAllocator {
    next: u32,
}

impl HiddenAllocator {
    pub(super) fn next(&mut self) -> HiddenBindingId {
        let id = HiddenBindingId::new(self.next);
        self.next += 1;
        id
    }
}

pub(super) fn node_binding(
    node: &NodePattern,
    analyzed: &AnalyzedStatement,
    names: &mut BTreeSet<DbString>,
    binding_ids: &mut BTreeSet<BindingId>,
) -> Result<Option<BindingId>, PlannerError> {
    node.binding
        .clone()
        .map(|name| {
            names.insert(name.clone());
            let binding =
                binding_for_pattern(name, node.span, BindingDeclKind::NodePattern, analyzed)?;
            binding_ids.insert(binding);
            Ok(binding)
        })
        .transpose()
}

pub(super) fn edge_binding(
    edge: &EdgePattern,
    analyzed: &AnalyzedStatement,
    names: &mut BTreeSet<DbString>,
    binding_ids: &mut BTreeSet<BindingId>,
) -> Result<Option<BindingId>, PlannerError> {
    edge.binding
        .clone()
        .map(|name| {
            names.insert(name.clone());
            let binding =
                binding_for_pattern(name, edge.span, BindingDeclKind::EdgePattern, analyzed)?;
            binding_ids.insert(binding);
            Ok(binding)
        })
        .transpose()
}

fn binding_for_pattern(
    name: DbString,
    span: SourceSpan,
    expected: BindingDeclKind,
    analyzed: &AnalyzedStatement,
) -> Result<BindingId, PlannerError> {
    if let Some(binding) = analyzed
        .scopes
        .declarations()
        .iter()
        .find(|decl| {
            decl.name() == name && decl.span() == span && same_element(decl.kind(), expected)
        })
        .map(BindingDecl::id)
    {
        return Ok(binding);
    }
    analyzed
        .references
        .iter()
        .find(|reference| {
            reference.name == name
                && reference.span == span
                && reference.kind == BindingUseKind::PatternReuse
        })
        .map(|reference| reference.binding)
        .ok_or(PlannerError::BindingResolutionLost {
            binding: BindingId::new(u32::MAX),
            span,
        })
}

fn same_element(found: BindingDeclKind, expected: BindingDeclKind) -> bool {
    matches!(
        (found, expected),
        (
            BindingDeclKind::NodePattern | BindingDeclKind::InsertNode,
            BindingDeclKind::NodePattern
        ) | (
            BindingDeclKind::EdgePattern | BindingDeclKind::InsertEdge,
            BindingDeclKind::EdgePattern
        ) | (BindingDeclKind::PathBinding, BindingDeclKind::PathBinding)
    )
}

pub(super) fn binding_defs(
    analyzed: &AnalyzedStatement,
    binding_ids: &BTreeSet<BindingId>,
) -> Vec<BindingDef> {
    analyzed
        .scopes
        .declarations()
        .iter()
        .filter(|decl| binding_ids.contains(&decl.id()))
        .filter_map(|decl| {
            let element = match decl.kind() {
                BindingDeclKind::NodePattern | BindingDeclKind::InsertNode => BindingElement::Node,
                BindingDeclKind::EdgePattern | BindingDeclKind::InsertEdge => BindingElement::Edge,
                BindingDeclKind::PathBinding => BindingElement::Path,
                BindingDeclKind::LetAlias
                | BindingDeclKind::ForAlias
                | BindingDeclKind::ProjectionAlias
                | BindingDeclKind::YieldColumn => return None,
            };
            Some(BindingDef {
                binding: decl.id(),
                name: decl.name(),
                element,
                ty: decl.ty().clone(),
                label_predicate: decl.label_expr().cloned(),
                span: decl.span(),
            })
        })
        .collect()
}
