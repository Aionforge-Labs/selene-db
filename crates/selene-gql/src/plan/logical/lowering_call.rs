//! Procedure-call and table-subquery lowering for logical plans.
//!
//! Imports resolve to outer binding identities (GP03 explicit variable
//! scope); bodies lower in an isolated child builder so parent columns stay
//! intact. Registry-drift diagnostics mirror the row adapter exactly so the
//! single-path gate preserves primary/additional statuses.

use crate::{
    GqlType, Literal, ProcedureCall, ProcedureDefaultValue, ProcedureMetadata, SourceSpan,
    analyze::{AnalyzedStatement, AnalyzedType, BindingDecl, BindingDeclKind, BindingId},
    plan::{BindingTableColumn, PlannerError, logical::lowering::LogicalBuilder},
};

/// Lower one inline `CALL { ... }` subquery is implemented on the builder in
/// `lowering_query`; this module owns the isolated body and yield helpers.
pub(crate) fn lower_subquery_body(
    analyzed: &AnalyzedStatement,
    registry: &dyn crate::ProcedureRegistry,
    body: &crate::QueryPipeline,
    _imports: &[BindingId],
) -> Result<crate::plan::logical::operator::LogicalPlan, PlannerError> {
    use crate::plan::logical::{effect::classify_analyzed, path::lowering::LoweredPathSet};

    let effects = classify_analyzed(analyzed);
    let mut builder = LogicalBuilder::new(analyzed, registry, effects);
    builder.lower_query_pipeline(body)?;
    let paths = LoweredPathSet::empty();
    Ok(builder.finish(paths))
}

/// Yield schema for one subquery from its body's output columns.
pub(crate) fn subquery_yield_schema(
    call: &crate::InlineProcedureCall,
    body: &crate::plan::logical::operator::LogicalPlan,
    analyzed: &AnalyzedStatement,
) -> Result<Vec<BindingTableColumn>, PlannerError> {
    let mut schema = Vec::new();
    for item in &call.yield_items {
        match &item.column {
            crate::YieldColumn::Star => {
                for column in &body.output_schema.columns {
                    if let Some(name) = &column.name {
                        schema.push(BindingTableColumn {
                            name: Some(name.clone()),
                            hidden: None,
                            ty: column.ty.clone(),
                        });
                    }
                }
            }
            crate::YieldColumn::Named(source) => {
                let column = body
                    .output_schema
                    .columns
                    .iter()
                    .find(|column| column.name.as_ref() == Some(source))
                    .ok_or(PlannerError::ProcedureMetadataMismatch {
                        procedure: Box::new([]),
                        detail: "CALL subquery yield column missing from body output schema",
                        span: item.span,
                    })?;
                let ty = analyzed
                    .scopes
                    .declarations()
                    .iter()
                    .find(|decl| {
                        decl.kind() == crate::analyze::BindingDeclKind::ProjectionAlias
                            && decl.name() == *source
                    })
                    .map(|decl| decl.ty().clone())
                    .unwrap_or_else(|| column.ty.clone());
                let output = item.alias.clone().unwrap_or_else(|| source.clone());
                schema.push(BindingTableColumn {
                    name: Some(output),
                    hidden: None,
                    ty,
                });
            }
        }
    }
    Ok(schema)
}

/// Resolve GP03 imports for one subquery.
///
/// An explicit list (`CALL (a, b)`) imports exactly those names; an absent
/// list is the legacy correlated form whose imports are the body's outer
/// uses. Unknown names are an analyzer error (42N03); a missing identity
/// here is a lowering backstop.
pub(crate) fn subquery_imports(
    call: &crate::InlineProcedureCall,
    analyzed: &AnalyzedStatement,
) -> Result<Vec<BindingId>, PlannerError> {
    if let Some(names) = &call.variable_scope {
        let mut ids = Vec::with_capacity(names.len());
        for name in names {
            let Some(binding) = super::lowering_scan::resolve_binding(name, analyzed) else {
                return Err(crate::plan::PlannerError::BindingResolutionLost {
                    binding: crate::analyze::BindingId::new(u32::MAX),
                    span: call.span,
                });
            };
            ids.push(binding);
        }
        Ok(ids)
    } else {
        Ok(super::lowering_scan::outer_refs_in_span(
            call.body.span,
            analyzed,
        ))
    }
}

/// Suppress dead-code warnings for the span helper until EXPLAIN wiring lands.
#[allow(dead_code)]
pub(crate) fn call_span(span: SourceSpan) -> SourceSpan {
    span
}

