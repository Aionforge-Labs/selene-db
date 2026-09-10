//! Procedure-call lowering.

use std::collections::HashSet;

use crate::{
    GqlType, Literal, ProcedureCall, ProcedureDefaultValue, ProcedureMetadata, ProcedureMutability,
    ProcedureOutputColumn, ProcedureRegistry, YieldColumn,
    analyze::{
        AnalyzedStatement, AnalyzedType, BindingDecl, BindingDeclKind, StatementCategory, infer,
    },
    plan::{
        BindingTableColumn, BindingTableSchema, ExecutionPlan, ImplDefinedCaps, PipelineOp,
        PlannedCall, PlannedYieldItem, PlannerError, ProjectExpr, YieldKind,
    },
};

use super::expr;

/// Lower a top-level CALL statement.
pub(crate) fn lower_top_level_call(
    call: &ProcedureCall,
    registry: &dyn ProcedureRegistry,
    analyzed: &AnalyzedStatement,
) -> Result<ExecutionPlan, PlannerError> {
    let planned = plan_call(call, registry, analyzed)?;
    if matches!(analyzed.source(), crate::Statement::Call(_)) {
        // Why: in-pipeline CALLs live inside query statements whose
        // analyzed.category describes the whole pipeline; only top-level CALL
        // has a category that is exactly the call's mutability classification.
        if classify_mutability(planned.mutability) != analyzed.category {
            return Err(PlannerError::ProcedureMetadataMismatch {
                procedure: planned.procedure.clone(),
                detail: "mutability classification changed",
                span: planned.span,
            });
        }
    }
    let columns = planned.yield_schema.clone();
    let pipeline = vec![PipelineOp::Call(planned)];
    let next_pipeline_op_id = crate::PipelineOpId::new(pipeline.len() as u32);
    Ok(ExecutionPlan {
        category: analyzed.category,
        pattern_plan: None,
        pipeline,
        output_schema: BindingTableSchema { columns },
        impl_defined_caps: ImplDefinedCaps::default(),
        expr_ids: analyzed.expr_ids.clone(),
        subqueries: Default::default(),
        next_expr_id: super::next_expr_id(analyzed),
        next_pipeline_op_id,
    })
}

/// Lower a procedure call into a planned call payload.
pub(crate) fn plan_call(
    call: &ProcedureCall,
    registry: &dyn ProcedureRegistry,
    analyzed: &AnalyzedStatement,
) -> Result<PlannedCall, PlannerError> {
    let metadata = registry
        .lookup(&call.name)
        .ok_or_else(|| PlannerError::UnknownProcedure {
            procedure: call.name.clone().into_vec().into_boxed_slice(),
            span: call.span,
        })?;

    let resolved = analyzed
        .calls
        .iter()
        .find(|resolved| resolved.span() == call.span)
        .ok_or_else(|| PlannerError::ProcedureMetadataMismatch {
            procedure: call.name.clone().into_vec().into_boxed_slice(),
            detail: "call has no semantic application",
            span: call.span,
        })?;
    if metadata.signature.parameters.len() != call.args.len() + resolved.defaults().len() {
        return Err(PlannerError::ProcedureMetadataMismatch {
            procedure: call.name.clone().into_vec().into_boxed_slice(),
            detail: "signature parameter count changed",
            span: call.span,
        });
    }

    let args = call
        .args
        .iter()
        .chain(resolved.defaults())
        .map(|arg| expr::project_expr(arg, None, analyzed))
        .collect::<Result<Vec<_>, _>>()?;
    validate_signature(call, &metadata, &args, analyzed)?;

    let yield_cols = call
        .yield_items
        .iter()
        .map(|item| PlannedYieldItem {
            column: match &item.column {
                YieldColumn::Star => YieldKind::Star,
                YieldColumn::Named(name) => YieldKind::Named(name.clone()),
            },
            alias: item.alias.clone(),
            span: item.span,
        })
        .collect();

    validate_output_schema(call, &metadata, analyzed)?;
    let changed = if metadata.mutability != resolved.metadata().mutability {
        Some("mutability classification changed")
    } else if metadata.tier != resolved.metadata().tier {
        Some("procedure tier changed")
    } else if metadata.handle != resolved.metadata().handle {
        Some("procedure handle changed")
    } else if !resolved.same_signature(&metadata) {
        Some("resolved procedure signature changed")
    } else {
        None
    };
    let mut planned = PlannedCall {
        optional: call.optional,
        procedure: call.name.clone().into_vec().into_boxed_slice(),
        handle: metadata.handle,
        args,
        yield_cols,
        output_schema: metadata.output_schema,
        yield_schema: Vec::new(),
        tier: metadata.tier,
        mutability: metadata.mutability,
        span: call.span,
    };
    planned.yield_schema = yield_to_columns(&planned)?;
    // Preserve specific wildcard/column diagnostics before the complete
    // dependency check. No changed metadata is allowed to escape this adapter.
    if let Some(detail) = changed {
        return Err(PlannerError::ProcedureMetadataMismatch {
            procedure: call.name.clone().into_vec().into_boxed_slice(),
            detail,
            span: call.span,
        });
    }
    Ok(planned)
}

