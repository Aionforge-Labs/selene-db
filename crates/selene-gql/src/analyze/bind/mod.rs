//! Bind-pass orchestration.

pub(crate) mod aggregate_rules;
pub(crate) mod call;
pub(crate) mod ddl;
pub(crate) mod element_ref;
pub(crate) mod expr;
pub(crate) mod expr_depth;
pub(crate) mod mutation;
pub(crate) mod parameters;
pub(crate) mod pattern;
pub(crate) mod query;
pub(crate) mod session;
pub(crate) mod transaction;
mod working_scope;

use selene_core::DbString;

use crate::{
    ProcedureRegistry, SourceSpan, Statement, ValueExpr,
    analyze::{
        ast::AnalyzedStatement,
        binding::{BindingDeclKind, BindingId, BindingUse, BindingUseKind},
        category::{self, StatementCategory},
        error::AnalysisError,
        scope::{BindingScopeTree, ScopeId, ScopeKind},
        types::{AnalyzedType, ExprId, ExprIdLookup, ExprTypeTable},
        write_set::{ElementKind, MutationWriteSet, WriteKind, WriteSetEntry},
    },
};

pub(crate) const ANALYZER_MAX_DEPTH: u32 = 256;

/// Analyze one statement with the binding pass.
pub(crate) fn bind_statement(
    stmt: std::sync::Arc<Statement>,
    registry: &dyn ProcedureRegistry,
    environment: Option<super::catalog::CatalogEnvironment>,
    supplied: &std::collections::BTreeMap<DbString, selene_core::StructuralType>,
) -> Result<AnalyzedStatement, AnalysisError> {
    let parameters = parameters::collect_statement_parameters(&stmt)?;
    let mut ctx = BindContext::new(stmt.span(), registry);
    ctx.catalog = environment.map(super::catalog::CatalogResolver::new);
    ctx.expr_ids.set_parameter_types(&parameters, supplied);
    let bind_result = (|| -> Result<(), AnalysisError> {
        match stmt.as_ref() {
            Statement::Query(pipeline) => query::bind_query_pipeline(&mut ctx, pipeline)?,
            Statement::Composite { first, rest, .. } => {
                ctx.with_child_scope(ScopeKind::Projection, first.span, true, |ctx| {
                    query::bind_query_pipeline(ctx, first)
                })?;
                for (_, pipeline) in rest {
                    ctx.with_child_scope(ScopeKind::Projection, pipeline.span, true, |ctx| {
                        query::bind_query_pipeline(ctx, pipeline)
                    })?;
                }
            }
            Statement::Chained { blocks, .. } => {
                // Each NEXT block consumes the prior block's binding table, so
                // we run sequential blocks in scopes chained off the previous
                // block's *terminal* scope (which holds the projection aliases
                // each block published). Boundary stays `false` so post-RETURN
                // bindings flow forward through GA07.
                let root = ctx.current_scope();
                let mut prior_tail = root;
                for block in blocks {
                    let block_root =
                        ctx.scopes
                            .push_scope(prior_tail, ScopeKind::Projection, block.span, false);
                    ctx.set_scope(block_root);
                    query::bind_query_pipeline(&mut ctx, block)?;
                    prior_tail = ctx.current_scope();
                }
                ctx.set_scope(root);
            }
            Statement::Mutate(pipeline) => mutation::bind_mutation_pipeline(&mut ctx, pipeline)?,
            Statement::Ddl(statement) => ddl::bind_ddl_statement(&mut ctx, statement)?,
            Statement::Call(call) => {
                call::bind_procedure_call(&mut ctx, call)?;
            }
            Statement::Explain { inner, .. } => bind_explain_inner(&mut ctx, inner)?,
            Statement::StartTransaction { span }
            | Statement::Commit { span }
            | Statement::Rollback { span } => {
                transaction::bind_transaction_control(&mut ctx, *span)
            }
            Statement::SessionSetValue { value, span, .. } => {
                expr::bind_value_expr(&mut ctx, value)?;
                session::bind_session_command(&mut ctx, *span);
            }
            Statement::SessionSetTimeZone { span, .. }
            | Statement::SessionSetGraph { span, .. }
            | Statement::SessionReset { span, .. }
            | Statement::SessionClose { span } => session::bind_session_command(&mut ctx, *span),
        }
        Ok(())
    })();
    bind_result?;
    let category = category::classify(&stmt, registry);
    let write_set = statement_write_set(&stmt).then(|| ctx.write_set.clone());
    Ok(ctx.finish(stmt, parameters, category, write_set))
}

