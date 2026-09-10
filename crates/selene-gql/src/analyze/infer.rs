//! Expression type-inference helpers.

#[path = "infer/binary.rs"]
mod binary_ops;
mod duration;
mod list;
mod numeric;
mod trim;
mod typed_target;

use crate::{
    GqlType, IsCheckKind, Literal, SourceSpan, UnaryOp,
    analyze::{
        error::{AnalysisError, ConditionClause, ExpectedType, TypeMismatchContext},
        types::AnalyzedType,
    },
};

use self::{
    list::list_union_type,
    numeric::{is_numeric, numeric_promotion},
    typed_target::is_supported_typed_target,
};

pub(crate) use self::binary_ops::binary;
pub(crate) use self::numeric::argument_assignable;
pub(crate) use self::trim::trim;

/// Infer a literal expression type.
#[must_use]
pub(crate) fn literal(literal: &Literal) -> AnalyzedType {
    match literal {
        Literal::Bool(..) => AnalyzedType::Resolved(GqlType::Boolean),
        Literal::Integer(..) | Literal::RadixInteger(..) => {
            AnalyzedType::Resolved(GqlType::Integer)
        }
        Literal::Decimal(..) => AnalyzedType::Resolved(GqlType::Decimal),
        Literal::Float(..) => AnalyzedType::Resolved(GqlType::Float),
        Literal::String(..) => AnalyzedType::Resolved(GqlType::String),
        Literal::Bytes(..) => AnalyzedType::Resolved(GqlType::Bytes),
        Literal::Uuid(..) => AnalyzedType::Resolved(GqlType::Uuid),
        Literal::ZonedDateTime(..) => AnalyzedType::Resolved(GqlType::ZonedDateTime),
        Literal::LocalDateTime(..) => AnalyzedType::Resolved(GqlType::LocalDateTime),
        Literal::Date(..) => AnalyzedType::Resolved(GqlType::Date),
        Literal::ZonedTime(..) => AnalyzedType::Resolved(GqlType::ZonedTime),
        Literal::LocalTime(..) => AnalyzedType::Resolved(GqlType::LocalTime),
        Literal::Duration(..) => AnalyzedType::Resolved(GqlType::Duration),
        Literal::Null(..) => AnalyzedType::Resolved(GqlType::Null),
    }
}

/// Infer a unary operator expression type.
pub(crate) fn unary(
    op: UnaryOp,
    operand: &AnalyzedType,
    span: SourceSpan,
) -> Result<AnalyzedType, AnalysisError> {
    match op {
        UnaryOp::Negate => match operand {
            AnalyzedType::Dynamic => Ok(AnalyzedType::Dynamic),
            AnalyzedType::Resolved(ty) if is_numeric(ty) => Ok(operand.clone()),
            AnalyzedType::Resolved(ty) if ty.is_duration() => {
                Ok(AnalyzedType::Resolved(GqlType::Duration))
            }
            // Three-valued logic: `- NULL` yields NULL, so analysis must not reject.
            AnalyzedType::Resolved(GqlType::Null) => Ok(AnalyzedType::Resolved(GqlType::Null)),
            AnalyzedType::Resolved(found) => Err(type_mismatch(
                TypeMismatchContext::UnaryNegate,
                ExpectedType::Numeric,
                found.clone(),
                span,
            )),
        },
        UnaryOp::Not => match operand {
            AnalyzedType::Dynamic => Ok(AnalyzedType::Resolved(GqlType::Boolean)),
            AnalyzedType::Resolved(ty) if matches!(ty.strip_not_null(), GqlType::Boolean) => {
                Ok(AnalyzedType::Resolved(GqlType::Boolean))
            }
            // `NOT NULL` yields UNKNOWN; the static result stays Boolean.
            AnalyzedType::Resolved(GqlType::Null) => Ok(AnalyzedType::Resolved(GqlType::Boolean)),
            AnalyzedType::Resolved(found) => Err(type_mismatch(
                TypeMismatchContext::UnaryNot,
                ExpectedType::Boolean,
                found.clone(),
                span,
            )),
        },
    }
}

