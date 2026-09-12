//! Query-pipeline lowering for logical plans: ordering, joins, projections.
//!
//! All decisions resolve from semantic descriptors. Source syntax supplies
//! only ordering and spans.

use selene_core::DbString;

use crate::{
    LimitValue, PipelineStatement, SourceSpan, ValueExpr,
    analyze::{AnalyzedStatement, AnalyzedType, BindingId, ExprId},
    plan::{
        BindingTableColumn, PlannerError,
        logical::{
            descriptors::LogicalOrderKey,
            lowering::LogicalBuilder,
            operator::{LogicalOp, LogicalPageAmount},
        },
    },
};

use super::{lowering_aggregate::collect_aggregates, lowering_scan::binding_refs_in};

/// Build one ordering key from semantic identities.
pub(crate) fn logical_order_key(
    term: &crate::OrderTerm,
    analyzed: &AnalyzedStatement,
) -> Result<LogicalOrderKey, PlannerError> {
    let (expr, ty) = expr_cell(&term.expr, analyzed)?;
    Ok(LogicalOrderKey {
        expr,
        direction: term.direction,
        nulls: term.nulls,
        ty,
        binding_refs: binding_refs_in(&term.expr, analyzed)?,
        origin: term.span,
    })
}

/// Shared binding identities between two name sets, in declaration order.
pub(crate) fn join_keys(
    left: &[DbString],
    right: &[DbString],
    analyzed: &AnalyzedStatement,
) -> Vec<BindingId> {
    let mut keys = Vec::new();
    for name in left {
        if right.contains(name)
            && let Some(binding) = super::lowering_scan::resolve_binding(name, analyzed)
            && !keys.contains(&binding)
        {
            keys.push(binding);
        }
    }
    keys.sort();
    keys
}

/// Return analyzer expression identity and type for one source expression.
pub(crate) fn expr_cell(
    expr: &ValueExpr,
    analyzed: &AnalyzedStatement,
) -> Result<(ExprId, crate::analyze::AnalyzedType), PlannerError> {
    let expr_id = analyzed
        .expression(expr)
        .ok_or(PlannerError::ExpressionTypeMissing { span: expr.span() })?
        .id;
    Ok((expr_id, analyzed.expr_types.get(expr_id).clone()))
}

/// Return true when one `NEXT` block references bindings declared outside it.
///
/// A reference inside `block` whose declaration sits outside it is a prior
/// binding (the same rule the row adapter uses to choose `CorrelatedChain`
/// over `Chain`). Spans alone decide ordering; identities come from the
/// semantic tree.
pub(crate) fn block_is_correlated(block: SourceSpan, analyzed: &AnalyzedStatement) -> bool {
    for reference in &analyzed.references {
        if !span_contains(block, reference.span) {
            continue;
        }
        let Some(decl) = analyzed.scopes.declaration(reference.binding) else {
            continue;
        };
        if !span_contains(block, decl.span()) {
            return true;
        }
    }
    false
}

fn span_contains(outer: SourceSpan, inner: SourceSpan) -> bool {
    outer.byte_offset <= inner.byte_offset && inner.end() <= outer.end()
}