/// Convert planned yield items into binding-table columns.
pub(crate) fn yield_to_columns(
    planned: &PlannedCall,
) -> Result<Vec<BindingTableColumn>, PlannerError> {
    if planned.yield_cols.is_empty() {
        return Ok(Vec::new());
    }

    let mut columns = Vec::new();
    let mut seen = HashSet::new();
    let wildcard_span = planned
        .yield_cols
        .iter()
        .find(|item| matches!(item.column, YieldKind::Star))
        .map(|item| item.span);

    if let Some(star_span) = wildcard_span {
        for col in &planned.output_schema.columns {
            push_binding_column(
                planned,
                &mut columns,
                &mut seen,
                col,
                col.name.clone(),
                star_span,
            )?;
        }
    }

    for item in &planned.yield_cols {
        let YieldKind::Named(ref name) = item.column else {
            continue;
        };
        let col = planned
            .output_schema
            .columns
            .iter()
            .find(|candidate| candidate.name == *name)
            .ok_or_else(|| PlannerError::ProcedureMetadataMismatch {
                procedure: planned.procedure.clone(),
                detail: "yield column not in registry output schema",
                span: item.span,
            })?;
        push_binding_column(
            planned,
            &mut columns,
            &mut seen,
            col,
            item.alias.clone().unwrap_or(name.clone()),
            item.span,
        )?;
    }

    Ok(columns)
}

fn validate_signature(
    call: &ProcedureCall,
    metadata: &ProcedureMetadata,
    args: &[ProjectExpr],
    analyzed: &AnalyzedStatement,
) -> Result<(), PlannerError> {
    for (arg, parameter) in args.iter().zip(&metadata.signature.parameters) {
        if arg.span == call.span && !default_matches_project_expr(parameter.default, &arg.expr) {
            return Err(PlannerError::ProcedureMetadataMismatch {
                procedure: call.name.clone().into_vec().into_boxed_slice(),
                detail: "signature parameter default changed",
                span: arg.span,
            });
        }
        let arg_ty = analyzed.expr_types.get(arg.expr_id);
        if let AnalyzedType::Resolved(found) = arg_ty
            && !infer::argument_assignable(found, &parameter.ty, parameter.nullable)
        {
            let detail = if matches!(found, GqlType::Null) {
                "signature parameter nullability changed"
            } else {
                "signature parameter type changed"
            };
            return Err(PlannerError::ProcedureMetadataMismatch {
                procedure: call.name.clone().into_vec().into_boxed_slice(),
                detail,
                span: arg.span,
            });
        }
    }
    Ok(())
}

