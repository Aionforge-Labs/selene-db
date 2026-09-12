//! Semantic element-test builder for path lowering.
//!
//! Resolves binding identities, types, and predicate identities from the
//! frozen semantic tree. Source syntax supplies only ordering and spans;
//! every identity comes from analyzer declarations, `PatternReuse`
//! references, and `ExprId` cells. Anonymous elements receive
//! [`crate::plan::logical::path::TemporaryBinding`] slots anchored at the
//! source element span.

use crate::{
    Quantifier, SourceSpan,
    analyze::{AnalyzedStatement, AnalyzedType, BindingDecl, BindingDeclKind, BindingId, ScopeId},
    plan::{
        PlannerError,
        logical::path::semantic::{
            BindingExposure, EdgeQuantifierKind, EdgeTest, NodeTest, TemporaryBinding,
            acceptance_for,
        },
    },
};

/// Builds [`NodeTest`] / [`EdgeTest`] values for one graph pattern.
pub(super) struct PatternBuilder<'a> {
    /// Frozen semantic tree identities resolve from here.
    pub(super) analyzed: &'a AnalyzedStatement,
    /// Scope used for anonymous elements without a declaration.
    pub(super) scope_fallback: ScopeId,
    /// Next per-automaton temporary slot.
    pub(super) next_temp: u32,
    /// Count of node tests built (diagnostic cursor).
    pub(super) node_tests: u32,
    /// Count of edge tests built (diagnostic cursor).
    pub(super) edge_tests: u32,
}

impl<'a> PatternBuilder<'a> {
    /// Allocate one temporary slot anchored at its source element span.
    fn next_temporary(&mut self, origin: SourceSpan, reason: &'static str) -> TemporaryBinding {
        let slot = self.next_temp;
        self.next_temp += 1;
        TemporaryBinding {
            slot,
            origin,
            reason,
        }
    }

    /// Return the lexical scope that owns `binding`.
    fn owning_scope(&self, binding: BindingId) -> ScopeId {
        for scope in self.analyzed.scopes.scopes() {
            if scope.locals.contains(&binding) || scope.imports.contains(&binding) {
                let index = self
                    .analyzed
                    .scopes
                    .scopes()
                    .iter()
                    .position(|candidate| {
                        candidate.span == scope.span
                            && candidate.locals == scope.locals
                            && candidate.imports == scope.imports
                    })
                    .unwrap_or(0);
                return ScopeId::new(index as u32);
            }
        }
        self.scope_fallback
    }

    /// Resolve one pattern name to its analyzer binding.
    ///
    /// Prefers the declaration at the exact source span, then falls back to a
    /// `PatternReuse` reference (repeated declarations share one identity).
    /// A missing cell is [`PlannerError::BindingResolutionLost`], never a cue
    /// to re-parse.
    pub(super) fn resolve_decl(
        &self,
        name: selene_core::DbString,
        span: SourceSpan,
        expected: BindingDeclKind,
    ) -> Result<BindingId, PlannerError> {
        if let Some(binding) = self
            .analyzed
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
        self.analyzed
            .references
            .iter()
            .find(|reference| {
                reference.name == name
                    && reference.span == span
                    && reference.kind == crate::analyze::BindingUseKind::PatternReuse
            })
            .map(|reference| reference.binding)
            .ok_or(PlannerError::BindingResolutionLost {
                binding: BindingId::new(u32::MAX),
                span,
            })
    }

    /// Resolve one source expression to its semantic identity.
    fn expr_id(
        &self,
        span: SourceSpan,
        expr: &crate::ValueExpr,
    ) -> Result<crate::analyze::ExprId, PlannerError> {
        self.analyzed
            .expr_ids
            .get(expr)
            .ok_or(PlannerError::ExpressionTypeMissing { span })
    }