/// Infer an `IS` predicate type and validate statically-checkable operands.
pub(crate) fn is_check(
    kind: &IsCheckKind,
    operand: &AnalyzedType,
    operand_span: SourceSpan,
    predicate_span: SourceSpan,
) -> Result<AnalyzedType, AnalysisError> {
    match kind {
        IsCheckKind::Typed(ty) if !is_supported_typed_target(ty) => Err(type_mismatch(
            TypeMismatchContext::IsTypedTarget,
            ExpectedType::Comparable,
            ty.clone(),
            predicate_span,
        )),
        IsCheckKind::Normalized(_) => {
            expect_string(operand, operand_span, TypeMismatchContext::IsNormalized)?;
            Ok(AnalyzedType::Resolved(GqlType::Boolean))
        }
        IsCheckKind::TruthValue(_) => {
            expect_boolean(operand, operand_span, TypeMismatchContext::IsTruthValue)?;
            Ok(AnalyzedType::Resolved(GqlType::Boolean))
        }
        IsCheckKind::Null
        | IsCheckKind::Directed
        | IsCheckKind::Labeled(_)
        | IsCheckKind::Typed(_)
        | IsCheckKind::SourceOf(_)
        | IsCheckKind::DestinationOf(_) => Ok(AnalyzedType::Resolved(GqlType::Boolean)),
    }
}

/// Infer `NORMALIZE(<string>[, <normal form>])`.
pub(crate) fn normalize(
    source: &AnalyzedType,
    source_span: SourceSpan,
) -> Result<AnalyzedType, AnalysisError> {
    expect_string(source, source_span, TypeMismatchContext::NormalizeFunction)?;
    Ok(AnalyzedType::Resolved(GqlType::String))
}

/// Infer an `IN` predicate type.
pub(crate) fn in_list(
    operand: &AnalyzedType,
    operand_span: SourceSpan,
    items: &[(AnalyzedType, SourceSpan)],
) -> Result<AnalyzedType, AnalysisError> {
    let mut unified: Option<(GqlType, SourceSpan)> = None;
    for (ty, span) in items {
        if let AnalyzedType::Resolved(item_ty) = ty {
            if let Some((current, _)) = &unified {
                if let Some(meet) = meet_gql_types(current, item_ty) {
                    unified = Some((meet, *span));
                } else {
                    return Err(type_mismatch(
                        TypeMismatchContext::InListUnification,
                        ExpectedType::Specific(current.clone()),
                        item_ty.clone(),
                        *span,
                    ));
                }
            } else {
                unified = Some((item_ty.clone(), *span));
            }
        }
    }
    if let (AnalyzedType::Resolved(operand_ty), Some((item_ty, item_span))) = (operand, unified)
        && meet_gql_types(operand_ty, &item_ty).is_none()
    {
        return Err(type_mismatch(
            TypeMismatchContext::InListUnification,
            ExpectedType::Specific(operand_ty.clone()),
            item_ty,
            item_span.max(operand_span),
        ));
    }
    Ok(AnalyzedType::Resolved(GqlType::Boolean))
}

/// Infer an `IN` predicate whose right side is a list-valued expression.
pub(crate) fn in_list_expression(
    operand: &AnalyzedType,
    operand_span: SourceSpan,
    list: &AnalyzedType,
    list_span: SourceSpan,
) -> Result<AnalyzedType, AnalysisError> {
    if let AnalyzedType::Resolved(list_ty) = list {
        match list_ty.strip_not_null() {
            GqlType::List(item_ty)
            | GqlType::BoundedList {
                element_type: item_ty,
                ..
            } => {
                if let AnalyzedType::Resolved(operand_ty) = operand
                    && meet_gql_types(operand_ty, item_ty).is_none()
                {
                    return Err(type_mismatch(
                        TypeMismatchContext::InListUnification,
                        ExpectedType::Specific(operand_ty.clone()),
                        (**item_ty).clone(),
                        list_span.max(operand_span),
                    ));
                }
            }
            GqlType::Null => {}
            found => {
                return Err(type_mismatch(
                    TypeMismatchContext::InListUnification,
                    ExpectedType::List,
                    found.clone(),
                    list_span,
                ));
            }
        }
    }
    Ok(AnalyzedType::Resolved(GqlType::Boolean))
}