fn default_matches_project_expr(
    default: Option<ProcedureDefaultValue>,
    expr: &crate::ValueExpr,
) -> bool {
    let Some(default) = default else {
        return false;
    };
    let crate::ValueExpr::Literal(literal) = expr else {
        return false;
    };
    match (default, literal) {
        (ProcedureDefaultValue::Boolean(expected), Literal::Bool(found, _)) => expected == *found,
        (ProcedureDefaultValue::Null, Literal::Null(_)) => true,
        (
            ProcedureDefaultValue::Integer(expected),
            Literal::Integer(found, _) | Literal::RadixInteger(found, _, _),
        ) => expected == *found,
        (ProcedureDefaultValue::String(expected), Literal::String(found, _, _)) => {
            expected == found.as_str()
        }
        _ => false,
    }
}

fn validate_output_schema(
    call: &ProcedureCall,
    metadata: &ProcedureMetadata,
    analyzed: &AnalyzedStatement,
) -> Result<(), PlannerError> {
    for decl in analyzed.scopes.declarations().iter().filter(|decl| {
        decl.kind() == BindingDeclKind::YieldColumn && span_inside(decl.span(), call.span)
    }) {
        let Some(expected_ty) = expected_yield_type_for_decl(call, metadata, decl) else {
            return Err(output_schema_changed(call, decl.span()));
        };
        if decl.ty() != &AnalyzedType::Resolved(expected_ty) {
            return Err(output_schema_changed(call, decl.span()));
        }
    }
    Ok(())
}

fn expected_yield_type_for_decl(
    call: &ProcedureCall,
    metadata: &ProcedureMetadata,
    decl: &BindingDecl,
) -> Option<GqlType> {
    for item in &call.yield_items {
        if item.span != decl.span() {
            continue;
        }
        let YieldColumn::Named(ref source_name) = item.column else {
            continue;
        };
        if item.alias.clone().unwrap_or(source_name.clone()) == decl.name() {
            return metadata
                .output_schema
                .columns
                .iter()
                .find(|candidate| candidate.name == *source_name)
                .map(|col| nullable_yield_gql_type(col.ty.clone(), call.optional));
        }
    }

    if call
        .yield_items
        .iter()
        .any(|item| item.span == decl.span() && matches!(item.column, YieldColumn::Star))
    {
        return metadata
            .output_schema
            .columns
            .iter()
            .find(|candidate| candidate.name == decl.name())
            .map(|col| nullable_yield_gql_type(col.ty.clone(), call.optional));
    }

    None
}

fn output_schema_changed(call: &ProcedureCall, span: crate::SourceSpan) -> PlannerError {
    PlannerError::ProcedureMetadataMismatch {
        procedure: call.name.clone().into_vec().into_boxed_slice(),
        detail: "output column type changed",
        span,
    }
}

fn nullable_yield_gql_type(ty: GqlType, optional: bool) -> GqlType {
    if !optional {
        return ty;
    }
    match ty {
        GqlType::NotNull(inner) => *inner,
        other => other,
    }
}

fn span_inside(inner: crate::SourceSpan, outer: crate::SourceSpan) -> bool {
    inner.byte_offset >= outer.byte_offset && inner.end() <= outer.end()
}

const fn classify_mutability(mutability: ProcedureMutability) -> StatementCategory {
    match mutability {
        ProcedureMutability::Read => StatementCategory::ReadOnly,
        ProcedureMutability::SchemaWrite => StatementCategory::CatalogModifying,
        ProcedureMutability::MaintenanceWrite => StatementCategory::Maintenance,
    }
}

fn push_binding_column(
    planned: &PlannedCall,
    columns: &mut Vec<BindingTableColumn>,
    seen: &mut HashSet<selene_core::DbString>,
    col: &ProcedureOutputColumn,
    name: selene_core::DbString,
    span: crate::SourceSpan,
) -> Result<(), PlannerError> {
    if !seen.insert(name.clone()) {
        return Err(PlannerError::ProcedureMetadataMismatch {
            procedure: planned.procedure.clone(),
            detail: "duplicate yield column after wildcard",
            span,
        });
    }
    columns.push(binding_column(col, name, planned.optional));
    Ok(())
}