/// Validate registry drift with row-adapter-identical diagnostics.
///
/// Checks parameter count, per-argument type/nullability/default, output
/// schema, mutability, tier, handle, and signature in the same order as the
/// row adapter so `ProcedureMetadataMismatch` details (and their GQLSTATUS)
/// survive the single-path gate.
pub(crate) fn validate_call_drift(
    call: &ProcedureCall,
    current: &ProcedureMetadata,
    resolved: &crate::analyze::semantic::ResolvedCall,
    analyzed: &AnalyzedStatement,
) -> Result<(), PlannerError> {
    if current.signature.parameters.len() != call.args.len() + resolved.defaults().len() {
        return Err(PlannerError::ProcedureMetadataMismatch {
            procedure: call.name.clone().into_vec().into_boxed_slice(),
            detail: "signature parameter count changed",
            span: call.span,
        });
    }
    validate_call_signature(call, current, resolved, analyzed)?;
    validate_call_output_schema(call, current, analyzed)?;
    validate_yield_duplicates(call, current)?;
    if current.mutability != resolved.metadata().mutability {
        return Err(PlannerError::ProcedureMetadataMismatch {
            procedure: call.name.clone().into_vec().into_boxed_slice(),
            detail: "mutability classification changed",
            span: call.span,
        });
    }
    if current.tier != resolved.metadata().tier {
        return Err(PlannerError::ProcedureMetadataMismatch {
            procedure: call.name.clone().into_vec().into_boxed_slice(),
            detail: "procedure tier changed",
            span: call.span,
        });
    }
    if current.handle != resolved.metadata().handle {
        return Err(PlannerError::ProcedureMetadataMismatch {
            procedure: call.name.clone().into_vec().into_boxed_slice(),
            detail: "procedure handle changed",
            span: call.span,
        });
    }
    if !resolved.same_signature(current) {
        return Err(PlannerError::ProcedureMetadataMismatch {
            procedure: call.name.clone().into_vec().into_boxed_slice(),
            detail: "resolved procedure signature changed",
            span: call.span,
        });
    }
    Ok(())
}

fn validate_call_signature(
    call: &ProcedureCall,
    current: &ProcedureMetadata,
    resolved: &crate::analyze::semantic::ResolvedCall,
    analyzed: &AnalyzedStatement,
) -> Result<(), PlannerError> {
    for (arg, parameter) in call
        .args
        .iter()
        .chain(resolved.defaults())
        .zip(&current.signature.parameters)
    {
        if arg.span() == call.span && !default_matches(parameter.default, arg) {
            return Err(PlannerError::ProcedureMetadataMismatch {
                procedure: call.name.clone().into_vec().into_boxed_slice(),
                detail: "signature parameter default changed",
                span: arg.span(),
            });
        }
        let Some(expr_id) = analyzed.expr_ids.get(arg) else {
            return Err(PlannerError::ExpressionTypeMissing { span: arg.span() });
        };
        let arg_ty = analyzed.expr_types.get(expr_id);
        if let AnalyzedType::Resolved(found) = arg_ty
            && !crate::analyze::infer::argument_assignable(found, &parameter.ty, parameter.nullable)
        {
            let detail = if matches!(found, GqlType::Null) {
                "signature parameter nullability changed"
            } else {
                "signature parameter type changed"
            };
            return Err(PlannerError::ProcedureMetadataMismatch {
                procedure: call.name.clone().into_vec().into_boxed_slice(),
                detail,
                span: arg.span(),
            });
        }
    }
    Ok(())
}

fn default_matches(default: Option<ProcedureDefaultValue>, expr: &crate::ValueExpr) -> bool {
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

fn validate_call_output_schema(
    call: &ProcedureCall,
    current: &ProcedureMetadata,
    analyzed: &AnalyzedStatement,
) -> Result<(), PlannerError> {
    for decl in analyzed.scopes.declarations().iter().filter(|decl| {
        decl.kind() == BindingDeclKind::YieldColumn && span_inside(decl.span(), call.span)
    }) {
        let Some(expected_ty) = expected_yield_type_for_decl(call, current, decl) else {
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
        let crate::YieldColumn::Named(ref source_name) = item.column else {
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
        .any(|item| item.span == decl.span() && matches!(item.column, crate::YieldColumn::Star))
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

fn output_schema_changed(call: &ProcedureCall, span: SourceSpan) -> PlannerError {
    PlannerError::ProcedureMetadataMismatch {
        procedure: call.name.clone().into_vec().into_boxed_slice(),
        detail: "output column type changed",
        span,
    }
}

/// Reject duplicate yield columns after wildcard expansion with the row
/// adapter's diagnostic.
///
/// Runs before the generic signature check so the specific wildcard detail
/// (and its span) survives the single-path gate, matching the row adapter's
/// "specific diagnostics before complete dependency check" order.
fn validate_yield_duplicates(
    call: &ProcedureCall,
    current: &ProcedureMetadata,
) -> Result<(), PlannerError> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let has_star = call
        .yield_items
        .iter()
        .any(|item| matches!(item.column, crate::YieldColumn::Star));
    if has_star {
        for col in &current.output_schema.columns {
            if !seen.insert(col.name.clone()) {
                continue;
            }
        }
    }
    for item in &call.yield_items {
        let crate::YieldColumn::Named(ref name) = item.column else {
            continue;
        };
        if current
            .output_schema
            .columns
            .iter()
            .all(|candidate| candidate.name != *name)
        {
            return Err(PlannerError::ProcedureMetadataMismatch {
                procedure: call.name.clone().into_vec().into_boxed_slice(),
                detail: "yield column not in registry output schema",
                span: item.span,
            });
        }
        let output = item.alias.clone().unwrap_or_else(|| name.clone());
        if !seen.insert(output) {
            return Err(PlannerError::ProcedureMetadataMismatch {
                procedure: call.name.clone().into_vec().into_boxed_slice(),
                detail: "duplicate yield column after wildcard",
                span: item.span,
            });
        }
    }
    Ok(())
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

fn span_inside(inner: SourceSpan, outer: SourceSpan) -> bool {
    inner.byte_offset >= outer.byte_offset && inner.end() <= outer.end()
}