    /// Build one node element test.
    pub(super) fn node_test(
        &mut self,
        node: &crate::NodePattern,
        index: usize,
    ) -> Result<NodeTest, PlannerError> {
        let binding = node
            .binding
            .clone()
            .map(|name| self.resolve_decl(name, node.span, BindingDeclKind::NodePattern))
            .transpose()?;
        let scope = binding.map_or(self.scope_fallback, |id| self.owning_scope(id));
        let ty = binding
            .and_then(|id| self.analyzed.scopes.declaration(id))
            .map(|decl| decl.ty().clone())
            .unwrap_or(AnalyzedType::Resolved(crate::GqlType::NodeRef));
        let temporary = binding.is_none().then(|| {
            let origin = node.span;
            self.next_temporary(origin, "anonymous node")
        });
        let mut property_predicates = Vec::with_capacity(node.properties.len());
        for (_, value) in &node.properties {
            property_predicates.push(self.expr_id(value.span(), value)?);
        }
        let inline_where = node
            .inline_where
            .as_ref()
            .map(|where_clause| self.expr_id(where_clause.span(), where_clause))
            .transpose()?;
        self.node_tests += 1;
        Ok(NodeTest {
            binding,
            temporary,
            ty,
            label: node.label_expr.clone(),
            property_predicates,
            inline_where,
            scope,
            element_index: index,
            origin: node.span,
        })
    }

    /// Build one edge element test with explicit exposure.
    ///
    /// `?` pairs only with [`BindingExposure::ConditionalSingleton`];
    /// bounded and gated-unbounded quantifiers pair only with
    /// [`BindingExposure::GroupList`], so `{0,1}` can never collapse into `?`.
    pub(super) fn edge_test(
        &mut self,
        edge: &crate::EdgePattern,
        index: usize,
    ) -> Result<EdgeTest, PlannerError> {
        let binding = edge
            .binding
            .clone()
            .map(|name| self.resolve_decl(name, edge.span, BindingDeclKind::EdgePattern))
            .transpose()?;
        let scope = binding.map_or(self.scope_fallback, |id| self.owning_scope(id));
        let (quantifier, exposure) = match edge.quantifier {
            None => {
                let hidden = binding.is_none().then(|| {
                    let origin = edge.span;
                    self.next_temporary(origin, "anonymous edge")
                });
                (
                    EdgeQuantifierKind::Single,
                    BindingExposure::Singleton { binding, hidden },
                )
            }
            Some(Quantifier::Questioned) => {
                let hidden = binding.is_none().then(|| {
                    let origin = edge.span;
                    self.next_temporary(origin, "anonymous questioned edge")
                });
                (
                    EdgeQuantifierKind::Questioned,
                    BindingExposure::ConditionalSingleton { binding, hidden },
                )
            }
            Some(Quantifier::GraphPattern {
                min,
                max: Some(max),
            }) => {
                let hidden = binding.is_none().then(|| {
                    let origin = edge.span;
                    self.next_temporary(origin, "anonymous group")
                });
                (
                    EdgeQuantifierKind::Bounded { min, max },
                    BindingExposure::GroupList { binding, hidden },
                )
            }
            Some(Quantifier::GraphPattern { min, max: None }) => {
                let hidden = binding.is_none().then(|| {
                    let origin = edge.span;
                    self.next_temporary(origin, "anonymous group")
                });
                (
                    EdgeQuantifierKind::Unbounded { min },
                    BindingExposure::GroupList { binding, hidden },
                )
            }
        };
        let mut property_predicates = Vec::with_capacity(edge.properties.len());
        for (_, value) in &edge.properties {
            property_predicates.push(self.expr_id(value.span(), value)?);
        }
        let inline_where = edge
            .inline_where
            .as_ref()
            .map(|where_clause| self.expr_id(where_clause.span(), where_clause))
            .transpose()?;
        self.edge_tests += 1;
        Ok(EdgeTest {
            exposure,
            quantifier,
            orientation: acceptance_for(edge.direction, edge.span),
            abbreviated: edge.abbreviated,
            label: edge.label_expr.clone(),
            property_predicates,
            inline_where,
            scope,
            element_index: index,
            origin: edge.span,
        })
    }
}

/// Match analyzer declaration kinds across the read/insert element families.
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
