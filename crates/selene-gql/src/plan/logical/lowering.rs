//! Semantic-to-logical lowering.
//!
//! Operators are built from the frozen semantic tree: binding declarations and
//! scopes, expression identities and types, write-set entries, and resolved
//! procedure applications. Source syntax supplies only statement ordering and
//! source spans; every identity, type, and effect comes from semantics. No
//! operator carries parser nodes, physical row coordinates, storage positions,
//! or execution policy.

use std::collections::BTreeMap;

use crate::{
    LimitValue, PipelineStatement, ProcedureRegistry, SourceSpan, Statement,
    analyze::{
        AnalyzedStatement, AnalyzedType, BindingDeclKind, BindingId, ExprId, ScopeId,
        StatementCategory,
    },
    plan::{
        BindingTableColumn, BindingTableSchema, PlannerError,
        logical::{
            effect::{EffectSummary, LogicalEffect, check_gp18, classify_analyzed},
            operator::{
                LogicalCallDescriptor, LogicalMutationDescriptor, LogicalOp, LogicalPageAmount,
                LogicalPlan, LogicalScanDescriptor,
            },
        },
    },
};

/// Lower one analyzed statement into a logical binding-table plan.
///
/// The returned plan preserves logical ordering, multiplicity, variable scope,
/// and type metadata from semantics. Mutation operators describe intent; the
/// caller stages them through the existing detached transaction state.
///
/// # Errors
///
/// Returns [`PlannerError`] when procedure metadata drifted between analysis
/// and lowering, when a required semantic cell is missing, or when one
/// statement mixes catalog and data effects under the selected GP18 policy.
pub fn lower_logical(
    analyzed: &AnalyzedStatement,
    registry: &dyn ProcedureRegistry,
) -> Result<LogicalPlan, PlannerError> {
    let effects = classify_analyzed(analyzed);
    check_gp18(&effects)?;
    let mut builder = LogicalBuilder::new(analyzed, registry, effects.clone());
    // Clone the source statement shape for the match below so the builder's
    // immutable borrow of `analyzed` does not conflict with the source borrow.
    // Only ordering and spans come from syntax; every identity, type, and
    // effect comes from the semantic tree.
    let source = analyzed.source().clone();
    match &source {
        Statement::Query(pipeline) => {
            builder.lower_query_pipeline(pipeline)?;
        }
        Statement::Composite { first, rest, .. } => {
            builder.lower_query_pipeline(first)?;
            for (_, rhs) in rest {
                builder.lower_query_pipeline(rhs)?;
            }
        }
        Statement::Chained { blocks, .. } => {
            for block in blocks {
                builder.lower_query_pipeline(block)?;
            }
        }
        Statement::Mutate(pipeline) => {
            builder.lower_mutation_pipeline(pipeline)?;
        }
        Statement::Call(call) => {
            builder.lower_top_level_call(call)?;
        }
        Statement::Ddl(_) | Statement::Explain { .. } => {
            builder.note_catalog_or_explain()?;
        }
        Statement::StartTransaction { span }
        | Statement::Commit { span }
        | Statement::Rollback { span } => {
            builder.finish_control(*span);
        }
        Statement::SessionSetValue { span, .. }
        | Statement::SessionSetTimeZone { span, .. }
        | Statement::SessionSetGraph { span, .. }
        | Statement::SessionReset { span, .. }
        | Statement::SessionClose { span } => {
            builder.finish_control(*span);
        }
    }
    Ok(builder.finish())
}

struct LogicalBuilder<'a, 'r> {
    analyzed: &'a AnalyzedStatement,
    registry: &'r dyn ProcedureRegistry,
    effects: EffectSummary,
    operators: Vec<LogicalOp>,
    current_schema: Vec<BindingTableColumn>,
    input_width: usize,
    scope: ScopeId,
}

impl<'a, 'r> LogicalBuilder<'a, 'r> {
    fn new(
        analyzed: &'a AnalyzedStatement,
        registry: &'r dyn ProcedureRegistry,
        effects: EffectSummary,
    ) -> Self {
        let scope = analyzed.root_scope();
        Self {
            analyzed,
            registry,
            effects,
            operators: Vec::new(),
            current_schema: Vec::new(),
            input_width: 0,
            scope,
        }
    }