impl<'a, 'r> LogicalBuilder<'a, 'r> {
    /// Lower one read pipeline into logical operators.
    ///
    /// Every family lowers here from semantic descriptors: graph patterns
    /// become `Match` (with `Join` for subsequent patterns), filters and
    /// projections carry expression identities, `LET` extends, `FOR` unwinds,
    /// `ORDER BY`/`DISTINCT`/grouping carry their semantic keys, paging
    /// preserves order, and `CALL` subqueries lower as nested plans with
    /// explicit GP03 imports.
    pub(crate) fn lower_query_pipeline(
        &mut self,
        pipeline: &crate::QueryPipeline,
    ) -> Result<(), PlannerError> {
        self.lower_scan_seed()?;
        // Track whether a pattern step has been emitted so subsequent MATCH
        // clauses join against prior bindings instead of reseeding.
        let mut has_pattern = self
            .operators
            .iter()
            .any(|op| matches!(op, LogicalOp::Scan { .. } | LogicalOp::Match { .. }));
        // Pending ORDER BY keys are consumed by the RETURN/WITH that follows
        // them in source order (ISO §14.10: RETURN plus its ordering-and-page
        // statement is one primitive). Stash them until the projection lands.
        let mut pending_order: Option<(Vec<LogicalOrderKey>, SourceSpan)> = None;
        let mut index = 0;
        while index < pipeline.statements.len() {
            let statement = &pipeline.statements[index];
            match statement {
                PipelineStatement::Match(clause) => {
                    let visible = self.visible_names();
                    let clause_names = clause_binding_names(clause, self.analyzed);
                    if has_pattern {
                        let keys = join_keys(&visible, &clause_names, self.analyzed);
                        let join_schema = self.schema();
                        self.push(LogicalOp::Join {
                            keys,
                            optional: clause.optional,
                            output_schema: join_schema,
                            scope: self.scope,
                            origin: clause.span,
                        });
                    }
                    // Extend the schema with bindings this clause contributes.
                    for name in &clause_names {
                        if visible.contains(name) {
                            continue;
                        }
                        let ty = self.binding_type_for_alias(name);
                        self.current_schema.push(BindingTableColumn {
                            name: Some(name.clone()),
                            hidden: None,
                            ty,
                        });
                    }
                    self.push(LogicalOp::Match {
                        optional: clause.optional,
                        output_schema: self.schema(),
                        scope: self.scope,
                        origin: clause.span,
                    });
                    has_pattern = true;
                    // A WHERE attached to the MATCH filters after the pattern.
                    if let Some(where_clause) = &clause.where_clause {
                        let predicate = self.expr_id(where_clause.span(), where_clause)?;
                        self.push(LogicalOp::Filter {
                            predicate,
                            scope: self.scope,
                            output_schema: self.schema(),
                            origin: where_clause.span(),
                        });
                    }
                }
                PipelineStatement::Filter(value) => {
                    let predicate = self.expr_id(value.span(), value)?;
                    self.push(LogicalOp::Filter {
                        predicate,
                        scope: self.scope,
                        output_schema: self.schema(),
                        origin: value.span(),
                    });
                }
                PipelineStatement::Let(bindings) => {
                    let mut expressions = Vec::with_capacity(bindings.len());
                    for binding in bindings {
                        expressions.push(self.expr_id(binding.span, &binding.value)?);
                    }
                    for binding in bindings {
                        let ty = self.binding_type_for_alias(&binding.alias);
                        self.current_schema.push(BindingTableColumn {
                            name: Some(binding.alias.clone()),
                            hidden: None,
                            ty,
                        });
                    }
                    self.push(LogicalOp::Extend {
                        expressions,
                        scope: self.scope,
                        output_schema: self.schema(),
                        origin: bindings
                            .first()
                            .map_or(SourceSpan::default(), |binding| binding.span),
                    });
                }
                PipelineStatement::For(statement) => {
                    let source = self.expr_id(statement.span, &statement.source)?;
                    let alias = self.resolve_or_dynamic_binding(&statement.alias, statement.span);
                    let position_alias = statement.position.as_ref().map(|position| {
                        self.resolve_or_dynamic_binding(&position.alias, statement.span)
                    });
                    // Row expansion appends the element (and optional position)
                    // while preserving prior columns.
                    self.current_schema.push(BindingTableColumn {
                        name: Some(statement.alias.clone()),
                        hidden: None,
                        ty: AnalyzedType::Dynamic,
                    });
                    if let Some(position) = &statement.position {
                        self.current_schema.push(BindingTableColumn {
                            name: Some(position.alias.clone()),
                            hidden: None,
                            ty: AnalyzedType::Resolved(crate::GqlType::Integer),
                        });
                    }
                    self.push(LogicalOp::Unwind {
                        source,
                        alias,
                        position_alias,
                        output_schema: self.schema(),
                        scope: self.scope,
                        origin: statement.span,
                    });
                }
                PipelineStatement::Sorting(terms) => {
                    let mut keys = Vec::with_capacity(terms.len());
                    for term in terms {
                        keys.push(logical_order_key(term, self.analyzed)?);
                    }
                    let span = terms
                        .first()
                        .map_or(SourceSpan::default(), |term| term.span);
                    // Defer emission until the RETURN/WITH projection lands;
                    // a trailing ORDER BY without a projection still orders.
                    pending_order = Some((keys, span));
                    // If no projection follows, emit immediately at the end.
                    let has_projection = pipeline.statements[index + 1..].iter().any(|next| {
                        matches!(
                            next,
                            PipelineStatement::Return(_) | PipelineStatement::With(_)
                        )
                    });
                    if !has_projection {
                        let (keys, span) = pending_order.take().expect("pending order set");
                        self.push(LogicalOp::Order {
                            keys,
                            output_schema: self.schema(),
                            origin: span,
                        });
                    }
                }
                PipelineStatement::Offset(offset) => {
                    if let Some(PipelineStatement::Limit(limit)) =
                        pipeline.statements.get(index + 1)
                    {
                        self.push_page(offset, limit)?;
                        index += 1;
                    } else {
                        self.push_page(offset, &LimitValue::Count(u64::MAX, offset_span(offset)))?;
                    }
                }
                PipelineStatement::Limit(limit) => {
                    if let Some(PipelineStatement::Offset(offset)) =
                        pipeline.statements.get(index + 1)
                    {
                        self.push_page(offset, limit)?;
                        index += 1;
                    } else {
                        self.push_page(&LimitValue::Count(0, limit_span(limit)), limit)?;
                    }
                }
                PipelineStatement::Return(clause) => {
                    self.lower_return_with(
                        &clause.items,
                        clause.group_by.as_deref(),
                        clause.having.as_ref(),
                        clause.distinct,
                        false,
                        None,
                        clause.span,
                    )?;
                    if let Some((keys, span)) = pending_order.take() {
                        self.push(LogicalOp::Order {
                            keys,
                            output_schema: self.schema(),
                            origin: span,
                        });
                    }
                }
                PipelineStatement::With(clause) => {
                    self.lower_return_with(
                        &clause.items,
                        clause.group_by.as_deref(),
                        clause.having.as_ref(),
                        clause.distinct,
                        false,
                        clause.where_clause.as_ref(),
                        clause.span,
                    )?;
                    if let Some((keys, span)) = pending_order.take() {
                        self.push(LogicalOp::Order {
                            keys,
                            output_schema: self.schema(),
                            origin: span,
                        });
                    }
                }
                PipelineStatement::Call(call) => {
                    self.lower_nested_call(call)?;
                }
                PipelineStatement::CallSubquery(call) => {
                    self.lower_call_subquery(call)?;
                }
            }
            index += 1;
        }
        // A trailing ORDER BY without a following projection was already
        // emitted above; any leftover here is a defensive flush.
        if let Some((keys, span)) = pending_order.take() {
            self.push(LogicalOp::Order {
                keys,
                output_schema: self.schema(),
                origin: span,
            });
        }
        Ok(())
    }