fn binding_column(
    col: &ProcedureOutputColumn,
    name: selene_core::DbString,
    optional: bool,
) -> BindingTableColumn {
    BindingTableColumn {
        name: Some(name),
        hidden: None,
        ty: super::nullable_call_yield_type(AnalyzedType::Resolved(col.ty.clone()), optional),
    }
}

#[cfg(test)]
mod defensive_tests {
    use super::*;
    use crate::{
        ProcedureDefaultValue, ProcedureHandle, ProcedureOutputSchema, ProcedureParameter,
        ProcedureSignature, ProcedureTier, SourceSpan,
        procedure_registry::{ProcedureError, ProcedureResult, Value},
    };

    #[derive(Clone)]
    struct StaticRegistry {
        metadata: ProcedureMetadata,
    }

    impl ProcedureRegistry for StaticRegistry {
        fn lookup(&self, _name: &[selene_core::DbString]) -> Option<ProcedureMetadata> {
            Some(self.metadata.clone())
        }

        fn execute(
            &self,
            _handle: ProcedureHandle,
            _args: &[Value],
            _ctx: &mut crate::ProcedureContext<'_, '_>,
        ) -> Result<ProcedureResult, ProcedureError> {
            Ok(ProcedureResult { rows: Vec::new() })
        }
    }

    fn db_string(value: &str) -> selene_core::DbString {
        selene_core::db_string(value).expect("test string fits DB string cap")
    }

    fn param(name: &str, ty: GqlType, nullable: bool) -> ProcedureParameter {
        ProcedureParameter::new(db_string(name), ty, nullable)
    }

    fn output(name: &str, ty: GqlType) -> ProcedureOutputColumn {
        ProcedureOutputColumn::new(db_string(name), ty)
    }

    fn registry(
        parameters: Vec<ProcedureParameter>,
        columns: Vec<ProcedureOutputColumn>,
        mutability: ProcedureMutability,
    ) -> StaticRegistry {
        StaticRegistry {
            metadata: ProcedureMetadata::new(
                ProcedureHandle::new(1),
                ProcedureSignature::new(parameters),
                ProcedureOutputSchema { columns },
                ProcedureTier::Graph,
                mutability,
            ),
        }
    }

    fn analyzed_with(source: &str, registry: &StaticRegistry) -> AnalyzedStatement {
        let statement = crate::parser::parse(source).expect("test input parses");
        crate::analyze::analyze(statement, registry, None).expect("test input analyzes")
    }

    fn assert_metadata_detail(err: PlannerError, expected: &'static str) {
        let PlannerError::ProcedureMetadataMismatch { detail, .. } = err else {
            panic!("expected metadata mismatch, got {err:?}");
        };
        assert_eq!(detail, expected);
    }

    #[test]
    fn changed_parameter_type_reports_metadata_mismatch() {
        let source = "CALL pkg.arg(1)";
        let original = registry(
            vec![param("value", GqlType::Integer, false)],
            Vec::new(),
            ProcedureMutability::Read,
        );
        let analyzed = analyzed_with(source, &original);
        let changed = registry(
            vec![param("value", GqlType::String, false)],
            Vec::new(),
            ProcedureMutability::Read,
        );

        let err =
            crate::plan::plan(&analyzed, &changed).expect_err("signature type drift is rejected");
        assert_metadata_detail(err, "signature parameter type changed");
    }

    #[test]
    fn changed_parameter_nullability_reports_metadata_mismatch() {
        let source = "CALL pkg.arg(NULL)";
        let original = registry(
            vec![param("value", GqlType::Integer, true)],
            Vec::new(),
            ProcedureMutability::Read,
        );
        let analyzed = analyzed_with(source, &original);
        let changed = registry(
            vec![param("value", GqlType::Integer, false)],
            Vec::new(),
            ProcedureMutability::Read,
        );

        let err = crate::plan::plan(&analyzed, &changed)
            .expect_err("signature nullability drift is rejected");
        assert_metadata_detail(err, "signature parameter nullability changed");
    }

