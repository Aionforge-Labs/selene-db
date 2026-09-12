//! Shared call-boundary checks, before any native implementation receives authority.

use crate::{
    PlannedCall, ProcedureError,
    runtime::{ExecutorError, TxContext, value_type_match::value_matches_gql_type},
};
use selene_core::Value;

use super::context::{procedure_error, validate_call_tier};

pub(crate) fn validate_registration(
    call: &PlannedCall,
    ctx: &TxContext<'_, '_>,
) -> Result<(), ExecutorError> {
    validate_call_tier(call)?;
    let current = ctx.registry().lookup(&call.procedure).ok_or_else(|| {
        procedure_error(
            ProcedureError::UnknownProcedure {
                name: call.procedure.clone(),
            },
            call.span,
            ctx.deadline(),
        )
    })?;
    if ctx.registry().registry_version() != call.registry_version
        || current != call.metadata
        || current.handle != call.handle
        || current.tier != call.tier
        || current.mutability != call.mutability
        || current.output_schema != call.output_schema
    {
        return Err(procedure_error(
            ProcedureError::Internal {
                detail: "stale procedure registration; recompile the statement".into(),
            },
            call.span,
            ctx.deadline(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_arguments(call: &PlannedCall, args: &[Value]) -> Result<(), ExecutorError> {
    let parameters = &call.metadata.signature.parameters;
    let invalid =
        |detail| procedure_error(ProcedureError::InvalidArgument { detail }, call.span, None);
    if args.len() != parameters.len() {
        return Err(invalid(format!(
            "expected {} arguments after defaults, got {}",
            parameters.len(),
            args.len()
        )));
    }
    for (value, parameter) in args.iter().zip(parameters) {
        let valid = if matches!(value, Value::Null) {
            parameter.nullable
        } else {
            argument_matches(value, &parameter.ty)
        };
        if !valid {
            return Err(invalid(argument_detail(
                value,
                &parameter.ty,
                parameter.name.to_string(),
            )));
        }
    }
    Ok(())
}

fn argument_detail(value: &Value, ty: &crate::GqlType, path: String) -> String {
    if let (Value::List(values), crate::GqlType::List(element)) = (value, ty.strip_not_null()) {
        for (index, value) in values.iter().enumerate() {
            if !argument_matches(value, element) {
                return argument_detail(value, element, format!("{path}[{index}]"));
            }
        }
    }
    let expected = match ty.strip_not_null() {
        crate::GqlType::NodeRef => "NODE".to_owned(),
        crate::GqlType::EdgeRef => "EDGE".to_owned(),
        _ => crate::ast::format::format_gql_type(ty),
    };
    format!("{path} must be a {expected}")
}

fn argument_matches(value: &Value, ty: &crate::GqlType) -> bool {
    if value_matches_gql_type(value, ty) {
        return true;
    }
    if let (Value::List(values), crate::GqlType::List(element)) = (value, ty.strip_not_null()) {
        return values.iter().all(|value| argument_matches(value, element));
    }
    // Procedure assignment permits numeric widening (including executable
    // integer defaults for FLOAT parameters); result membership does not.
    let source = match value {
        Value::Int(_) => crate::GqlType::Int64,
        Value::Uint(_) => crate::GqlType::Uint64,
        Value::Int128(_) => crate::GqlType::Int128,
        Value::Uint128(_) => crate::GqlType::Uint128,
        Value::Float32(_) => crate::GqlType::Float32,
        Value::Float(_) => crate::GqlType::Float64,
        Value::Decimal(_) => crate::GqlType::Decimal,
        _ => return false,
    };
    crate::analyze::infer::argument_assignable(&source, ty, false)
}
