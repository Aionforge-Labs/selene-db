//! Semantic-to-logical lowering.
//!
//! Operators are built from the frozen semantic tree: binding declarations and
//! scopes, expression identities and types, write-set entries, and resolved
//! procedure applications. Source syntax supplies only statement ordering and
//! source spans; every identity, type, and effect comes from semantics. No
//! operator carries parser nodes, physical row coordinates, storage positions,
//! or execution policy.

use crate::{
    LimitValue, ProcedureRegistry, SourceSpan, Statement,
    analyze::{AnalyzedStatement, AnalyzedType, BindingDeclKind, ExprId, ScopeId},
    plan::{
        BindingTableColumn, BindingTableSchema, PlannerError,
        logical::{
            descriptors::{LogicalCallDescriptor, LogicalScanDescriptor},
            effect::{EffectSummary, LogicalEffect, check_gp18, classify_analyzed},
            operator::{LogicalOp, LogicalPlan},
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
            // The union output is the left-arm schema per the operator
            // contract; every right arm lowers in isolation below.
            let left_schema = builder.schema();
            let outer_input_width = builder.input_width;
            for (op, rhs) in rest {
                let rhs_span = rhs.span;
                builder.current_schema = Vec::new();
                builder.pipeline_base = builder.operators.len();
                builder.lower_query_pipeline(rhs)?;
                // Restore the left-arm schema so the boundary carries it and
                // no right arm inherits (or leaks into) sibling bindings.
                builder.current_schema = left_schema.columns.clone();
                builder.push_union(*op, rhs_span)?;
                builder.pipeline_base = builder.operators.len();
                builder.input_width = outer_input_width;
            }
        }
        Statement::Chained { blocks, .. } => {
            let Some((first, rest)) = blocks.split_first() else {
                // Empty chain lowers to no operators; the empty plan below
                // preserves the statement effect without claiming work.
                return Ok(
                    builder.finish(crate::plan::logical::path::lowering::LoweredPathSet::empty())
                );
            };
            builder.lower_query_pipeline(first)?;
            let outer_input_width = builder.input_width;
            for block in rest {
                let correlated = super::lowering_query::block_is_correlated(block.span, analyzed);
                let span = block.span;
                builder.current_schema = Vec::new();
                builder.pipeline_base = builder.operators.len();
                builder.lower_query_pipeline(block)?;
                builder.push_chain(correlated, span)?;
                builder.pipeline_base = builder.operators.len();
                builder.input_width = outer_input_width;
            }
        }
        Statement::Mutate(pipeline) => {
            builder.lower_mutation_pipeline(pipeline)?;
        }
        Statement::Call(call) => {
            builder.lower_top_level_call(call)?;
        }
        Statement::Ddl(statement) => {
            builder.lower_catalog(statement)?;
        }
        Statement::Explain { inner, span } => {
            builder.lower_explain(inner, *span, registry)?;
        }
        Statement::StartTransaction { span } => {
            builder.push_control(
                crate::plan::logical::descriptors::LogicalControlKind::StartTransaction,
                *span,
            );
        }
        Statement::Commit { span } => {
            builder.push_control(
                crate::plan::logical::descriptors::LogicalControlKind::Commit,
                *span,
            );
        }
        Statement::Rollback { span } => {
            builder.push_control(
                crate::plan::logical::descriptors::LogicalControlKind::Rollback,
                *span,
            );
        }
        Statement::SessionSetValue { span, .. } => {
            builder.push_control(
                crate::plan::logical::descriptors::LogicalControlKind::SessionSetValue,
                *span,
            );
        }
        Statement::SessionSetTimeZone { span, .. } => {
            builder.push_control(
                crate::plan::logical::descriptors::LogicalControlKind::SessionSetTimeZone,
                *span,
            );
        }
        Statement::SessionSetGraph { span, .. } => {
            builder.push_control(
                crate::plan::logical::descriptors::LogicalControlKind::SessionSetGraph,
                *span,
            );
        }
        Statement::SessionReset { span, .. } => {
            builder.push_control(
                crate::plan::logical::descriptors::LogicalControlKind::SessionReset,
                *span,
            );
        }
        Statement::SessionClose { span } => {
            builder.push_control(
                crate::plan::logical::descriptors::LogicalControlKind::SessionClose,
                *span,
            );
        }
    }
    let paths = crate::plan::logical::path::lowering::lower_path_automata_with_defaults(analyzed)?;
    Ok(builder.finish(paths))
}