fn bind_explain_inner(ctx: &mut BindContext<'_>, inner: &Statement) -> Result<(), AnalysisError> {
    match inner {
        Statement::Query(pipeline) => query::bind_query_pipeline(ctx, pipeline),
        Statement::Composite { first, rest, .. } => {
            ctx.with_child_scope(ScopeKind::Projection, first.span, true, |ctx| {
                query::bind_query_pipeline(ctx, first)
            })?;
            for (_, pipeline) in rest {
                ctx.with_child_scope(ScopeKind::Projection, pipeline.span, true, |ctx| {
                    query::bind_query_pipeline(ctx, pipeline)
                })?;
            }
            Ok(())
        }
        Statement::Chained { blocks, .. } => {
            let root = ctx.current_scope();
            let mut prior_tail = root;
            for block in blocks {
                let block_root =
                    ctx.scopes
                        .push_scope(prior_tail, ScopeKind::Projection, block.span, false);
                ctx.set_scope(block_root);
                query::bind_query_pipeline(ctx, block)?;
                prior_tail = ctx.current_scope();
            }
            ctx.set_scope(root);
            Ok(())
        }
        Statement::Mutate(pipeline) => mutation::bind_mutation_pipeline(ctx, pipeline),
        Statement::Ddl(statement) => ddl::bind_ddl_statement(ctx, statement),
        Statement::Call(call) => call::bind_procedure_call(ctx, call),
        Statement::Explain { span, .. } => Err(AnalysisError::NotImplemented {
            message: "EXPLAIN EXPLAIN is not supported".into(),
            span: *span,
            hint: None,
        }),
        Statement::StartTransaction { span }
        | Statement::Commit { span }
        | Statement::Rollback { span } => {
            transaction::bind_transaction_control(ctx, *span);
            Ok(())
        }
        // The grammar's `explainable_statement` rule excludes session commands,
        // so EXPLAIN never wraps one; reject defensively rather than silently
        // binding a non-explainable statement.
        Statement::SessionSetValue { span, .. }
        | Statement::SessionSetTimeZone { span, .. }
        | Statement::SessionSetGraph { span, .. }
        | Statement::SessionReset { span, .. }
        | Statement::SessionClose { span } => Err(AnalysisError::NotImplemented {
            message: "EXPLAIN of a SESSION command is not supported".into(),
            span: *span,
            hint: None,
        }),
    }
}

fn statement_write_set(statement: &Statement) -> bool {
    match statement {
        Statement::Mutate(_) => true,
        Statement::Explain { inner, .. } => matches!(inner.as_ref(), Statement::Mutate(_)),
        _ => false,
    }
}

pub(crate) struct BindContext<'ctx> {
    scopes: BindingScopeTree,
    current: ScopeId,
    references: Vec<BindingUse>,
    expr_types: ExprTypeTable,
    expr_ids: ExprIdLookup,
    write_set: MutationWriteSet,
    registry: &'ctx dyn ProcedureRegistry,
    expr_depth: u32,
    calls: Vec<super::semantic::ResolvedCall>,
    expressions: Vec<super::semantic::SemanticExpression>,
    catalog: Option<super::catalog::CatalogResolver>,
}

impl<'ctx> BindContext<'ctx> {
    fn new(root_span: SourceSpan, registry: &'ctx dyn ProcedureRegistry) -> Self {
        let scopes = BindingScopeTree::new(root_span);
        let current = scopes.root();
        Self {
            scopes,
            current,
            references: Vec::new(),
            expr_types: ExprTypeTable::default(),
            expr_ids: ExprIdLookup::default(),
            write_set: MutationWriteSet::default(),
            registry,
            expr_depth: 0,
            calls: Vec::new(),
            expressions: Vec::new(),
            catalog: None,
        }
    }

    fn finish(
        self,
        stmt: std::sync::Arc<Statement>,
        parameters: Vec<crate::analyze::ParameterUse>,
        category: StatementCategory,
        write_set: Option<MutationWriteSet>,
    ) -> AnalyzedStatement {
        let span = stmt.span();
        AnalyzedStatement::new(
            stmt,
            super::ast::SemanticTree {
                scopes: self.scopes,
                references: self.references,
                parameters,
                expr_types: self.expr_types,
                expr_ids: self.expr_ids,
                expressions: self.expressions,
                span,
                category,
                write_set,
                calls: self.calls,
                catalog: self.catalog.map(super::catalog::CatalogResolver::finish),
                profile: selene_profile::current_profile_identity(),
                procedure_registry_version: self.registry.registry_version(),
            },
        )
    }

