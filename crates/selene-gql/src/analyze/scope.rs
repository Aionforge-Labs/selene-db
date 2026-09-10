//! Binding scope tree.

use selene_core::DbString;

use crate::{
    SourceSpan,
    analyze::{
        binding::{BindingDecl, BindingDeclKind, BindingId},
        error::{AnalysisError, PatternElementKind},
        types::AnalyzedType,
    },
};

/// Stable index of a lexical binding scope.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct ScopeId(u32);

impl ScopeId {
    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Return this scope's zero-based numeric index.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Metadata kind for a binding scope.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ScopeKind {
    /// Root statement scope.
    Statement,
    /// Isolated projection boundary created by `RETURN` or `WITH`.
    Projection,
    /// Nested subquery scope created by `EXISTS` or `VALUE { ... }`.
    Subquery,
    /// Diagnostic scope for a `CASE` branch.
    CaseBranch,
    /// Lexical AT working-schema clause.
    WorkingSchema,
    /// Lexical USE working-graph clause.
    WorkingGraph,
    /// Brace-delimited query specification origin.
    QuerySpecification,
}

impl ScopeKind {
    /// Return true when a scope boundary must keep inner pattern refinements
    /// from mutating declarations in an outer scope.
    #[must_use]
    pub const fn is_subquery_boundary(self) -> bool {
        matches!(self, Self::Subquery)
    }
}

/// One lexical scope in a [`BindingScopeTree`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingScope {
    /// Parent scope index; root has no parent.
    pub parent: Option<ScopeId>,
    /// Declaration indexes owned by this scope.
    pub locals: Vec<BindingId>,
    /// Outer binding ids re-exposed in this scope by reference (GP03 explicit
    /// variable-scope `CALL` imports — ISO §15.2). Visible for name resolution
    /// like [`Self::locals`], but **read-only**: a pattern that reuses an
    /// imported binding must not refine the (shared, outer) declaration's
    /// labels, matching GP02's cross-subquery-boundary behavior.
    pub imports: Vec<BindingId>,
    /// Best-effort source extent for this scope.
    pub span: SourceSpan,
    /// Diagnostic scope kind.
    pub kind: ScopeKind,
    /// Whether lookup should stop before walking to the parent.
    pub boundary: bool,
}

/// Lexical binding scopes allocated during one analyzer call.
#[derive(Clone, Debug)]
pub struct BindingScopeTree {
    decls: Vec<BindingDecl>,
    scopes: Vec<BindingScope>,
}

impl BindingScopeTree {
    /// Create a tree with a single root statement scope.
    #[must_use]
    pub fn new(root_span: SourceSpan) -> Self {
        Self {
            decls: Vec::new(),
            scopes: vec![BindingScope {
                parent: None,
                locals: Vec::new(),
                imports: Vec::new(),
                span: root_span,
                kind: ScopeKind::Statement,
                boundary: false,
            }],
        }
    }

    /// Return the root scope ID.
    #[must_use]
    pub const fn root(&self) -> ScopeId {
        ScopeId::new(0)
    }

    /// Return all declarations in allocation order.
    #[must_use]
    pub fn declarations(&self) -> &[BindingDecl] {
        &self.decls
    }

    /// Return all scopes in allocation order.
    #[must_use]
    pub fn scopes(&self) -> &[BindingScope] {
        &self.scopes
    }

    /// Return the declaration for a binding ID.
    #[must_use]
    pub fn declaration(&self, id: BindingId) -> Option<&BindingDecl> {
        self.decls.get(id.get() as usize)
    }

    /// Return a scope by ID.
    #[must_use]
    pub fn scope(&self, id: ScopeId) -> Option<&BindingScope> {
        self.scopes.get(id.get() as usize)
    }

    pub(crate) fn has_visible_bindings(&self, scope: ScopeId) -> bool {
        let mut cursor = Some(scope);
        while let Some(scope_id) = cursor {
            let Some(scope) = self.scope(scope_id) else {
                return false;
            };
            if !scope.locals.is_empty() || !scope.imports.is_empty() {
                return true;
            }
            if scope.boundary {
                return false;
            }
            cursor = scope.parent;
        }
        false
    }

