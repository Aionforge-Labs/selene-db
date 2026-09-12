//! Session services for physical controls (ISO/IEC 39075:2024 section 7).
//!
//! Bare lower-engine commands mutate [`Session`] state directly and return
//! [`StatementOutput::Empty`]. Selected facade commands use [`prepare`] to
//! validate and evaluate without mutating that transient session.
//! `SESSION SET VALUE` evaluates its right-hand
//! side against an empty binding row (the value is restricted to a
//! `<value specification>`, so GS14 is runtime-unsupported) and binds the result as a
//! session-local parameter. `SESSION SET TIME ZONE` parses the time-zone
//! string with `jiff`. `SESSION SET [PROPERTY] GRAPH <current graph>` is a
//! no-op in the D1 single-graph engine. The RESET forms clear parameters /
//! reset the time zone, and `SESSION CLOSE` raises the termination flag.

use selene_core::Value;

use crate::{
    GqlType, ImplDefinedCaps, ProcedureRegistry, SessionOp, SourceSpan, ValueExpr,
    plan::SubqueryRegistry,
    runtime::{
        Binding, BindingTableSchema, DataExceptionSubclass, EvalCtx, ExecutorError,
        PreparedSessionControl, Session, StatementOutput, TxContext, evaluator,
    },
};

/// Execute one top-level session-control operation.
pub(crate) fn execute(
    op: &SessionOp,
    session: &mut Session<'_>,
    registry: &dyn ProcedureRegistry,
) -> Result<StatementOutput, ExecutorError> {
    match op {
        SessionOp::SetValue {
            param,
            declared_type,
            value,
            if_not_exists,
            span,
        } => set_value(
            session,
            registry,
            param.clone(),
            declared_type.as_ref(),
            value,
            *if_not_exists,
            *span,
        ),
        SessionOp::SetTimeZone { zone, span } => set_time_zone(session, zone, *span),
        SessionOp::SetSchema { span, .. } => Err(ExecutorError::FeatureNotSupportedYet {
            feature: "catalog schema switching requires a selected facade session",
            span: *span,
        }),
        SessionOp::SetGraph {
            target: crate::SessionSetGraphTarget::CatalogReference(_),
            span,
        } => Err(ExecutorError::FeatureNotSupportedYet {
            feature: "catalog graph switching requires a selected facade session",
            span: *span,
        }),
        SessionOp::SetGraph { .. } => {
            // D1 has exactly one graph; both current-graph expressions resolve
            // to the session's embedded graph and leave state unchanged.
            Ok(StatementOutput::Empty)
        }
        SessionOp::ResetAllCharacteristics { .. } => {
            session.reset_characteristics();
            Ok(StatementOutput::Empty)
        }
        SessionOp::ResetSchema { .. } | SessionOp::ResetGraph { .. } => {
            // Persistent schema/graph characteristics are facade-owned. A
            // bare lower session has one fixed graph, so both resets are a
            // truthful no-op at this lower-engine seam.
            Ok(StatementOutput::Empty)
        }
        SessionOp::ResetParameters { .. } => {
            session.reset_parameters();
            Ok(StatementOutput::Empty)
        }
        SessionOp::ResetTimeZone { .. } => {
            session.reset_time_zone();
            Ok(StatementOutput::Empty)
        }
        SessionOp::ResetParameter { param, .. } => {
            session.reset_parameter(param);
            Ok(StatementOutput::Empty)
        }
        SessionOp::Close { .. } => {
            session.close();
            Ok(StatementOutput::Empty)
        }
    }
}

/// Evaluate and validate one facade-owned session operation without mutating
/// the transient lower-engine session.
pub(crate) fn prepare(
    op: &SessionOp,
    session: &Session<'_>,
    registry: &dyn ProcedureRegistry,
    skip_if_exists: bool,
) -> Result<PreparedSessionControl, ExecutorError> {
    Ok(match op {
        SessionOp::SetValue {
            param,
            declared_type,
            value,
            span,
            ..
        } => {
            if skip_if_exists {
                return Ok(PreparedSessionControl::NoOp);
            }
            let evaluated = evaluate_constant(session, registry, value)?;
            if let Some(declared_type) = declared_type {
                crate::runtime::parameter_type::validate_declared_type(
                    param.clone(),
                    &evaluated,
                    declared_type,
                    *span,
                )?;
            }
            PreparedSessionControl::SetValue {
                param: param.clone(),
                declared_type: declared_type.clone().unwrap_or(GqlType::Any),
                value: evaluated,
            }
        }
        SessionOp::SetTimeZone { zone, span } => {
            let zone = parse_time_zone(zone, *span)?;
            let displacement_seconds = zone
                .to_offset(session.effective_request_timestamp())
                .seconds();
            PreparedSessionControl::SetTimeZone {
                zone,
                displacement_seconds,
            }
        }
        SessionOp::SetSchema { reference, .. } => {
            PreparedSessionControl::SetSchema(reference.clone())
        }
        SessionOp::SetGraph { target, .. } => PreparedSessionControl::SetGraph(target.clone()),
        SessionOp::ResetAllCharacteristics { .. } => {
            PreparedSessionControl::ResetAllCharacteristics
        }
        SessionOp::ResetSchema { .. } => PreparedSessionControl::ResetSchema,
        SessionOp::ResetGraph { .. } => PreparedSessionControl::ResetGraph,
        SessionOp::ResetParameters { .. } => PreparedSessionControl::ResetParameters,
        SessionOp::ResetTimeZone { .. } => PreparedSessionControl::ResetTimeZone,
        SessionOp::ResetParameter { param, .. } => {
            PreparedSessionControl::ResetParameter(param.clone())
        }
        SessionOp::Close { .. } => PreparedSessionControl::Close,
    })
}