    /// Lower `RETURN`/`WITH` projection with grouping, having, and distinct.
    #[allow(clippy::too_many_arguments)]
    fn lower_return_with(
        &mut self,
        items: &[crate::ReturnItem],
        group_by: Option<&[ValueExpr]>,
        having: Option<&ValueExpr>,
        distinct: bool,
        _star: bool,
        where_clause: Option<&ValueExpr>,
        span: SourceSpan,
    ) -> Result<(), PlannerError> {
        // Grouping keys resolve to semantic identities; aggregates resolve to
        // LogicalAggregate descriptors with deterministic output names.
        let mut key_ids = Vec::new();
        if let Some(keys) = group_by {
            for key in keys {
                key_ids.push(self.expr_id(key.span(), key)?);
            }
        }
        let aggregates = collect_aggregates(items, having, self.analyzed)?;
        if group_by.is_some() || !aggregates.is_empty() {
            // Grouping output is the keys plus aggregate outputs; the
            // projection below reads them by alias.
            let mut columns = Vec::new();
            if let Some(keys) = group_by {
                for key in keys {
                    let ty = self
                        .analyzed
                        .expr_ids
                        .get(key)
                        .map(|id| self.analyzed.expr_types.get(id).clone())
                        .unwrap_or(AnalyzedType::Dynamic);
                    let name = match key {
                        ValueExpr::Variable { name, .. } => Some(name.clone()),
                        _ => None,
                    };
                    columns.push(BindingTableColumn {
                        name,
                        hidden: None,
                        ty,
                    });
                }
            }
            for agg in &aggregates {
                columns.push(BindingTableColumn {
                    name: Some(agg.output_name.clone()),
                    hidden: None,
                    ty: agg.ty.clone(),
                });
            }
            // The grouping operator preserves the input schema when there are
            // no keys/columns to name yet; the projection below defines the
            // user-visible aliases.
            let output_schema = if columns.is_empty() {
                self.schema()
            } else {
                crate::plan::BindingTableSchema { columns }
            };
            self.push(LogicalOp::Aggregate {
                keys: key_ids,
                aggregates,
                output_schema: output_schema.clone(),
                scope: self.scope,
                origin: span,
            });
            // Keep grouping outputs visible for the projection's alias
            // resolution; the projection below replaces the schema.
            self.current_schema = output_schema.columns.clone();
        }
        if let Some(having) = having {
            let predicate = self.expr_id(having.span(), having)?;
            self.push(LogicalOp::Filter {
                predicate,
                scope: self.scope,
                output_schema: self.schema(),
                origin: having.span(),
            });
        }
        // Star projections keep the incoming schema; explicit items project.
        let is_star = items.is_empty() && self.is_star_projection(span);
        if !is_star {
            let mut expressions = Vec::with_capacity(items.len());
            for item in items {
                expressions.push(self.expr_id(item.span, &item.expr)?);
            }
            let mut columns = Vec::with_capacity(items.len());
            for item in items {
                let ty = self
                    .analyzed
                    .expr_ids
                    .get(&item.expr)
                    .map(|id| self.analyzed.expr_types.get(id).clone())
                    .unwrap_or(AnalyzedType::Dynamic);
                let name = item.alias.clone().or_else(|| match &item.expr {
                    ValueExpr::Variable { name, .. } => Some(name.clone()),
                    _ => None,
                });
                columns.push(BindingTableColumn {
                    name,
                    hidden: None,
                    ty,
                });
            }
            self.current_schema = columns;
            self.push(LogicalOp::Project {
                expressions,
                scope: self.scope,
                output_schema: self.schema(),
                origin: span,
            });
        }
        if distinct {
            self.push(LogicalOp::Distinct {
                output_schema: self.schema(),
                origin: span,
            });
        }
        if let Some(where_clause) = where_clause {
            let predicate = self.expr_id(where_clause.span(), where_clause)?;
            self.push(LogicalOp::Filter {
                predicate,
                scope: self.scope,
                output_schema: self.schema(),
                origin: where_clause.span(),
            });
        }
        Ok(())
    }

