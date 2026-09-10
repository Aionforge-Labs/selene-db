//! Adapter from legacy flat property tags into the structural type service.

use crate::{
    DurationTypeQualifier as D, PropertyValueType as P, ScalarType as S, StructuralType as T,
    TypeKind as K,
};

impl P {
    /// Interpret a legacy container/scalar tag as a structural descriptor.
    /// A list tag alone carries no element constraint; it admits supported
    /// property values, not arbitrary query-only runtime values.
    #[must_use]
    pub fn structural_type(self) -> T {
        let scalar = match self {
            P::List => {
                return T::list(
                    T::new(K::Property, true).expect("property descriptor"),
                    None,
                )
                .expect("one list level");
            }
            P::Record | P::RecordTyped => {
                return T::new(K::Record(None), true).expect("open record");
            }
            P::NodeRef => return T::NODE,
            P::EdgeRef => return T::EDGE,
            P::Path => return T::PATH,
            P::GraphRef => return T::new(K::GraphRef, true).expect("graph reference"),
            P::TableRef => return T::new(K::TableRef(None), true).expect("table reference"),
            P::Null => return T::NULL,
            P::Bool => S::Boolean,
            P::Int => S::Int64,
            P::Uint => S::Uint64,
            P::Int128 => S::Int128,
            P::Uint128 => S::Uint128,
            P::Float => S::Float64,
            P::Float32 => S::Float32,
            P::Decimal => S::Decimal(None),
            P::String => S::String(None),
            P::Bytes => S::Bytes(None),
            P::ZonedDateTime => S::ZonedDateTime,
            P::LocalDateTime => S::LocalDateTime,
            P::Date => S::Date,
            P::ZonedTime => S::ZonedTime,
            P::LocalTime => S::LocalTime,
            P::Duration => S::Duration(None),
            P::DurationYearToMonth => S::Duration(Some(D::YearToMonth)),
            P::DurationDayToSecond => S::Duration(Some(D::DayToSecond)),
            P::Uuid => S::Uuid,
            P::Json => S::Json,
            P::Vector => S::Vector,
        };
        T::from_scalar(scalar).expect("unconstrained scalar descriptor")
    }
}

impl T {
    /// Whether this descriptor uses only property-admitted families.
    /// Value admission is still recursive and mandatory for open containers.
    #[must_use]
    pub fn is_storable_descriptor(&self) -> bool {
        match self.kind() {
            K::Scalar(_) | K::Property | K::Null | K::Record(None) => true,
            K::List { element, .. } => element.is_storable_descriptor(),
            K::Union(members) => members.iter().all(T::is_storable_descriptor),
            K::Record(Some(fields)) => fields.iter().all(|(_, ty)| ty.is_storable_descriptor()),
            K::Dynamic
            | K::Empty
            | K::NodeRef
            | K::EdgeRef
            | K::Path
            | K::GraphRef
            | K::TableRef(_) => false,
        }
    }
}