    pub(crate) fn declare_strict_typed(
        &mut self,
        kind: BindingDeclKind,
        name: DbString,
        span: SourceSpan,
        ty: AnalyzedType,
    ) -> Result<BindingId, AnalysisError> {
        self.scopes
            .declare_strict_typed(self.current, kind, name, span, ty)
    }

    pub(crate) fn declare_or_reuse(
        &mut self,
        kind: BindingDeclKind,
        name: DbString,
        span: SourceSpan,
    ) -> Result<BindingId, AnalysisError> {
        self.declare_or_reuse_with_labels(kind, name, span, None)
    }

    pub(crate) fn declare_or_reuse_with_labels(
        &mut self,
        kind: BindingDeclKind,
        name: DbString,
        span: SourceSpan,
        labels: Option<crate::LabelExpr>,
    ) -> Result<BindingId, AnalysisError> {
        self.declare_or_reuse_with_labels_info(kind, name, span, labels)
            .map(|(binding, _)| binding)
    }

    pub(crate) fn declare_or_reuse_with_labels_info(
        &mut self,
        kind: BindingDeclKind,
        name: DbString,
        span: SourceSpan,
        labels: Option<crate::LabelExpr>,
    ) -> Result<(BindingId, bool), AnalysisError> {
        self.declare_or_reuse_with_labels_typed_info(
            kind,
            name,
            span,
            crate::analyze::binding::BindingDecl::default_type(kind),
            labels,
        )
    }

    pub(crate) fn declare_or_reuse_with_labels_typed_info(
        &mut self,
        kind: BindingDeclKind,
        name: DbString,
        span: SourceSpan,
        ty: AnalyzedType,
        labels: Option<crate::LabelExpr>,
    ) -> Result<(BindingId, bool), AnalysisError> {
        let (binding, reused) = self.scopes.declare_or_reuse_with_labels_typed(
            self.current,
            kind,
            name.clone(),
            span,
            ty,
            labels,
        )?;
        if reused {
            self.references.push(BindingUse {
                name,
                binding,
                span,
                kind: BindingUseKind::PatternReuse,
            });
        }
        Ok((binding, reused))
    }

    pub(crate) fn resolve(
        &mut self,
        name: DbString,
        span: SourceSpan,
        kind: BindingUseKind,
    ) -> Result<BindingId, AnalysisError> {
        let Some(binding) = self.scopes.resolve(self.current, name.clone()) else {
            return Err(AnalysisError::undefined_reference(name, span));
        };
        self.references.push(BindingUse {
            name,
            binding,
            span,
            kind,
        });
        Ok(binding)
    }

    pub(crate) fn element_kind(&self, binding: BindingId) -> ElementKind {
        let declaration = self
            .scopes
            .declaration(binding)
            .expect("resolved binding has declaration");
        ElementKind::from_decl_kind(declaration.kind())
    }

    pub(crate) fn record_write(
        &mut self,
        statement_index: usize,
        span: SourceSpan,
        kind: WriteKind,
    ) {
        self.write_set.push(WriteSetEntry {
            statement_index,
            span,
            kind,
        });
    }

    pub(crate) fn with_child_scope<T>(
        &mut self,
        kind: ScopeKind,
        span: SourceSpan,
        boundary: bool,
        f: impl FnOnce(&mut Self) -> Result<T, AnalysisError>,
    ) -> Result<T, AnalysisError> {
        let parent = self.current;
        let child = self.scopes.push_scope(parent, kind, span, boundary);
        self.current = child;
        let result = f(self);
        self.current = parent;
        result
    }

