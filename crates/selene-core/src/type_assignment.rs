//! Type-level assignment compatibility. This is not value membership, equality,
//! or a promise that every value fits a target's runtime bounds.

use crate::{ScalarType as S, StructuralType as T, TypeKind as K};

impl T {
    /// Whether this target permits assignment from a source family. Unknown
    /// analysis cells defer checks; scalar/list bounds and reference validity
    /// still require runtime validation. Null-only sources honor nullability.
    #[must_use]
    pub fn assignment_compatible(&self, source: &Self) -> bool {
        if matches!(source.kind(), K::Null) {
            return self.is_nullable();
        }
        if matches!(source.kind(), K::Empty | K::Dynamic) || matches!(self.kind(), K::Dynamic) {
            return true;
        }
        match (self.kind(), source.kind()) {
            (_, K::Union(members)) => members
                .iter()
                .all(|source| self.assignment_compatible(source)),
            (K::Union(members), _) => members
                .iter()
                .any(|target| target.assignment_compatible(source)),
            (K::Property, _) => source.is_storable_descriptor(),
            (K::Scalar(target), K::Scalar(source)) => scalar_assignment(*source, *target),
            (
                K::List {
                    element: target, ..
                },
                K::List {
                    element: source, ..
                },
            ) => target.assignment_compatible(source),
            (K::Record(None), K::Record(_)) | (K::TableRef(None), K::TableRef(_)) => true,
            (K::Record(Some(target)), K::Record(Some(source)))
            | (K::TableRef(Some(target)), K::TableRef(Some(source))) => {
                target.len() == source.len()
                    && target
                        .iter()
                        .zip(source.iter())
                        .all(|((tn, tt), (sn, st))| tn == sn && tt.assignment_compatible(st))
            }
            (target, source) => target == source,
        }
    }
}

fn integer(scalar: S) -> Option<(bool, u16)> {
    Some(match scalar {
        S::Int8 => (true, 8),
        S::Int16 => (true, 16),
        S::Int32 => (true, 32),
        S::Int64 => (true, 64),
        S::Int128 => (true, 128),
        S::Uint8 => (false, 8),
        S::Uint16 => (false, 16),
        S::Uint32 => (false, 32),
        S::Uint64 => (false, 64),
        S::Uint128 => (false, 128),
        _ => return None,
    })
}

fn scalar_assignment(source: S, target: S) -> bool {
    if source == target {
        return true;
    }
    if let Some((signed, width)) = integer(source) {
        if let Some((target_signed, target_width)) = integer(target) {
            return if signed == target_signed {
                width <= target_width
            } else {
                !signed && target_signed && width < target_width
            };
        }
        return matches!(target, S::Decimal(_) | S::Float | S::Float32 | S::Float64);
    }
    matches!(
        (source, target),
        (
            S::Decimal(_),
            S::Decimal(_) | S::Float | S::Float32 | S::Float64
        ) | (S::Float, S::Float32 | S::Float64)
            | (S::Float32 | S::Float64, S::Float)
            | (S::Float32, S::Float64)
            | (S::String(_), S::String(_))
            | (S::Bytes(_), S::Bytes(_))
    )
}
