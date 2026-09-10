//! Exact comparison and grouping keys for the selected numeric representations.
//!
//! A finite value is `significand * 2^exponent / 5^fives`. Reducing powers
//! of two and five produces one representation across integers, decimals and
//! binary floats, without rounding either operand through another value type.

use std::{cmp::Ordering, hash::Hash};

use crate::Value;

/// Canonical numeric grouping identity, not a persisted encoding or type ID.
///
/// All NaNs belong to one not-distinct group. Predicate comparison of a NaN
/// remains unknown. Signed zeros share one group; infinities retain their sign.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NumericKey(Number);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Number {
    Finite {
        negative: bool,
        significand: u128,
        exponent: i32,
        fives: u32,
    },
    Infinity(bool),
    Nan,
}

impl NumericKey {
    /// Construct a grouping key, or `None` for a non-numeric value.
    #[must_use]
    pub fn of(value: &Value) -> Option<Self> {
        let number = match value {
            Value::Int(v) => finite(v.is_negative(), u128::from(v.unsigned_abs()), 0, 0),
            Value::Uint(v) => finite(false, u128::from(*v), 0, 0),
            Value::Int128(v) => finite(v.is_negative(), v.unsigned_abs(), 0, 0),
            Value::Uint128(v) => finite(false, *v, 0, 0),
            Value::Decimal(v) => finite(
                v.is_sign_negative(),
                v.mantissa().unsigned_abs(),
                -(v.scale() as i32),
                v.scale(),
            ),
            Value::Float(v) => binary(*v),
            Value::Float32(v) => binary(f64::from(*v)),
            _ => return None,
        };
        Some(Self(number))
    }

    /// Compare numeric values; `None` denotes the NaN predicate outcome.
    #[must_use]
    pub fn predicate_cmp(self, rhs: Self) -> Option<Ordering> {
        use Number::{Finite, Infinity, Nan};
        match (self.0, rhs.0) {
            (Nan, _) | (_, Nan) => None,
            (Infinity(a), Infinity(b)) => Some(b.cmp(&a)),
            (Infinity(negative), _) => Some(if negative {
                Ordering::Less
            } else {
                Ordering::Greater
            }),
            (_, Infinity(negative)) => Some(if negative {
                Ordering::Greater
            } else {
                Ordering::Less
            }),
            (
                Finite {
                    negative: a,
                    significand: am,
                    exponent: ae,
                    fives: af,
                },
                Finite {
                    negative: b,
                    significand: bm,
                    exponent: be,
                    fives: bf,
                },
            ) => {
                if a != b {
                    return Some(b.cmp(&a));
                }
                let order = magnitude_cmp(am, ae, af, bm, be, bf);
                Some(if a { order.reverse() } else { order })
            }
        }
    }

    /// Deterministic numeric sorting: NaNs high, and equal numeric values tied.
    #[must_use]
    pub fn sort_cmp(self, rhs: Self) -> Ordering {
        self.predicate_cmp(rhs)
            .unwrap_or_else(|| matches!(self.0, Number::Nan).cmp(&matches!(rhs.0, Number::Nan)))
    }

    /// Append a collision-free transient grouping identity for in-memory
    /// composite keys. This is not a storage codec or a stable byte format.
    pub fn append_grouping_key(self, out: &mut Vec<u8>) {
        match self.0 {
            Number::Finite {
                negative,
                significand,
                exponent,
                fives,
            } => {
                out.extend_from_slice(&[0, u8::from(negative)]);
                out.extend_from_slice(&significand.to_le_bytes());
                out.extend_from_slice(&exponent.to_le_bytes());
                out.extend_from_slice(&fives.to_le_bytes());
            }
            Number::Infinity(negative) => out.extend_from_slice(&[1, u8::from(negative)]),
            Number::Nan => out.push(2),
        }
    }
}

fn finite(mut negative: bool, mut significand: u128, mut exponent: i32, mut fives: u32) -> Number {
    if significand == 0 {
        negative = false;
        exponent = 0;
        fives = 0;
    } else {
        while fives > 0 && significand.is_multiple_of(5) {
            significand /= 5;
            fives -= 1;
        }
        let twos = significand.trailing_zeros();
        significand >>= twos;
        exponent += twos as i32;
    }
    Number::Finite {
        negative,
        significand,
        exponent,
        fives,
    }
}

fn binary(value: f64) -> Number {
    let bits = value.to_bits();
    let negative = bits >> 63 != 0;
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    if exponent == 0x7ff {
        if fraction == 0 {
            Number::Infinity(negative)
        } else {
            Number::Nan
        }
    } else if exponent == 0 {
        finite(negative, u128::from(fraction), -1074, 0)
    } else {
        finite(
            negative,
            u128::from(fraction | (1_u64 << 52)),
            exponent - 1075,
            0,
        )
    }
}

fn magnitude_cmp(am: u128, ae: i32, af: u32, bm: u128, be: i32, bf: u32) -> Ordering {
    if am == 0 || bm == 0 {
        return am.cmp(&bm);
    }
    if af == bf {
        return ((128 - am.leading_zeros()) as i32 + ae)
            .cmp(&((128 - bm.leading_zeros()) as i32 + be))
            .then_with(|| (am << am.leading_zeros()).cmp(&(bm << bm.leading_zeros())));
    }
    // At most 128 + ceil(log2(5^28)) = 194 significant bits. No heap,
    // unbounded bigint, decimal conversion, or exponent-sized shift is needed.
    let mut a = Wide::new(am);
    let mut b = Wide::new(bm);
    if bf > af {
        a.mul_fives(bf - af);
    } else {
        b.mul_fives(af - bf);
    }
    (a.bits() as i32 + ae)
        .cmp(&(b.bits() as i32 + be))
        .then_with(|| a.normalized().cmp(&b.normalized()))
}

struct Wide([u64; 4]);

impl Wide {
    fn new(value: u128) -> Self {
        Self([value as u64, (value >> 64) as u64, 0, 0])
    }

    fn mul_fives(&mut self, count: u32) {
        for _ in 0..count {
            let mut carry = 0_u128;
            for limb in &mut self.0 {
                let product = u128::from(*limb) * 5 + carry;
                *limb = product as u64;
                carry = product >> 64;
            }
            debug_assert_eq!(carry, 0);
        }
    }

    fn bits(&self) -> u32 {
        for i in (0..4).rev() {
            if self.0[i] != 0 {
                return i as u32 * 64 + 64 - self.0[i].leading_zeros();
            }
        }
        0
    }

    fn normalized(&self) -> [u64; 4] {
        let shift = 256 - self.bits();
        let words = (shift / 64) as usize;
        let bits = shift % 64;
        let mut result = [0; 4];
        for i in words..4 {
            result[3 - i] = self.0[i - words] << bits;
            if bits != 0 && i > words {
                result[3 - i] |= self.0[i - words - 1] >> (64 - bits);
            }
        }
        result
    }
}

#[cfg(test)]
#[path = "numeric_value_tests.rs"]
mod tests;