    #[test]
    fn changed_parameter_default_reports_metadata_mismatch() {
        let source = "CALL pkg.arg()";
        let original = registry(
            vec![
                param("enabled", GqlType::Boolean, false)
                    .with_default(ProcedureDefaultValue::Boolean(false)),
            ],
            Vec::new(),
            ProcedureMutability::Read,
        );
        let analyzed = analyzed_with(source, &original);
        let changed = registry(
            vec![
                param("enabled", GqlType::Boolean, false)
                    .with_default(ProcedureDefaultValue::Boolean(true)),
            ],
            Vec::new(),
            ProcedureMutability::Read,
        );

        let err = crate::plan::plan(&analyzed, &changed).expect_err("default drift is rejected");
        assert_metadata_detail(err, "signature parameter default changed");
    }

    #[test]
    fn changed_top_level_mutability_reports_metadata_mismatch() {
        let source = "CALL pkg.work()";
        let original = registry(Vec::new(), Vec::new(), ProcedureMutability::Read);
        let analyzed = analyzed_with(source, &original);
        let changed = registry(Vec::new(), Vec::new(), ProcedureMutability::SchemaWrite);

        let err = crate::plan::plan(&analyzed, &changed).expect_err("mutability drift is rejected");
        assert_metadata_detail(err, "mutability classification changed");
    }

    #[test]
    fn changed_output_column_type_reports_metadata_mismatch() {
        let source = "CALL pkg.out() YIELD out";
        let original = registry(
            Vec::new(),
            vec![output("out", GqlType::String)],
            ProcedureMutability::Read,
        );
        let analyzed = analyzed_with(source, &original);
        let changed = registry(
            Vec::new(),
            vec![output("out", GqlType::Integer)],
            ProcedureMutability::Read,
        );

        let err = crate::plan::plan(&analyzed, &changed)
            .expect_err("output column type drift is rejected");
        assert_metadata_detail(err, "output column type changed");
    }

    #[test]
    fn duplicate_wildcard_output_reports_metadata_mismatch() {
        let source = "CALL pkg.out() YIELD *, outA AS first";
        let original = registry(
            Vec::new(),
            vec![
                output("outA", GqlType::String),
                output("outB", GqlType::Integer),
            ],
            ProcedureMutability::Read,
        );
        let analyzed = analyzed_with(source, &original);
        let changed = registry(
            Vec::new(),
            vec![
                output("outA", GqlType::String),
                output("outB", GqlType::Integer),
                output("first", GqlType::String),
            ],
            ProcedureMutability::Read,
        );

        let err = crate::plan::plan(&analyzed, &changed)
            .expect_err("wildcard output name drift is rejected");
        assert_metadata_detail(err, "duplicate yield column after wildcard");
    }

    #[test]
    fn changed_yield_schema_reports_metadata_mismatch() {
        let name = selene_core::db_string("pkg").expect("test string fits DB string cap");
        let col = selene_core::db_string("out").expect("test string fits DB string cap");
        let planned = PlannedCall {
            optional: false,
            procedure: Box::new([name.clone()]),
            handle: ProcedureHandle::new(1),
            args: Vec::new(),
            yield_cols: vec![PlannedYieldItem {
                column: YieldKind::Named(col),
                alias: None,
                span: SourceSpan::new(0, 3),
            }],
            output_schema: registry(
                Vec::new(),
                vec![output("different", GqlType::String)],
                ProcedureMutability::Read,
            )
            .lookup(std::slice::from_ref(&name))
            .expect("metadata")
            .output_schema,
            yield_schema: Vec::new(),
            tier: ProcedureTier::Graph,
            mutability: ProcedureMutability::Read,
            span: SourceSpan::new(0, 3),
        };

        let err = yield_to_columns(&planned).expect_err("metadata changed");
        assert!(matches!(
            err,
            PlannerError::ProcedureMetadataMismatch {
                detail: "yield column not in registry output schema",
                ..
            }
        ));
    }
}