/// Infer a list literal type.
pub(crate) fn list_literal(
    items: &[(AnalyzedType, SourceSpan)],
) -> Result<AnalyzedType, AnalysisError> {
    let mut unified: Option<(GqlType, SourceSpan)> = None;
    let mut saw_dynamic = false;
    for (ty, span) in items {
        match ty {
            AnalyzedType::Dynamic => saw_dynamic = true,
            AnalyzedType::Resolved(item_ty) => {
                unified = Some(match unified {
                    Some((ref current, _)) => (
                        meet_gql_types(current, item_ty).ok_or_else(|| {
                            type_mismatch(
                                TypeMismatchContext::ListLiteralUnification,
                                ExpectedType::Specific(current.clone()),
                                item_ty.clone(),
                                *span,
                            )
                        })?,
                        *span,
                    ),
                    None => (item_ty.clone(), *span),
                });
            }
        }
    }
    if saw_dynamic {
        return Ok(AnalyzedType::Dynamic);
    }
    let Some((item_ty, _)) = unified else {
        return Ok(AnalyzedType::Dynamic);
    };
    Ok(AnalyzedType::Resolved(GqlType::List(Box::new(item_ty))))
}

/// Infer a CASE expression result type from branch result cells.
pub(crate) fn case_result(
    branches: &[(AnalyzedType, SourceSpan)],
) -> Result<AnalyzedType, AnalysisError> {
    let mut unified: Option<(GqlType, SourceSpan)> = None;
    let mut saw_dynamic = false;
    for (ty, span) in branches {
        match ty {
            AnalyzedType::Dynamic => saw_dynamic = true,
            AnalyzedType::Resolved(branch_ty) => {
                unified = Some(match unified {
                    Some((ref current, _)) => (
                        meet_gql_types(current, branch_ty).ok_or_else(|| {
                            type_mismatch(
                                TypeMismatchContext::CaseBranchUnification,
                                ExpectedType::Specific(current.clone()),
                                branch_ty.clone(),
                                *span,
                            )
                        })?,
                        *span,
                    ),
                    None => (branch_ty.clone(), *span),
                });
            }
        }
    }
    if saw_dynamic {
        return Ok(AnalyzedType::Dynamic);
    }
    Ok(unified
        .map(|(ty, _)| AnalyzedType::Resolved(ty))
        .unwrap_or(AnalyzedType::Dynamic))
}

/// Ensure a clause condition is boolean when statically known.
pub(crate) fn condition(
    ty: &AnalyzedType,
    span: SourceSpan,
    clause: ConditionClause,
) -> Result<(), AnalysisError> {
    match ty {
        AnalyzedType::Dynamic => Ok(()),
        AnalyzedType::Resolved(found) if matches!(found.strip_not_null(), GqlType::Boolean) => {
            Ok(())
        }
        AnalyzedType::Resolved(found) => Err(type_mismatch(
            TypeMismatchContext::Condition { clause },
            ExpectedType::Specific(GqlType::Boolean),
            found.clone(),
            span,
        )),
    }
}

fn expect_numeric(
    ty: &AnalyzedType,
    span: SourceSpan,
    context: TypeMismatchContext,
) -> Result<(), AnalysisError> {
    match ty {
        // A NULL operand is accepted: ISO three-valued logic makes the operator
        // yield NULL rather than a type error (the runtime already does so).
        AnalyzedType::Dynamic | AnalyzedType::Resolved(GqlType::Null) => Ok(()),
        AnalyzedType::Resolved(found) if is_numeric(found) => Ok(()),
        AnalyzedType::Resolved(found) => Err(type_mismatch(
            context,
            ExpectedType::Numeric,
            found.clone(),
            span,
        )),
    }
}

fn expect_boolean(
    ty: &AnalyzedType,
    span: SourceSpan,
    context: TypeMismatchContext,
) -> Result<(), AnalysisError> {
    match ty {
        // A NULL operand is accepted: under three-valued logic a boolean
        // operator over NULL yields NULL, not a type error (runtime parity).
        AnalyzedType::Dynamic | AnalyzedType::Resolved(GqlType::Null) => Ok(()),
        AnalyzedType::Resolved(found) if matches!(found.strip_not_null(), GqlType::Boolean) => {
            Ok(())
        }
        AnalyzedType::Resolved(found) => Err(type_mismatch(
            context,
            ExpectedType::Boolean,
            found.clone(),
            span,
        )),
    }
}

