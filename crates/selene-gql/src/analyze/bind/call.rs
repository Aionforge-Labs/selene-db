//! Procedure-call bind handling.

use selene_core::{DbString, db_string};

use super::{BindContext, expr};
use crate::{
    GqlType, Literal, ProcedureCall, ProcedureDefaultValue, ProcedureMetadata,
    ProcedureOutputColumn, ValueExpr, YieldColumn,
    analyze::{
        binding::BindingDeclKind,
        error::{AnalysisError, ExpectedType, TypeMismatchContext},
        infer,
        types::AnalyzedType,
    },
    ast::CharacterStringLiteralKind,
};

pub(crate) fn bind_procedure_call(
    ctx: &mut BindContext,
    call: &ProcedureCall,
) -> Result<(), AnalysisError> {
    let metadata = lookup_metadata(ctx, call)?;
    bind_procedure_call_with_metadata(ctx, call, metadata)
}

pub(crate) fn lookup_metadata(
    ctx: &BindContext,
    call: &ProcedureCall,
) -> Result<ProcedureMetadata, AnalysisError> {
    ctx.registry()
        .lookup(&call.name)
        .ok_or_else(|| AnalysisError::UnknownProcedure {
            name: procedure_name(call),
            span: call.span,
        })
}

/// Build the boxed name slice used by the error variants that name the
/// offending procedure. Computed lazily at each error-return site so the
/// happy path allocates nothing.
fn procedure_name(call: &ProcedureCall) -> Box<[DbString]> {
    call.name.clone().into_vec().into_boxed_slice()
}

pub(crate) fn bind_procedure_call_with_metadata(
    ctx: &mut BindContext,
    call: &ProcedureCall,
    metadata: ProcedureMetadata,
) -> Result<(), AnalysisError> {
    let arity = metadata.signature.arity();
    ctx.use_working_graph(call.span)?;
    let actual = call.args.len();
    if !arity.accepts(actual) {
        return Err(AnalysisError::WrongArgumentCount {
            procedure: procedure_name(call),
            expected: arity.maximum,
            minimum: arity.minimum,
            actual,
            span: call.span,
        });
    }
    let defaults = missing_defaults(call, &metadata)?;

    let mut arg_types = Vec::with_capacity(call.args.len());
    for arg in call.args.iter().chain(&defaults) {
        let id = expr::bind_value_expr(ctx, arg)?;
        arg_types.push((ctx.expr_type(id).clone(), arg.span()));
    }

    for ((arg_ty, span), (position, parameter)) in arg_types
        .iter()
        .zip(metadata.signature.parameters.iter().enumerate())
    {
        // Dynamic operands have no static type to compare; runtime validation
        // remains responsible for those values.
        if let AnalyzedType::Resolved(found) = arg_ty
            && !infer::argument_assignable(found, &parameter.ty, parameter.nullable)
        {
            return Err(AnalysisError::TypeMismatch {
                context: TypeMismatchContext::ProcedureArgument {
                    procedure: procedure_name(call),
                    parameter: parameter.name.clone(),
                    position,
                },
                expected: ExpectedType::Specific(parameter.ty.clone()),
                found: found.clone(),
                span: *span,
            });
        }
    }

    // `YIELD *` is a Selene extension. Expand wildcard columns first in the
    // registered schema order, then process explicit named items in source
    // order. Duplicate output names naturally fail through strict declaration.
    if let Some(star_span) = call
        .yield_items
        .iter()
        .find(|item| matches!(item.column, YieldColumn::Star))
        .map(|item| item.span)
    {
        for column in &metadata.output_schema.columns {
            declare_output(ctx, column, column.name.clone(), star_span, call.optional)?;
        }
    }

    for item in &call.yield_items {
        if let YieldColumn::Named(column) = &item.column {
            let Some(output) = metadata
                .output_schema
                .columns
                .iter()
                .find(|candidate| candidate.name == *column)
            else {
                return Err(AnalysisError::UnknownYieldColumn {
                    procedure: procedure_name(call),
                    column: column.clone(),
                    span: item.span,
                });
            };
            let name = item.alias.clone().unwrap_or_else(|| column.clone());
            declare_output(ctx, output, name, item.span, call.optional)?;
        }
    }
    ctx.calls.push(crate::analyze::semantic::ResolvedCall {
        span: call.span,
        metadata,
        defaults,
    });
    Ok(())
}

fn missing_defaults(
    call: &ProcedureCall,
    metadata: &ProcedureMetadata,
) -> Result<Vec<ValueExpr>, AnalysisError> {
    let provided = call.args.len();
    let expected = metadata.signature.parameters.len();
    let mut defaults = Vec::with_capacity(expected.saturating_sub(provided));
    for parameter in metadata.signature.parameters.iter().skip(provided) {
        let Some(default) = parameter.default else {
            return Err(AnalysisError::WrongArgumentCount {
                procedure: procedure_name(call),
                expected,
                minimum: expected,
                actual: provided,
                span: call.span,
            });
        };
        defaults.push(default_expr(default, call.span)?);
    }
    Ok(defaults)
}

fn default_expr(
    default: ProcedureDefaultValue,
    span: crate::SourceSpan,
) -> Result<ValueExpr, AnalysisError> {
    let literal = match default {
        ProcedureDefaultValue::Boolean(value) => Literal::Bool(value, span),
        ProcedureDefaultValue::Null => Literal::Null(span),
        ProcedureDefaultValue::Integer(value) => Literal::Integer(value, span),
        ProcedureDefaultValue::String(value) => {
            let default_value = db_string(value).map_err(|_| AnalysisError::NotImplemented {
                message: "procedure default string exceeds the maximum DB string byte length"
                    .to_owned(),
                span,
                hint: None,
            })?;
            Literal::String(default_value, span, CharacterStringLiteralKind::Escaped)
        }
    };
    Ok(ValueExpr::Literal(literal))
}

fn declare_output(
    ctx: &mut BindContext,
    column: &ProcedureOutputColumn,
    name: DbString,
    span: crate::SourceSpan,
    optional: bool,
) -> Result<(), AnalysisError> {
    ctx.declare_strict_typed(
        BindingDeclKind::YieldColumn,
        name,
        span,
        nullable_call_yield_type(AnalyzedType::Resolved(column.ty.clone()), optional),
    )?;
    Ok(())
}

pub(super) fn nullable_call_yield_type(ty: AnalyzedType, optional: bool) -> AnalyzedType {
    if !optional {
        return ty;
    }
    match ty {
        AnalyzedType::Resolved(ty) => AnalyzedType::Resolved(nullable_gql_type(ty)),
        AnalyzedType::Dynamic => AnalyzedType::Dynamic,
    }
}

fn nullable_gql_type(ty: GqlType) -> GqlType {
    match ty {
        GqlType::NotNull(inner) => *inner,
        other => other,
    }
}
