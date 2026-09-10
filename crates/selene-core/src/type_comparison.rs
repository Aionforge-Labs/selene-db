//! Operation-specific comparability over normalized structural descriptors.

use crate::{ScalarType as S, StructuralType as T, TypeKind as K};

/// The operation whose comparability contract is being checked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComparisonMode {
    /// Three-valued equality predicates; path comparison is unsupported (GA09).
    PredicateEquality,
    /// Not-distinct grouping and duplicate elimination, including paths.
    Distinctness,
    /// Ordering; native JSON has no selected order.
    Ordering,
}

impl T {
    /// Whether these declared families permit the selected operation. Dynamic
    /// cells and open records defer their concrete checks to execution. This
    /// checks neither assignment conversion nor referent liveness.
    #[must_use]
    pub fn comparable_with(&self, rhs: &Self, mode: ComparisonMode) -> bool {
        use ComparisonMode::{Ordering, PredicateEquality};
        if matches!(self.kind(), K::Dynamic | K::Property | K::Null | K::Empty)
            || matches!(rhs.kind(), K::Dynamic | K::Property | K::Null | K::Empty)
        {
            return true;
        }
        match (self.kind(), rhs.kind()) {
            (K::Union(members), _) => members.iter().all(|ty| ty.comparable_with(rhs, mode)),
            (_, K::Union(members)) => members.iter().all(|ty| self.comparable_with(ty, mode)),
            (K::Scalar(lhs), K::Scalar(rhs)) => {
                scalar_family(*lhs) == scalar_family(*rhs)
                    && !(mode == Ordering && matches!(lhs, S::Json))
                    && !matches!((lhs, rhs), (S::Duration(Some(a)), S::Duration(Some(b))) if a != b)
            }
            (K::List { element: lhs, .. }, K::List { element: rhs, .. }) => {
                lhs.comparable_with(rhs, mode)
            }
            (K::Record(Some(lhs)), K::Record(Some(rhs))) => {
                lhs.len() == rhs.len()
                    && lhs
                        .iter()
                        .zip(rhs.iter())
                        .all(|((ln, lt), (rn, rt))| ln == rn && lt.comparable_with(rt, mode))
            }
            (K::Record(_), K::Record(_)) => true,
            (K::Path, K::Path) => mode != PredicateEquality,
            (K::NodeRef, K::NodeRef)
            | (K::EdgeRef, K::EdgeRef)
            | (K::GraphRef, K::GraphRef)
            | (K::TableRef(_), K::TableRef(_)) => true,
            _ => false,
        }
    }
}

fn scalar_family(scalar: S) -> u8 {
    match scalar {
        S::Boolean => 0,
        S::Int8
        | S::Int16
        | S::Int32
        | S::Int64
        | S::Int128
        | S::Uint8
        | S::Uint16
        | S::Uint32
        | S::Uint64
        | S::Uint128
        | S::Float
        | S::Float32
        | S::Float64
        | S::Decimal(_) => 1,
        S::String(_) => 2,
        S::Bytes(_) => 3,
        S::Uuid => 4,
        S::Json => 5,
        S::Vector => 6,
        S::Date => 7,
        S::LocalDateTime => 8,
        S::ZonedDateTime => 9,
        S::LocalTime => 10,
        S::ZonedTime => 11,
        S::Duration(_) => 12,
    }
}