fn set_value(
    session: &mut Session<'_>,
    registry: &dyn ProcedureRegistry,
    param: selene_core::DbString,
    declared_type: Option<&GqlType>,
    value: &ValueExpr,
    if_not_exists: bool,
    span: SourceSpan,
) -> Result<StatementOutput, ExecutorError> {
    // IF NOT EXISTS (ISO section 7.4): leave an existing binding untouched.
    if if_not_exists && session.has_parameter(&param) {
        return Ok(StatementOutput::Empty);
    }
    let evaluated = evaluate_constant(session, registry, value)?;
    if let Some(declared_type) = declared_type {
        crate::runtime::parameter_type::validate_declared_type(
            param.clone(),
            &evaluated,
            declared_type,
            span,
        )?;
    }
    session.bind_parameter(param, evaluated);
    Ok(StatementOutput::Empty)
}

/// Evaluate a `<value specification>` right-hand side against an empty binding.
///
/// The RHS may reference already-bound session parameters (`$other`) and
/// literals; it carries no row context (no graph variables) because session
/// commands run outside a binding-producing pipeline.
fn evaluate_constant(
    session: &Session<'_>,
    registry: &dyn ProcedureRegistry,
    value: &ValueExpr,
) -> Result<Value, ExecutorError> {
    let snapshot = session.graph().read();
    let providers = session.graph().index_providers();
    let session_tz = session.effective_time_zone();
    let caps = ImplDefinedCaps::default();
    let ctx = TxContext::read_only_with_parameters(
        snapshot,
        &caps,
        registry,
        providers,
        &session.scalar_parameters,
    )
    .with_session_time_zone(session_tz);
    let expr_ids = crate::analyze::ExprIdLookup::default();
    let subqueries = SubqueryRegistry::default();
    let eval_ctx = EvalCtx {
        tx: &ctx,
        expr_ids: &expr_ids,
        subqueries: &subqueries,
    };
    let binding = Binding::empty();
    let schema = BindingTableSchema {
        columns: Vec::new(),
    };
    evaluator::evaluate(value, &binding, &schema, &eval_ctx)
}

fn set_time_zone(
    session: &mut Session<'_>,
    zone: &str,
    span: SourceSpan,
) -> Result<StatementOutput, ExecutorError> {
    let tz = parse_time_zone(zone, span)?;
    session.set_time_zone(tz);
    Ok(StatementOutput::Empty)
}

/// Parse a `<time zone string>` into a `jiff` time zone.
///
/// Accepts, in priority order: a fixed UTC offset (`+HH:MM`, `-HH:MM:SS`,
/// `+HHMM`, or `Z`), an IANA region name (e.g. `America/New_York`), and a POSIX
/// TZ string. An unparseable string raises a data exception rather than
/// panicking (forbid-unsafe / no-panic discipline).
fn parse_time_zone(zone: &str, span: SourceSpan) -> Result<jiff::tz::TimeZone, ExecutorError> {
    if let Some(offset) = parse_fixed_offset(zone) {
        return Ok(jiff::tz::TimeZone::fixed(offset));
    }
    jiff::tz::TimeZone::get(zone)
        .or_else(|_| jiff::tz::TimeZone::posix(zone))
        .map_err(|_| {
            ExecutorError::data_exception(
                DataExceptionSubclass::InvalidTimeZone,
                format!("unknown or invalid time zone: {zone}"),
                span,
            )
        })
}

/// Parse a fixed UTC offset string into a `jiff` offset.
///
/// Supports `Z`/`z` (UTC) and `(+|-)HH[:]MM[[:]SS]`. Returns `None` for any
/// other form so the caller can fall back to IANA / POSIX resolution.
fn parse_fixed_offset(zone: &str) -> Option<jiff::tz::Offset> {
    if zone.eq_ignore_ascii_case("Z") {
        return Some(jiff::tz::Offset::UTC);
    }
    let (sign, rest) = match zone.as_bytes().first()? {
        b'+' => (1_i32, &zone[1..]),
        b'-' => (-1_i32, &zone[1..]),
        _ => return None,
    };
    // Accept `HH:MM`, `HH:MM:SS`, `HHMM`, or `HHMMSS`.
    let digits: String = rest.chars().filter(|ch| *ch != ':').collect();
    if digits.len() < 2 || digits.len() > 6 || !digits.len().is_multiple_of(2) {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[0..2].parse().ok()?;
    let minutes: i32 = digits.get(2..4).map_or(Ok(0), str::parse).ok()?;
    let seconds: i32 = digits.get(4..6).map_or(Ok(0), str::parse).ok()?;
    if hours > 25 || minutes > 59 || seconds > 59 {
        return None;
    }
    let total = sign * (hours * 3600 + minutes * 60 + seconds);
    jiff::tz::Offset::from_seconds(total).ok()
}
