//! Numeric type promotion lattice.

use crate::GqlType;

/// Find the promoted result type for numeric operands.
#[must_use]
pub(crate) fn numeric_promotion(lhs: &GqlType, rhs: &GqlType) -> Option<GqlType> {
    if lhs == rhs && is_numeric(lhs) && !matches!(lhs.strip_not_null(), GqlType::DecimalExact(_)) {
        return Some(lhs.clone());
    }
    match (numeric_kind(lhs)?, numeric_kind(rhs)?) {
        (NumericKind::Float(left), NumericKind::Float(right)) => Some(float_result(left, right)),
        (NumericKind::Float(_), _) | (_, NumericKind::Float(_)) => Some(GqlType::Float64),
        (NumericKind::Decimal, _) | (_, NumericKind::Decimal) => Some(GqlType::Decimal),
        (NumericKind::Integer(left), NumericKind::Integer(right)) => {
            Some(integer_result(left, right))
        }
    }
}

/// Return true when `ty` participates in numeric promotion.
pub(crate) fn is_numeric(ty: &GqlType) -> bool {
    numeric_kind(ty).is_some()
}

/// Return true if `arg_ty` can flow into a procedure parameter of `param_ty`.
pub(crate) fn argument_assignable(arg_ty: &GqlType, param_ty: &GqlType, nullable: bool) -> bool {
    let (Ok(source), Ok(target)) = (
        crate::normalize_value_type(arg_ty),
        crate::normalize_value_type(param_ty),
    ) else {
        return false;
    };
    let accepts_null = nullable && target.is_nullable();
    target
        .with_nullability(accepts_null)
        .assignment_compatible(&source)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumericKind {
    Integer(IntegerKind),
    Decimal,
    Float(FloatKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IntegerKind {
    signed: bool,
    width: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum FloatKind {
    Unsized,
    F32,
    F64,
}

fn numeric_kind(ty: &GqlType) -> Option<NumericKind> {
    Some(match ty.strip_not_null() {
        GqlType::Integer | GqlType::BigInt | GqlType::Int64 => NumericKind::Integer(IntegerKind {
            signed: true,
            width: 64,
        }),
        GqlType::SmallInt | GqlType::Int16 => NumericKind::Integer(IntegerKind {
            signed: true,
            width: 16,
        }),
        GqlType::Int8 => NumericKind::Integer(IntegerKind {
            signed: true,
            width: 8,
        }),
        GqlType::Int32 => NumericKind::Integer(IntegerKind {
            signed: true,
            width: 32,
        }),
        GqlType::Int128 => NumericKind::Integer(IntegerKind {
            signed: true,
            width: 128,
        }),
        GqlType::Uint8 => NumericKind::Integer(IntegerKind {
            signed: false,
            width: 8,
        }),
        GqlType::Uint16 | GqlType::USmallInt => NumericKind::Integer(IntegerKind {
            signed: false,
            width: 16,
        }),
        GqlType::Uint32 | GqlType::Uint => NumericKind::Integer(IntegerKind {
            signed: false,
            width: 32,
        }),
        GqlType::Uint64 | GqlType::UBigInt => NumericKind::Integer(IntegerKind {
            signed: false,
            width: 64,
        }),
        GqlType::Uint128 => NumericKind::Integer(IntegerKind {
            signed: false,
            width: 128,
        }),
        GqlType::Decimal | GqlType::DecimalExact(_) => NumericKind::Decimal,
        GqlType::Float => NumericKind::Float(FloatKind::Unsized),
        GqlType::Float32 | GqlType::Real => NumericKind::Float(FloatKind::F32),
        GqlType::Float64 | GqlType::Double => NumericKind::Float(FloatKind::F64),
        _ => return None,
    })
}

fn integer_result(lhs: IntegerKind, rhs: IntegerKind) -> GqlType {
    match (lhs.signed, rhs.signed) {
        (true, true) => signed_integer(lhs.width.max(rhs.width)).unwrap_or(GqlType::Decimal),
        (false, false) => unsigned_integer(lhs.width.max(rhs.width)),
        (true, false) => mixed_integer_result(lhs, rhs),
        (false, true) => mixed_integer_result(rhs, lhs),
    }
}

fn mixed_integer_result(signed: IntegerKind, unsigned: IntegerKind) -> GqlType {
    if signed.width > unsigned.width {
        return signed_integer(signed.width).unwrap_or(GqlType::Decimal);
    }
    next_signed_width(unsigned.width)
        .and_then(signed_integer)
        .unwrap_or(GqlType::Decimal)
}

fn signed_integer(width: u16) -> Option<GqlType> {
    Some(match width {
        8 => GqlType::Int8,
        16 => GqlType::Int16,
        32 => GqlType::Int32,
        64 => GqlType::Int64,
        128 => GqlType::Int128,
        _ => return None,
    })
}

fn unsigned_integer(width: u16) -> GqlType {
    match width {
        8 => GqlType::Uint8,
        16 => GqlType::Uint16,
        32 => GqlType::Uint32,
        64 => GqlType::Uint64,
        _ => GqlType::Uint128,
    }
}

fn next_signed_width(width: u16) -> Option<u16> {
    match width {
        0..=8 => Some(16),
        9..=16 => Some(32),
        17..=32 => Some(64),
        33..=64 => Some(128),
        _ => None,
    }
}

fn float_result(lhs: FloatKind, rhs: FloatKind) -> GqlType {
    match (lhs, rhs) {
        (FloatKind::Unsized, FloatKind::Unsized) => GqlType::Float,
        (FloatKind::F32, FloatKind::F32) => GqlType::Float32,
        _ => GqlType::Float64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_unsigned_equal_width_promotes_one_signed_step() {
        assert_eq!(
            numeric_promotion(&GqlType::Uint32, &GqlType::Int32),
            Some(GqlType::Int64)
        );
        assert_eq!(
            numeric_promotion(&GqlType::Uint64, &GqlType::Int64),
            Some(GqlType::Int128)
        );
    }

    #[test]
    fn signed_unsigned_128_promotes_to_decimal() {
        assert_eq!(
            numeric_promotion(&GqlType::Uint128, &GqlType::Int128),
            Some(GqlType::Decimal)
        );
    }

    #[test]
    fn exact_integer_overrides_widen_when_same_type() {
        assert_eq!(
            numeric_promotion(&GqlType::Uint128, &GqlType::Uint128),
            Some(GqlType::Uint128)
        );
        assert_eq!(
            numeric_promotion(&GqlType::Integer, &GqlType::Integer),
            Some(GqlType::Integer)
        );
    }

    #[test]
    fn float_absorbs_exact_types_to_float64() {
        assert_eq!(
            numeric_promotion(&GqlType::Float32, &GqlType::Integer),
            Some(GqlType::Float64)
        );
        assert_eq!(
            numeric_promotion(&GqlType::Decimal, &GqlType::Float32),
            Some(GqlType::Float64)
        );
    }

    #[test]
    fn unsized_float_with_float32_promotes_to_float64() {
        assert_eq!(
            numeric_promotion(&GqlType::Float, &GqlType::Float32),
            Some(GqlType::Float64)
        );
        assert_eq!(
            numeric_promotion(&GqlType::Float, &GqlType::Float),
            Some(GqlType::Float)
        );
    }

    #[test]
    fn argument_assignment_accepts_identity() {
        assert!(argument_assignable(
            &GqlType::String,
            &GqlType::String,
            false
        ));
    }

    #[test]
    fn argument_assignment_accepts_narrower_to_wider_numeric() {
        assert!(argument_assignable(&GqlType::Int8, &GqlType::Int64, false));
        assert!(argument_assignable(
            &GqlType::Uint32,
            &GqlType::Int64,
            false
        ));
    }

    #[test]
    fn argument_assignment_rejects_wider_to_narrower_numeric() {
        assert!(!argument_assignable(&GqlType::Int64, &GqlType::Int8, false));
        assert!(!argument_assignable(
            &GqlType::Int64,
            &GqlType::Uint64,
            false
        ));
    }

    #[test]
    fn argument_assignment_allows_tier_0_to_1_but_not_reverse() {
        assert!(argument_assignable(
            &GqlType::Integer,
            &GqlType::Decimal,
            false
        ));
        assert!(!argument_assignable(
            &GqlType::Decimal,
            &GqlType::Integer,
            false
        ));
    }

    #[test]
    fn argument_assignment_allows_tier_1_to_2_but_not_reverse() {
        assert!(argument_assignable(
            &GqlType::Decimal,
            &GqlType::Float64,
            false
        ));
        assert!(!argument_assignable(
            &GqlType::Float64,
            &GqlType::Decimal,
            false
        ));
    }

    #[test]
    fn argument_assignment_treats_unsized_float_as_abstract() {
        assert!(argument_assignable(
            &GqlType::Float,
            &GqlType::Float32,
            false
        ));
        assert!(argument_assignable(
            &GqlType::Float32,
            &GqlType::Float,
            false
        ));
        assert!(argument_assignable(
            &GqlType::Float64,
            &GqlType::Float,
            false
        ));
    }

    #[test]
    fn argument_assignment_keeps_concrete_float_width_directional() {
        assert!(argument_assignable(
            &GqlType::Float32,
            &GqlType::Float64,
            false
        ));
        assert!(!argument_assignable(
            &GqlType::Float64,
            &GqlType::Float32,
            false
        ));
    }

    #[test]
    fn argument_assignment_null_uses_nullable_flag() {
        assert!(argument_assignable(&GqlType::Null, &GqlType::Integer, true));
        assert!(!argument_assignable(
            &GqlType::Null,
            &GqlType::Integer,
            false
        ));
    }

    #[test]
    fn argument_assignment_rejects_unrelated_families() {
        assert!(!argument_assignable(
            &GqlType::String,
            &GqlType::Date,
            false
        ));
    }
}