fn expect_string(
    ty: &AnalyzedType,
    span: SourceSpan,
    context: TypeMismatchContext,
) -> Result<(), AnalysisError> {
    match ty {
        AnalyzedType::Dynamic | AnalyzedType::Resolved(GqlType::Null) => Ok(()),
        AnalyzedType::Resolved(found) if is_character_string(found) => Ok(()),
        AnalyzedType::Resolved(found) => Err(type_mismatch(
            context,
            ExpectedType::String,
            found.clone(),
            span,
        )),
    }
}

fn expect_concat_operand(
    ty: &AnalyzedType,
    span: SourceSpan,
    context: TypeMismatchContext,
) -> Result<(), AnalysisError> {
    match ty {
        AnalyzedType::Dynamic | AnalyzedType::Resolved(GqlType::Null) => Ok(()),
        AnalyzedType::Resolved(found)
            if matches!(
                found.strip_not_null(),
                GqlType::String
                    | GqlType::CharacterString(_)
                    | GqlType::Bytes
                    | GqlType::ByteString(_)
                    | GqlType::List(_)
                    | GqlType::BoundedList { .. }
                    | GqlType::Path
            ) =>
        {
            Ok(())
        }
        AnalyzedType::Resolved(found) => Err(type_mismatch(
            context,
            ExpectedType::ListStringBytesOrPath,
            found.clone(),
            span,
        )),
    }
}

fn meet_gql_types(lhs: &GqlType, rhs: &GqlType) -> Option<GqlType> {
    if lhs == rhs {
        return Some(lhs.clone());
    }
    if matches!(lhs, GqlType::Null) {
        return Some(rhs.strip_not_null().clone());
    }
    if matches!(rhs, GqlType::Null) {
        return Some(lhs.strip_not_null().clone());
    }
    if is_numeric(lhs) && is_numeric(rhs) {
        return numeric_promotion(lhs, rhs);
    }
    let lhs_base = lhs.strip_not_null();
    let rhs_base = rhs.strip_not_null();
    if lhs_base == rhs_base {
        return Some(
            if matches!(lhs, GqlType::NotNull(_)) && matches!(rhs, GqlType::NotNull(_)) {
                GqlType::NotNull(Box::new(lhs_base.clone()))
            } else {
                lhs_base.clone()
            },
        );
    }
    if matches!(
        (lhs_base, rhs_base),
        (GqlType::Record(_), GqlType::Record(_))
    ) {
        // Distinct named shapes still share the supported open RECORD family.
        // This is type unification, not permission to compare their field sets.
        return Some(GqlType::Record(crate::RecordType::Open));
    }
    list_union_type(lhs_base, rhs_base, meet_gql_types)
}

fn type_mismatch(
    context: TypeMismatchContext,
    expected: ExpectedType,
    found: GqlType,
    span: SourceSpan,
) -> AnalysisError {
    AnalysisError::TypeMismatch {
        context,
        expected,
        found,
        span,
    }
}

fn is_byte_string(ty: &GqlType) -> bool {
    matches!(ty.strip_not_null(), GqlType::Bytes | GqlType::ByteString(_))
}

fn is_character_string(ty: &GqlType) -> bool {
    matches!(
        ty.strip_not_null(),
        GqlType::String | GqlType::CharacterString(_)
    )
}

trait SpanMax {
    fn max(self, other: Self) -> Self;
}

impl SpanMax for SourceSpan {
    fn max(self, other: Self) -> Self {
        if self.byte_len == 0 { other } else { self }
    }
}

/// Infer the result type of an explicit `CAST(<value> AS <target_type>)`
/// expression.
///
/// The analyzer reports the cast's static result type as the declared target
/// type. Runtime validity (whether the source value can actually be cast to
/// the target) is enforced by `runtime::evaluator::cast::eval_cast` per ISO
/// §22; the analyzer does not pre-reject because (i) source types are often
/// Dynamic, (ii) ISO §22 specifies a runtime error model (`22018`, `22003`,
/// `42N01`) rather than a compile-time rejection model.
pub(crate) fn cast(target_type: &GqlType) -> Result<AnalyzedType, AnalysisError> {
    Ok(AnalyzedType::Resolved(target_type.clone()))
}