    pub(crate) fn push_scope(
        &mut self,
        parent: ScopeId,
        kind: ScopeKind,
        span: SourceSpan,
        boundary: bool,
    ) -> ScopeId {
        let id = ScopeId::new(self.scopes.len() as u32);
        self.scopes.push(BindingScope {
            parent: Some(parent),
            locals: Vec::new(),
            imports: Vec::new(),
            span,
            kind,
            boundary,
        });
        id
    }

    pub(crate) fn declare_strict_typed(
        &mut self,
        scope: ScopeId,
        kind: BindingDeclKind,
        name: DbString,
        span: SourceSpan,
        ty: AnalyzedType,
    ) -> Result<BindingId, AnalysisError> {
        if let Some(prior) = self.resolve_local(scope, name.clone()) {
            let prior_span = self
                .declaration(prior)
                .map(BindingDecl::span)
                .unwrap_or_default();
            return Err(AnalysisError::Shadow {
                name,
                span,
                prior_span,
            });
        }
        Ok(self.declare_unchecked(scope, kind, name, span, ty, None))
    }

    /// Re-expose an already-declared binding inside `scope` under `name`,
    /// without creating a new declaration (the existing [`BindingId`] is shared).
    ///
    /// Used by GP03 explicit variable-scope `CALL (x, ...) { ... }` subqueries
    /// (ISO/IEC 39075:2024 §15.2) to seed a boundary subquery scope with only the
    /// named outer bindings: body references then resolve to the *outer* binding
    /// id (so they flow through `outer_binding_refs`), while any unnamed outer
    /// variable hits the boundary and resolves to nothing. A duplicate import
    /// name is rejected with [`AnalysisError::Shadow`] (ISO §15.2 forbids
    /// duplicate imported variable names).
    pub(crate) fn import_binding(
        &mut self,
        scope: ScopeId,
        binding: BindingId,
        name: DbString,
        span: SourceSpan,
    ) -> Result<(), AnalysisError> {
        if let Some(prior) = self.resolve_local(scope, name.clone()) {
            let prior_span = self
                .declaration(prior)
                .map(BindingDecl::span)
                .unwrap_or_default();
            return Err(AnalysisError::Shadow {
                name,
                span,
                prior_span,
            });
        }
        self.scopes[scope.get() as usize].imports.push(binding);
        Ok(())
    }

    /// Return true when `binding` is an *import* (re-exposed outer binding) of
    /// `scope`, rather than a declaration owned by it. Imports are read-only:
    /// pattern reuse must not refine their labels (see [`BindingScope::imports`]).
    fn scope_imports(&self, scope: ScopeId, binding: BindingId) -> bool {
        self.scope(scope)
            .is_some_and(|scope| scope.imports.contains(&binding))
    }