pub(crate) struct LogicalBuilder<'a, 'r> {
    pub(crate) analyzed: &'a AnalyzedStatement,
    pub(crate) registry: &'r dyn ProcedureRegistry,
    pub(crate) effects: EffectSummary,
    pub(crate) operators: Vec<LogicalOp>,
    pub(crate) current_schema: Vec<BindingTableColumn>,
    pub(crate) input_width: usize,
    pub(crate) scope: ScopeId,
    /// Start index in `operators` of the pipeline currently being lowered.
    ///
    /// Each `UNION` arm / `NEXT` block lowers as an isolated pipeline anchored
    /// here, so seed emission and pattern-join decisions never observe a
    /// sibling arm's operators.
    pub(crate) pipeline_base: usize,
}

impl<'a, 'r> LogicalBuilder<'a, 'r> {
    pub(crate) fn new(
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
            pipeline_base: 0,
        }
    }

    pub(crate) fn lower_mutation_pipeline(
        &mut self,
        pipeline: &crate::MutationPipeline,
    ) -> Result<(), PlannerError> {
        self.lower_scan_seed_in(pipeline.span)?;
        let mut has_pattern = false;
        for statement in &pipeline.statements {
            match statement {
                crate::MutationStatement::Match(clause) => {
                    let visible: Vec<selene_core::DbString> = self
                        .current_schema
                        .iter()
                        .filter_map(|column| column.name.clone())
                        .collect();
                    let clause_names =
                        super::lowering_query::clause_binding_names(clause, self.analyzed);
                    if has_pattern {
                        let keys = super::lowering_query::join_keys(
                            &visible,
                            &clause_names,
                            self.analyzed,
                        );
                        let join_schema = self.schema();
                        self.push(LogicalOp::Join {
                            keys,
                            optional: clause.optional,
                            output_schema: join_schema,
                            scope: self.scope,
                            origin: clause.span,
                        });
                    }
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

    pub(crate) fn lower_top_level_call(
        &mut self,
        call: &crate::ProcedureCall,
    ) -> Result<(), PlannerError> {
        let descriptor = self.call_descriptor(call)?;
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

    pub(crate) fn lower_nested_call(
        &mut self,
        call: &crate::ProcedureCall,
    ) -> Result<(), PlannerError> {
        let span = call.span;
        let descriptor = self.call_descriptor(call)?;
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

    pub(crate) fn lower_catalog(
        &mut self,
        statement: &crate::DdlStatement,
    ) -> Result<(), PlannerError> {
        let kind = super::lowering_catalog::catalog_kind_for_ddl(statement);
        let output_schema = super::lowering_catalog::catalog_output_schema(statement)?;
        self.current_schema = output_schema.columns.clone();
        self.push(LogicalOp::Catalog {
            kind,
            output_schema: self.schema(),
            origin: statement.span(),
        });
        Ok(())
    }

    pub(crate) fn lower_explain(
        &mut self,
        inner: &Statement,
        span: SourceSpan,
        registry: &dyn ProcedureRegistry,
    ) -> Result<(), PlannerError> {
        // EXPLAIN never executes its inner plan. Lower the inner statement
        // through the same semantic route into a child logical plan so the
        // wrapper transports — rather than rederives — its decisions.
        let inner_plan = lower_inner_statement(self.analyzed, registry, inner)?;
        let output_schema = super::lowering_catalog::explain_output_schema(span)?;
        self.current_schema = output_schema.columns.clone();
        self.push(LogicalOp::Explain {
            inner: Box::new(inner_plan),
            output_schema: self.schema(),
            origin: span,
        });
        Ok(())
    }

    pub(crate) fn push_union(
        &mut self,
        op: crate::SetOp,
        origin: SourceSpan,
    ) -> Result<(), PlannerError> {
        // Set-composition arms are bound independently; column name-equality
        // is enforced by the row adapter (SR v). The logical boundary records
        // the operator so every supported family reaches logical planning.
        let output_schema = self.schema();
        self.push(LogicalOp::Union {
            op,
            output_schema,
            origin,
        });
        Ok(())
    }

    pub(crate) fn push_chain(
        &mut self,
        correlated: bool,
        origin: SourceSpan,
    ) -> Result<(), PlannerError> {
        let output_schema = self.schema();
        self.push(LogicalOp::Chain {
            correlated,
            output_schema,
            origin,
        });
        Ok(())
    }

    pub(crate) fn push_control(
        &mut self,
        kind: crate::plan::logical::descriptors::LogicalControlKind,
        span: SourceSpan,
    ) {
        self.push(LogicalOp::Control { kind, origin: span });
    }

    #[allow(dead_code, reason = "retained for transition created before F03-PR04")]
    pub(crate) fn note_catalog_or_explain(&mut self) -> Result<(), PlannerError> {
        Ok(())
    }

    #[allow(dead_code, reason = "retained for transition created before F03-PR04")]
    pub(crate) fn finish_control(&mut self, _span: SourceSpan) {}

    pub(crate) fn lower_scan_seed_in(&mut self, scope: SourceSpan) -> Result<(), PlannerError> {
        // Seed scans from semantic binding declarations of matchable kinds
        // declared inside the pipeline being lowered. Each UNION arm / NEXT
        // block seeds independently from its own span so no arm inherits a
        // sibling arm's bindings; only the first seed per isolated pipeline
        // is emitted.
        if self.operators.len() > self.pipeline_base {
            return Ok(());
        }
        let mut seeded = false;
        for decl in self.analyzed.scopes.declarations() {
            if !span_contains(scope, decl.span()) {
                continue;
            }
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
            // Only the outermost pipeline defines the plan-level input width;
            // isolated arms restore it after lowering (see the Composite /
            // Chained loops above), so a sibling seed must not clobber it.
            if self.pipeline_base == 0 {
                self.input_width = self.current_schema.len();
            }
            self.push(LogicalOp::Scan {
                descriptor,
                output_schema: self.schema(),
                scope: self.scope,
                origin: decl.span(),
            });
            seeded = true;
            break;
        }
        if !seeded && self.current_schema.is_empty() && self.pipeline_base == 0 {
            self.input_width = 0;
        }
        Ok(())
    }

    pub(crate) fn call_descriptor(
        &self,
        call: &crate::ProcedureCall,
    ) -> Result<LogicalCallDescriptor, PlannerError> {
        let name = &call.name;
        let span = call.span;
        let Some(resolved) = self.analyzed.calls.iter().find(|call| call.span() == span) else {
            return Err(PlannerError::ProcedureMetadataMismatch {
                procedure: name.clone().into_vec().into_boxed_slice(),
                detail: "call has no semantic application",
                span,
            });
        };
        // Effects resolve from current registration metadata, never from the
        // recorded semantic copy alone. A drift between analysis and lowering
        // must fail with row-adapter-identical diagnostics rather than
        // execute with stale authority.
        let Some(current) = self.registry.lookup(name) else {
            return Err(PlannerError::UnknownProcedure {
                procedure: name.clone().into_vec().into_boxed_slice(),
                span,
            });
        };
        super::lowering_call::validate_call_drift(call, &current, resolved, self.analyzed)?;
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

    pub(crate) fn yield_type(&self, name: &selene_core::DbString) -> Option<AnalyzedType> {
        self.analyzed
            .scopes
            .declarations()
            .iter()
            .find(|decl| decl.kind() == BindingDeclKind::YieldColumn && decl.name() == *name)
            .map(|decl| decl.ty().clone())
    }

    pub(crate) fn expr_id(
        &self,
        span: SourceSpan,
        expr: &crate::ValueExpr,
    ) -> Result<ExprId, PlannerError> {
        // A span-based fallback is forbidden: expression identity comes only
        // from the semantic lookup. A missing cell is a lowering error, not a
        // cue to re-parse syntax.
        self.analyzed
            .expr_ids
            .get(expr)
            .ok_or(PlannerError::ExpressionTypeMissing { span })
    }

    pub(crate) fn schema(&self) -> BindingTableSchema {
        BindingTableSchema {
            columns: self.current_schema.clone(),
        }
    }

    pub(crate) fn push(&mut self, op: LogicalOp) {
        self.operators.push(op);
    }

    pub(crate) fn finish(
        self,
        paths: crate::plan::logical::path::lowering::LoweredPathSet,
    ) -> LogicalPlan {
        let output_schema = self
            .operators
            .last()
            .map_or_else(|| self.schema(), |op| op.output_schema().clone());
        LogicalPlan {
            operators: self.operators,
            paths,
            effects: self.effects,
            output_schema,
            registry_version: self.analyzed.procedure_registry_version,
            input_width: self.input_width,
        }
    }
}

pub(crate) fn offset_span(value: &LimitValue) -> SourceSpan {
    match value {
        LimitValue::Count(_, span) | LimitValue::Parameter { span, .. } => *span,
    }
}

pub(crate) fn limit_span(value: &LimitValue) -> SourceSpan {
    match value {
        LimitValue::Count(_, span) | LimitValue::Parameter { span, .. } => *span,
    }
}

fn span_contains(outer: SourceSpan, inner: SourceSpan) -> bool {
    outer.byte_offset <= inner.byte_offset && inner.end() <= outer.end()
}

/// Lower one `EXPLAIN` inner statement shape with the outer semantics.
///
/// The inner statement shares the outer analyzed tree (the analyzer binds the
/// whole `EXPLAIN <stmt>` as one statement). Only ordering and spans come
/// from `inner`; every identity, type, and effect comes from `analyzed`.
pub(crate) fn lower_inner_statement(
    analyzed: &AnalyzedStatement,
    registry: &dyn ProcedureRegistry,
    inner: &Statement,
) -> Result<LogicalPlan, PlannerError> {
    use crate::plan::logical::{effect::classify_analyzed, path::lowering::LoweredPathSet};

    let effects = classify_analyzed(analyzed);
    let mut builder = LogicalBuilder::new(analyzed, registry, effects);
    match inner {
        Statement::Query(pipeline) => {
            builder.lower_query_pipeline(pipeline)?;
        }
        Statement::Composite { first, rest, .. } => {
            builder.lower_query_pipeline(first)?;
            let left_schema = builder.schema();
            let outer_input_width = builder.input_width;
            for (op, rhs) in rest {
                let span = rhs.span;
                builder.current_schema = Vec::new();
                builder.pipeline_base = builder.operators.len();
                builder.lower_query_pipeline(rhs)?;
                builder.current_schema = left_schema.columns.clone();
                builder.push_union(*op, span)?;
                builder.pipeline_base = builder.operators.len();
                builder.input_width = outer_input_width;
            }
        }
        Statement::Chained { blocks, .. } => {
            if let Some((first, rest)) = blocks.split_first() {
                builder.lower_query_pipeline(first)?;
                let outer_input_width = builder.input_width;
                for block in rest {
                    let correlated =
                        super::lowering_query::block_is_correlated(block.span, analyzed);
                    let span = block.span;
                    builder.current_schema = Vec::new();
                    builder.pipeline_base = builder.operators.len();
                    builder.lower_query_pipeline(block)?;
                    builder.push_chain(correlated, span)?;
                    builder.pipeline_base = builder.operators.len();
                    builder.input_width = outer_input_width;
                }
            }
        }
        Statement::Mutate(pipeline) => {
            builder.lower_mutation_pipeline(pipeline)?;
        }
        Statement::Call(call) => {
            builder.lower_top_level_call(call)?;
        }
        Statement::Ddl(statement) => {
            builder.lower_catalog(statement)?;
        }
        Statement::Explain { inner, span } => {
            // Nested EXPLAIN is rejected by the analyzer; this arm is a
            // lowering backstop that preserves the outer span.
            builder.lower_explain(inner, *span, registry)?;
        }
        Statement::StartTransaction { span }
        | Statement::Commit { span }
        | Statement::Rollback { span } => {
            // Control statements inside EXPLAIN are still control effects;
            // the outer EXPLAIN wrapper ensures they never execute.
            builder.push_control(
                match inner {
                    Statement::StartTransaction { .. } => {
                        super::descriptors::LogicalControlKind::StartTransaction
                    }
                    Statement::Commit { .. } => super::descriptors::LogicalControlKind::Commit,
                    _ => super::descriptors::LogicalControlKind::Rollback,
                },
                *span,
            );
        }
        Statement::SessionSetValue { span, .. } => {
            builder.push_control(
                super::descriptors::LogicalControlKind::SessionSetValue,
                *span,
            );
        }
        Statement::SessionSetTimeZone { span, .. } => {
            builder.push_control(
                super::descriptors::LogicalControlKind::SessionSetTimeZone,
                *span,
            );
        }
        Statement::SessionSetGraph { span, .. } => {
            builder.push_control(
                super::descriptors::LogicalControlKind::SessionSetGraph,
                *span,
            );
        }
        Statement::SessionReset { span, .. } => {
            builder.push_control(super::descriptors::LogicalControlKind::SessionReset, *span);
        }
        Statement::SessionClose { span } => {
            builder.push_control(super::descriptors::LogicalControlKind::SessionClose, *span);
        }
    }
    Ok(builder.finish(LoweredPathSet::empty()))
}

pub use super::lowering_cost::measure_lowering_cost;