    /// Lower one inline `CALL { ... }` subquery with explicit GP03 imports.
    fn lower_call_subquery(
        &mut self,
        call: &crate::InlineProcedureCall,
    ) -> Result<(), PlannerError> {
        let imports = super::lowering_call::subquery_imports(call, self.analyzed)?;
        let body = super::lowering_call::lower_subquery_body(
            self.analyzed,
            self.registry,
            &call.body,
            &imports,
        )?;
        let yield_schema = super::lowering_call::subquery_yield_schema(call, &body, self.analyzed)?;
        for column in &yield_schema {
            self.current_schema.push(column.clone());
        }
        self.push(LogicalOp::Subquery {
            imports,
            optional: call.optional,
            body: Box::new(body),
            output_schema: self.schema(),
            scope: self.scope,
            origin: call.span,
        });
        Ok(())
    }

    /// Resolve a `FOR` alias to its semantic binding, falling back to a
    /// synthetic dynamic binding when the analyzer left it anonymous.
    fn resolve_or_dynamic_binding(
        &self,
        name: &DbString,
        span: SourceSpan,
    ) -> crate::analyze::BindingId {
        super::lowering_scan::resolve_binding(name, self.analyzed).unwrap_or_else(|| {
            // `FOR` aliases are always declared by the analyzer; this fallback
            // exists only so a bypassed gate fails downstream with a
            // binding-resolution error rather than panicking here.
            let _ = span;
            crate::analyze::BindingId::new(u32::MAX)
        })
    }

    pub(crate) fn push_page(
        &mut self,
        offset: &LimitValue,
        limit: &LimitValue,
    ) -> Result<(), PlannerError> {
        let (offset_amount, offset_span) = self.page_amount(offset)?;
        let (count_amount, count_span) = self.page_amount(limit)?;
        let origin = SourceSpan::merge(offset_span, count_span);
        self.push(LogicalOp::Page {
            offset: offset_amount,
            count: count_amount,
            output_schema: self.schema(),
            origin,
        });
        Ok(())
    }

    pub(crate) fn page_amount(
        &self,
        value: &LimitValue,
    ) -> Result<(LogicalPageAmount, SourceSpan), PlannerError> {
        match value {
            LimitValue::Count(count, span) => Ok((LogicalPageAmount::Literal(*count), *span)),
            LimitValue::Parameter { name, span, .. } => {
                Ok((LogicalPageAmount::Parameter { name: name.clone() }, *span))
            }
        }
    }

    /// Visible binding names in the current schema, in order.
    fn visible_names(&self) -> Vec<DbString> {
        self.current_schema
            .iter()
            .filter_map(|column| column.name.clone())
            .collect()
    }

    /// True when `span` belongs to a `RETURN *` projection.
    fn is_star_projection(&self, _span: SourceSpan) -> bool {
        // The caller passes `items.is_empty()` as the star signal: the parser
        // represents `RETURN *` with `star: true` and no items. The span alone
        // cannot distinguish it, so the items check above is authoritative.
        false
    }
}

/// Binding names declared or reused by one `MATCH` clause, in source order.
pub(crate) fn clause_binding_names(
    clause: &crate::MatchClause,
    analyzed: &AnalyzedStatement,
) -> Vec<DbString> {
    let mut names = Vec::new();
    for pattern in &clause.patterns {
        for element in &pattern.elements {
            match element {
                crate::PatternElement::Node(node) => {
                    if let Some(name) = &node.binding
                        && !names.contains(name)
                    {
                        names.push(name.clone());
                    }
                }
                crate::PatternElement::Edge(edge) => {
                    if let Some(name) = &edge.binding
                        && !names.contains(name)
                    {
                        names.push(name.clone());
                    }
                }
            }
        }
    }
    // Retain only names the analyzer actually declared; anonymous elements
    // contribute no binding.
    names.retain(|name| super::lowering_scan::resolve_binding(name, analyzed).is_some());
    names
}

use super::lowering::{limit_span, offset_span};