    pub(crate) fn declare_or_reuse_with_labels_typed(
        &mut self,
        scope: ScopeId,
        kind: BindingDeclKind,
        name: DbString,
        span: SourceSpan,
        ty: AnalyzedType,
        labels: Option<crate::LabelExpr>,
    ) -> Result<(BindingId, bool), AnalysisError> {
        if let Some((existing, existing_scope)) = self.resolve_with_scope(scope, name.clone()) {
            // Cross-element-kind reuse is a semantic error: a node variable
            // cannot be silently rebound as an edge/path variable, and vice
            // versa. Same-element-kind reuse (NodePattern <-> InsertNode,
            // EdgePattern <-> InsertEdge) is the legitimate path that lets
            // `MATCH (n) INSERT (n)-[:K]->(m)` work.
            let prior_decl = self
                .declaration(existing)
                .expect("resolved binding has decl");
            let new_element = PatternElementKind::from_decl_kind(kind);
            let prior_element = PatternElementKind::from_decl_kind(prior_decl.kind());
            if let (Some(new_kind), None) = (new_element, prior_element)
                && is_alias_decl_kind(prior_decl.kind())
            {
                return Err(AnalysisError::AliasReusedAsPatternBinding {
                    name: name.clone(),
                    prior_kind: prior_decl.kind(),
                    new_kind,
                    span,
                });
            }
            if let (Some(new_kind), Some(prior_kind)) = (new_element, prior_element)
                && new_kind != prior_kind
            {
                return Err(AnalysisError::PatternKindMismatch {
                    name: name.clone(),
                    prior: prior_kind,
                    current: new_kind,
                    span,
                    prior_span: prior_decl.span(),
                });
            }
            if prior_decl.ty() != &ty {
                return Err(AnalysisError::NotImplemented {
                    message:
                        "mixing scalar and group edge variables under one binding is not supported"
                            .into(),
                    span,
                    hint: Some(
                        "use different variable names for scalar edge bindings and quantified edge group variables"
                            .into(),
                    ),
                });
            }
            // Skip label refinement when the reused binding is an explicit
            // import (GP03): the declaration lives in an outer scope, and
            // refining it here would leak the subquery's label constraint into
            // the outer query's plan. This mirrors the cross-subquery-boundary
            // skip below (GP02), which protects implicitly-inherited bindings.
            if !self.crosses_subquery_boundary(scope, existing_scope)
                && !self.scope_imports(existing_scope, existing)
            {
                self.decls[existing.get() as usize].refine_label_expr(labels);
            }
            return Ok((existing, true));
        }
        Ok((
            self.declare_unchecked(scope, kind, name, span, ty, labels),
            false,
        ))
    }

    pub(crate) fn resolve(&self, scope: ScopeId, name: DbString) -> Option<BindingId> {
        self.resolve_with_scope(scope, name)
            .map(|(binding, _)| binding)
    }

    pub(crate) fn resolve_with_scope(
        &self,
        scope: ScopeId,
        name: DbString,
    ) -> Option<(BindingId, ScopeId)> {
        let mut cursor = Some(scope);
        while let Some(scope_id) = cursor {
            let scope = self.scope(scope_id)?;
            if let Some(id) = self.resolve_local(scope_id, name.clone()) {
                return Some((id, scope_id));
            }
            if scope.boundary {
                return None;
            }
            cursor = scope.parent;
        }
        None
    }

    fn crosses_subquery_boundary(&self, from: ScopeId, to: ScopeId) -> bool {
        let mut cursor = Some(from);
        while let Some(scope_id) = cursor {
            if scope_id == to {
                return false;
            }
            let Some(scope) = self.scope(scope_id) else {
                return false;
            };
            if scope.kind.is_subquery_boundary() {
                return true;
            }
            cursor = scope.parent;
        }
        false
    }

    fn declare_unchecked(
        &mut self,
        scope: ScopeId,
        kind: BindingDeclKind,
        name: DbString,
        span: SourceSpan,
        ty: AnalyzedType,
        labels: Option<crate::LabelExpr>,
    ) -> BindingId {
        let id = BindingId::new(self.decls.len() as u32);
        self.decls
            .push(BindingDecl::new(kind, id, name, span, ty, labels));
        self.scopes[scope.get() as usize].locals.push(id);
        id
    }

    fn resolve_local(&self, scope: ScopeId, name: DbString) -> Option<BindingId> {
        let scope = self.scope(scope)?;
        // Locals first, then imports (GP03): both are visible in this scope; a
        // local declared here shadows an import of the same name.
        scope
            .locals
            .iter()
            .chain(scope.imports.iter())
            .cloned()
            .find(|id| {
                self.declaration(*id)
                    .is_some_and(|decl| decl.name() == name)
            })
    }
}

const fn is_alias_decl_kind(kind: BindingDeclKind) -> bool {
    matches!(
        kind,
        BindingDeclKind::LetAlias
            | BindingDeclKind::ForAlias
            | BindingDeclKind::ProjectionAlias
            | BindingDeclKind::YieldColumn
    )
}