    /// Bind `f` inside a boundary subquery scope that imports ONLY the named
    /// outer bindings (ISO/IEC 39075:2024 §15.2 explicit variable scope, GP03).
    ///
    /// Each name is resolved against the current (parent) scope first — an
    /// unknown name is an undefined-reference error — and re-exposed in the
    /// child by id, so a body reference to an import resolves to the outer
    /// binding (and flows through `outer_binding_refs`). The child is a
    /// `boundary`, so any *unnamed* outer variable referenced in the body stops
    /// at the child and resolves to undefined. An empty `imports` slice
    /// (`CALL () { ... }`) yields a fully isolated scope. Duplicate import names
    /// are rejected by [`BindingScopeTree::import_binding`].
    pub(crate) fn with_imported_scope<T>(
        &mut self,
        imports: &[DbString],
        span: SourceSpan,
        f: impl FnOnce(&mut Self) -> Result<T, AnalysisError>,
    ) -> Result<T, AnalysisError> {
        let parent = self.current;
        // Resolve imports against the parent scope before entering the (boundary)
        // child — the child cannot see the parent, so resolution must happen now.
        let mut resolved = Vec::with_capacity(imports.len());
        for name in imports {
            let binding = self
                .scopes
                .resolve(parent, name.clone())
                .ok_or_else(|| AnalysisError::undefined_reference(name.clone(), span))?;
            resolved.push((name.clone(), binding));
        }
        let child = self
            .scopes
            .push_scope(parent, ScopeKind::Subquery, span, true);
        for (name, binding) in resolved {
            self.scopes.import_binding(child, binding, name, span)?;
        }
        self.current = child;
        let result = f(self);
        self.current = parent;
        result
    }

    pub(crate) fn allocate_expr(
        &mut self,
        expr: &ValueExpr,
        ty: AnalyzedType,
    ) -> Result<ExprId, AnalysisError> {
        let id = self
            .expr_types
            .push(ty)
            .map_err(|source| AnalysisError::StructuralType {
                source,
                span: expr.span(),
            })?;
        let binding = if let ValueExpr::Variable { name, .. } = expr {
            self.scopes.resolve(self.current, name.clone())
        } else {
            None
        };
        self.expressions
            .push(super::semantic::SemanticExpression::new(
                id,
                expr,
                self.current,
                binding,
                &self.expr_ids,
            ));
        self.expr_ids.insert(expr, id);
        Ok(id)
    }

    pub(crate) fn expr_type(&self, id: ExprId) -> &AnalyzedType {
        self.expr_types.get(id)
    }

    pub(crate) fn expr_id(&self, expr: &ValueExpr) -> Option<ExprId> {
        self.expr_ids.get(expr)
    }

    pub(crate) fn binding_type(&self, binding: BindingId) -> AnalyzedType {
        self.scopes
            .declaration(binding)
            .map(|decl| decl.ty().clone())
            .unwrap_or(AnalyzedType::Dynamic)
    }

    /// Enter a fresh projection scope and stay there for the rest of the
    /// pipeline.
    ///
    /// `boundary = false` matches ISO Feature `GA07` ("Ordering by discarded
    /// binding variables") for `RETURN`-style projections: pre-projection
    /// bindings stay reachable for downstream `ORDER BY` / `OFFSET` /
    /// `LIMIT`, and `RETURN *` keeps the entire input row visible.
    ///
    /// `boundary = true` matches the `WITH` continuation rule: pre-`WITH`
    /// bindings end at the boundary and only the WITH-projected aliases
    /// flow into the next clauses.
    pub(crate) fn enter_projection_scope(&mut self, span: SourceSpan, boundary: bool) {
        let child = self
            .scopes
            .push_scope(self.current, ScopeKind::Projection, span, boundary);
        self.current = child;
    }

    pub(crate) fn current_scope(&self) -> ScopeId {
        self.current
    }

    pub(crate) fn current_scope_has_visible_bindings(&self) -> bool {
        self.scopes.has_visible_bindings(self.current)
    }

    pub(crate) fn set_scope(&mut self, scope: ScopeId) {
        self.current = scope;
    }

    pub(crate) fn registry(&self) -> &'ctx dyn ProcedureRegistry {
        self.registry
    }

    pub(crate) fn with_expr_depth<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, AnalysisError>,
    ) -> Result<T, AnalysisError> {
        let next = self.expr_depth.saturating_add(1);
        if next > ANALYZER_MAX_DEPTH {
            return Err(AnalysisError::RecursionLimitExceeded { depth: next });
        }
        self.expr_depth = next;
        let result = f(self);
        self.expr_depth = self.expr_depth.saturating_sub(1);
        result
    }

    pub(crate) const fn at_expr_root(&self) -> bool {
        self.expr_depth == 0
    }
}