    fn lower_query_pipeline(
        &mut self,
        pipeline: &crate::QueryPipeline,
    ) -> Result<(), PlannerError> {
        self.lower_scan_seed()?;
        for statement in &pipeline.statements {
            match statement {
                PipelineStatement::Match(_) => {
                    // Graph-pattern expansion beyond the leading scan seed is
                    // full family coverage owned by F03-PR04. The scan seed
                    // above already carries the semantic binding descriptors;
                    // this arm preserves ordering without inventing a second
                    // path representation here.
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
                    // LET extends the row; each new alias becomes a column.
                    for binding in bindings {
                        let ty = self.binding_type_for_alias(&binding.alias);
                        self.current_schema.push(BindingTableColumn {
                            name: Some(binding.alias.clone()),
                            hidden: None,
                            ty,
                        });
                    }
                    self.push(LogicalOp::Project {
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
                    // Row expansion preserves duplicates by construction: one
                    // output row per list element.
                    self.current_schema.push(BindingTableColumn {
                        name: Some(statement.alias.clone()),
                        hidden: None,
                        ty: AnalyzedType::Dynamic,
                    });
                    self.push(LogicalOp::Project {
                        expressions: vec![source],
                        scope: self.scope,
                        output_schema: self.schema(),
                        origin: statement.span,
                    });
                }
                PipelineStatement::Sorting(_) => {
                    // Ordering operators beyond page preservation are full
                    // family coverage owned by F03-PR04. Page below never
                    // reorders; sort preservation is documented in the
                    // operator contracts.
                }
                PipelineStatement::Offset(offset) => {
                    self.push_page(offset, &LimitValue::Count(u64::MAX, offset_span(offset)))?;
                }
                PipelineStatement::Limit(limit) => {
                    self.push_page(&LimitValue::Count(0, limit_span(limit)), limit)?;
                }
                PipelineStatement::Return(clause) => {
                    self.lower_return_projection(
                        &clause
                            .items
                            .iter()
                            .map(|item| (&item.expr, item.alias.clone(), item.span))
                            .collect::<Vec<_>>(),
                        clause.span,
                    )?;
                }
                PipelineStatement::With(clause) => {
                    self.lower_return_projection(
                        &clause
                            .items
                            .iter()
                            .map(|item| (&item.expr, item.alias.clone(), item.span))
                            .collect::<Vec<_>>(),
                        clause.span,
                    )?;
                }
                PipelineStatement::Call(call) => {
                    self.lower_nested_call(call)?;
                }
                PipelineStatement::CallSubquery(call) => {
                    // `CALL { ... }` bodies lower as nested pipelines in the
                    // old plan; the logical slice records the boundary scope
                    // and preserves the outer schema. Full subquery operator
                    // coverage stays with F03-PR04.
                    self.push(LogicalOp::Project {
                        expressions: Vec::new(),
                        scope: self.scope,
                        output_schema: self.schema(),
                        origin: call.span,
                    });
                }
            }
        }
        Ok(())
    }

    fn lower_mutation_pipeline(
        &mut self,
        pipeline: &crate::MutationPipeline,
    ) -> Result<(), PlannerError> {
        self.lower_scan_seed()?;
        for statement in &pipeline.statements {
            match statement {
                crate::MutationStatement::Match(_) => {}
                crate::MutationStatement::Filter(value) => {
                    let predicate = self.expr_id(value.span(), value)?;
                    self.push(LogicalOp::Filter {
                        predicate,
                        scope: self.scope,
                        output_schema: self.schema(),
                        origin: value.span(),
                    });
                }
                crate::MutationStatement::Insert(_)
                | crate::MutationStatement::Set(_)
                | crate::MutationStatement::Remove(_)
                | crate::MutationStatement::Delete(_) => {
                    // One ordinary mutation path: the descriptor below is
                    // built from the analyzer write set, not from parser
                    // payloads.
                }
            }
        }
        let descriptor = self.mutation_descriptor(pipeline.span)?;
        // Mutation output columns are the projection aliases visible after the
        // mutation boundary, resolved from semantic declarations.
        let output_schema = self.projection_schema();
        self.push(LogicalOp::Mutate {
            descriptor,
            output_schema,
            scope: self.scope,
            origin: pipeline.span,
        });
        Ok(())
    }

    fn lower_top_level_call(&mut self, call: &crate::ProcedureCall) -> Result<(), PlannerError> {
        let descriptor = self.call_descriptor(&call.name, call.span)?;
        let span = call.span;
        let mut output_schema = self.schema();
        for resolved in &self.analyzed.calls {
            if resolved.span() == span {
                for column in &resolved.metadata().output_schema.columns {
                    let decl_ty = self
                        .yield_type(&column.name)
                        .unwrap_or_else(|| AnalyzedType::Resolved(column.ty.clone()));
                    output_schema.columns.push(BindingTableColumn {
                        name: Some(column.name.clone()),
                        hidden: None,
                        ty: decl_ty,
                    });
                }
            }
        }
        self.current_schema = output_schema.columns.clone();
        self.push(LogicalOp::Call {
            descriptor,
            output_schema: self.schema(),
            scope: self.scope,
            origin: span,
        });
        Ok(())
    }

    fn lower_nested_call(&mut self, call: &crate::ProcedureCall) -> Result<(), PlannerError> {
        let span = call.span;
        let descriptor = self.call_descriptor(&call.name, span)?;
        // Nested calls extend the row with their yielded columns, resolved
        // from semantic yield declarations.
        for call in &self.analyzed.calls {
            if call.span() == span {
                for column in &call.metadata().output_schema.columns {
                    let decl_ty = self
                        .yield_type(&column.name)
                        .unwrap_or_else(|| AnalyzedType::Resolved(column.ty.clone()));
                    self.current_schema.push(BindingTableColumn {
                        name: Some(column.name.clone()),
                        hidden: None,
                        ty: decl_ty,
                    });
                }
            }
        }
        self.push(LogicalOp::Call {
            descriptor,
            output_schema: self.schema(),
            scope: self.scope,
            origin: span,
        });
        Ok(())
    }

    fn note_catalog_or_explain(&mut self) -> Result<(), PlannerError> {
        // Catalog DDL and EXPLAIN carry no binding-table operators in this
        // slice. Their effects remain classified (catalog vs query), their
        // origins remain in the plan summary, and full operator coverage stays
        // with F03-PR04. Recording no operator here is honest: the plan does
        // not claim a scan it did not lower.
        Ok(())
    }

    fn finish_control(&mut self, _span: SourceSpan) {}

    fn lower_scan_seed(&mut self) -> Result<(), PlannerError> {
        // Seed scans from semantic binding declarations of matchable kinds.
        // Only the first seed per pipeline is emitted; subsequent MATCH
        // prefixes are ordering-preserving no-ops until F03-PR04 owns the
        // full join family. This keeps the slice to one graph-access shape
        // while still building it from semantic descriptors.
        if !self.operators.is_empty() {
            return Ok(());
        }
        let mut seeded = false;
        for decl in self.analyzed.scopes.declarations() {
            let (is_node, ty) = match decl.kind() {
                BindingDeclKind::NodePattern => (true, decl.ty().clone()),
                BindingDeclKind::EdgePattern => (false, decl.ty().clone()),
                BindingDeclKind::LetAlias
                | BindingDeclKind::ForAlias
                | BindingDeclKind::ProjectionAlias
                | BindingDeclKind::YieldColumn
                | BindingDeclKind::InsertNode
                | BindingDeclKind::InsertEdge
                | BindingDeclKind::PathBinding => continue,
            };
            let descriptor = LogicalScanDescriptor {
                binding: decl.id(),
                scope: self.scope,
                ty: ty.clone(),
                is_node,
                origin: decl.span(),
            };
            self.current_schema.push(BindingTableColumn {
                name: Some(decl.name()),
                hidden: None,
                ty,
            });
            self.input_width = self.current_schema.len();
            self.push(LogicalOp::Scan {
                descriptor,
                output_schema: self.schema(),
                scope: self.scope,
                origin: decl.span(),
            });
            seeded = true;
            break;
        }
        if !seeded && self.current_schema.is_empty() {
            self.input_width = 0;
        }
        Ok(())
    }

    fn lower_return_projection(
        &mut self,
        items: &[(&crate::ValueExpr, Option<selene_core::DbString>, SourceSpan)],
        span: SourceSpan,
    ) -> Result<(), PlannerError> {
        let mut expressions = Vec::with_capacity(items.len());
        for (expr, _, expr_span) in items {
            expressions.push(self.expr_id(*expr_span, expr)?);
        }
        // Projection output columns preserve aliases from semantics: an
        // explicit `AS` alias wins, then a bare variable name, else the
        // column stays anonymous. Types come from semantic expression cells.
        let mut columns = Vec::with_capacity(items.len());
        for (expr, alias, _) in items {
            let ty = self
                .analyzed
                .expr_ids
                .get(expr)
                .map(|id| self.analyzed.expr_types.get(id).clone())
                .unwrap_or(AnalyzedType::Dynamic);
            let name = alias.clone().or_else(|| match expr {
                crate::ValueExpr::Variable { name, .. } => Some(name.clone()),
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
        Ok(())
    }

    fn push_page(&mut self, offset: &LimitValue, limit: &LimitValue) -> Result<(), PlannerError> {
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

    fn page_amount(
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

    fn mutation_descriptor(
        &self,
        span: SourceSpan,
    ) -> Result<LogicalMutationDescriptor, PlannerError> {
        let Some(write_set) = self.analyzed.write_set.as_ref() else {
            return Err(PlannerError::WriteSetMissing { span });
        };
        let mut inserts_node = false;
        let mut inserts_edge = false;
        let mut updates_graph = false;
        let mut deletes_target = false;
        for entry in &write_set.entries {
            match &entry.kind {
                crate::WriteKind::InsertNode { .. } => inserts_node = true,
                crate::WriteKind::InsertEdge { .. } => inserts_edge = true,
                crate::WriteKind::SetProperty { .. }
                | crate::WriteKind::SetLabel { .. }
                | crate::WriteKind::RemoveProperty { .. }
                | crate::WriteKind::RemoveLabel { .. } => updates_graph = true,
                crate::WriteKind::DeleteTarget { .. } => deletes_target = true,
            }
        }
        Ok(LogicalMutationDescriptor {
            write_entry_count: write_set.entries.len(),
            inserts_node,
            inserts_edge,
            updates_graph,
            deletes_target,
            origin: span,
        })
    }

    fn call_descriptor(
        &self,
        name: &[selene_core::DbString],
        span: SourceSpan,
    ) -> Result<LogicalCallDescriptor, PlannerError> {
        let Some(resolved) = self.analyzed.calls.iter().find(|call| call.span() == span) else {
            return Err(PlannerError::ProcedureMetadataMismatch {
                procedure: name.to_vec().into_boxed_slice(),
                detail: "call has no semantic application",
                span,
            });
        };
        // Effects resolve from current registration metadata, never from the
        // recorded semantic copy alone. A drift between analysis and lowering
        // must fail rather than execute with stale authority.
        let Some(current) = self.registry.lookup(name) else {
            return Err(PlannerError::UnknownProcedure {
                procedure: name.to_vec().into_boxed_slice(),
                span,
            });
        };
        if current.mutability != resolved.metadata().mutability
            || current.tier != resolved.metadata().tier
            || current.handle != resolved.metadata().handle
            || !resolved.same_signature(&current)
        {
            return Err(PlannerError::ProcedureMetadataMismatch {
                procedure: name.to_vec().into_boxed_slice(),
                detail: "procedure metadata changed between analyze and plan",
                span,
            });
        }
        Ok(LogicalCallDescriptor {
            name: name
                .iter()
                .map(|segment| segment.as_str().to_owned())
                .collect(),
            effect: LogicalEffect::from_mutability(current.mutability),
            argument_count: current.signature.parameters.len(),
            yield_count: current.output_schema.columns.len(),
            origin: span,
        })
    }

    fn projection_schema(&self) -> BindingTableSchema {
        let mut columns = Vec::new();
        for decl in self.analyzed.scopes.declarations() {
            match decl.kind() {
                BindingDeclKind::ProjectionAlias | BindingDeclKind::YieldColumn => {
                    columns.push(BindingTableColumn {
                        name: Some(decl.name()),
                        hidden: None,
                        ty: decl.ty().clone(),
                    });
                }
                BindingDeclKind::NodePattern
                | BindingDeclKind::EdgePattern
                | BindingDeclKind::LetAlias
                | BindingDeclKind::ForAlias
                | BindingDeclKind::InsertNode
                | BindingDeclKind::InsertEdge
                | BindingDeclKind::PathBinding => {}
            }
        }
        if columns.is_empty() {
            self.schema()
        } else {
            BindingTableSchema { columns }
        }
    }

    fn binding_type_for_alias(&self, name: &selene_core::DbString) -> AnalyzedType {
        self.analyzed
            .scopes
            .declarations()
            .iter()
            .find(|decl| decl.name() == *name)
            .map(|decl| decl.ty().clone())
            .unwrap_or(AnalyzedType::Dynamic)
    }

    fn yield_type(&self, name: &selene_core::DbString) -> Option<AnalyzedType> {
        self.analyzed
            .scopes
            .declarations()
            .iter()
            .find(|decl| decl.kind() == BindingDeclKind::YieldColumn && decl.name() == *name)
            .map(|decl| decl.ty().clone())
    }

    fn expr_id(&self, span: SourceSpan, expr: &crate::ValueExpr) -> Result<ExprId, PlannerError> {
        // A span-based fallback is forbidden: expression identity comes only
        // from the semantic lookup. A missing cell is a lowering error, not a
        // cue to re-parse syntax.
        self.analyzed
            .expr_ids
            .get(expr)
            .ok_or(PlannerError::ExpressionTypeMissing { span })
    }

    fn schema(&self) -> BindingTableSchema {
        BindingTableSchema {
            columns: self.current_schema.clone(),
        }
    }

    fn push(&mut self, op: LogicalOp) {
        self.operators.push(op);
    }

    fn finish(self) -> LogicalPlan {
        let output_schema = self
            .operators
            .last()
            .map_or_else(|| self.schema(), |op| op.output_schema().clone());
        LogicalPlan {
            operators: self.operators,
            effects: self.effects,
            output_schema,
            registry_version: self.analyzed.procedure_registry_version,
            input_width: self.input_width,
        }
    }
}

fn offset_span(value: &LimitValue) -> SourceSpan {
    match value {
        LimitValue::Count(_, span) | LimitValue::Parameter { span, .. } => *span,
    }
}

fn limit_span(value: &LimitValue) -> SourceSpan {
    match value {
        LimitValue::Count(_, span) | LimitValue::Parameter { span, .. } => *span,
    }
}

/// Measure lowering cost without asserting a service-level objective.
///
/// Runs `lower_logical` once and returns the wall-clock microseconds plus the
/// conservative dependency sizes the cache must compare. Callers report the
/// numbers; they never gate correctness on them.
#[must_use]
pub fn measure_lowering_cost(
    analyzed: &AnalyzedStatement,
    registry: &dyn ProcedureRegistry,
) -> (u128, BTreeMap<&'static str, usize>) {
    let start = std::time::Instant::now();
    let plan = lower_logical(analyzed, registry);
    let elapsed = start.elapsed().as_micros();
    let mut sizes = BTreeMap::new();
    sizes.insert("calls", analyzed.calls.len());
    sizes.insert(
        "write_entries",
        analyzed
            .write_set
            .as_ref()
            .map_or(0, |set| set.entries.len()),
    );
    sizes.insert("expressions", analyzed.expressions.len());
    if let Ok(plan) = plan {
        sizes.insert("operators", plan.operators.len());
        sizes.insert("output_columns", plan.output_schema.columns.len());
    }
    (elapsed, sizes)
}

#[allow(
    dead_code,
    reason = "category mapping is exercised through effect tests"
)]
const fn category_effect(category: StatementCategory) -> LogicalEffect {
    LogicalEffect::from_category(category)
}

#[allow(
    dead_code,
    reason = "binding lookup helper documents the semantic path"
)]
fn binding_lookup(analyzed: &AnalyzedStatement, binding: BindingId) -> Option<ScopeId> {
    analyzed
        .scopes
        .declarations()
        .iter()
        .find(|decl| decl.id() == binding)
        .map(|_| analyzed.root_scope())
}
