//! Shared value-domain validation for query comparison and graph constraints.

use crate::{
    ComparisonMode, DbString, DurationTypeQualifier as Q, DurationValueFamily as D, Record,
    ScalarType as S, StructuralType as T, TypeKind as K, Value,
};
use std::collections::BTreeMap;

/// A value cannot join the observed operation-specific comparison domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ValueComparisonError {
    /// Values do not share a selected comparison family or recursive shape.
    #[error("values are not comparable for this operation")]
    NotComparable,
    /// Recursive value nesting exceeds the structural limit.
    #[error("comparison nesting limit exceeded")]
    TooDeep,
}

/// Bounded observed comparison shape, retaining no values or graph ownership.
/// Use one domain per compared column or UNIQUE entity/type/property domain.
/// After an error, discard the domain; observation need not be transactional.
#[derive(Default)]
pub struct ValueComparisonDomain(Domain);

impl ValueComparisonDomain {
    /// Observe one value before infallible grouping, hashing or ordering.
    pub fn observe(
        &mut self,
        value: &Value,
        mode: ComparisonMode,
    ) -> Result<(), ValueComparisonError> {
        self.0.observe(value, mode, 1)
    }
}

#[derive(Default)]
enum Domain {
    #[default]
    Unknown,
    Leaf(T),
    List(Vec<Domain>),
    Record(BTreeMap<DbString, Domain>),
}

impl Domain {
    fn observe(
        &mut self,
        value: &Value,
        mode: ComparisonMode,
        depth: usize,
    ) -> Result<(), ValueComparisonError> {
        if depth > crate::MAX_STRUCTURAL_TYPE_DEPTH {
            return Err(ValueComparisonError::TooDeep);
        }
        if matches!(value, Value::Null) {
            return Ok(());
        }
        match value {
            Value::List(values) => {
                if matches!(self, Self::Unknown) {
                    *self = Self::List(Vec::new());
                }
                let Self::List(domains) = self else {
                    return Err(ValueComparisonError::NotComparable);
                };
                if domains.len() < values.len() {
                    domains.resize_with(values.len(), Self::default);
                }
                for (domain, value) in domains.iter_mut().zip(values) {
                    domain.observe(value, mode, depth + 1)?;
                }
            }
            Value::Record(record) => {
                let Record::Open(fields) = record.as_ref();
                if matches!(self, Self::Unknown) {
                    *self = Self::Record(
                        fields
                            .iter()
                            .map(|(name, _)| (name.clone(), Self::Unknown))
                            .collect(),
                    );
                }
                let Self::Record(domains) = self else {
                    return Err(ValueComparisonError::NotComparable);
                };
                if domains.len() != fields.len() {
                    return Err(ValueComparisonError::NotComparable);
                }
                for (name, value) in fields {
                    domains
                        .get_mut(name)
                        .ok_or(ValueComparisonError::NotComparable)?
                        .observe(value, mode, depth + 1)?;
                }
            }
            _ => {
                let ty = comparison_leaf_type(value)?;
                if !ty.comparable_with(&ty, mode) {
                    return Err(ValueComparisonError::NotComparable);
                }
                match self {
                    Self::Unknown => *self = Self::Leaf(ty),
                    Self::Leaf(prior) if prior.comparable_with(&ty, mode) => {
                        // A zero duration may precede either concrete unit group.
                        if matches!(prior.kind(), K::Scalar(S::Duration(None))) {
                            *prior = ty;
                        }
                    }
                    _ => return Err(ValueComparisonError::NotComparable),
                }
            }
        }
        Ok(())
    }
}

/// Classify a scalar/reference leaf for operation-specific comparison.
/// Constructed values and unsupported carriers return `NotComparable`.
#[doc(hidden)]
pub fn comparison_leaf_type(value: &Value) -> Result<T, ValueComparisonError> {
    let scalar = match value {
        Value::Bool(_) => S::Boolean,
        Value::Int(_)
        | Value::Int128(_)
        | Value::Uint(_)
        | Value::Uint128(_)
        | Value::Float(_)
        | Value::Float32(_)
        | Value::Decimal(_) => return Ok(T::INT64),
        Value::String(_) => S::String(None),
        Value::Bytes(_) => S::Bytes(None),
        Value::Uuid(_) => S::Uuid,
        Value::Json(_) => S::Json,
        Value::Vector(_) => S::Vector,
        Value::Date(_) => S::Date,
        Value::LocalDateTime(_) => S::LocalDateTime,
        Value::ZonedDateTime(_) => S::ZonedDateTime,
        Value::LocalTime(_) => S::LocalTime,
        Value::ZonedTime(_) => S::ZonedTime,
        Value::Duration(span) => S::Duration(match crate::duration_value_family(span) {
            Some(D::YearMonth) => Some(Q::YearToMonth),
            Some(D::DayTime) => Some(Q::DayToSecond),
            Some(D::Zero) => None,
            None => return Err(ValueComparisonError::NotComparable),
        }),
        Value::NodeRef(_) => return Ok(T::NODE),
        Value::EdgeRef(_) => return Ok(T::EDGE),
        Value::GraphRef(_) => return Ok(T::new(K::GraphRef, true).expect("leaf type")),
        Value::Path(_) => return Ok(T::PATH),
        Value::TableRef(_) => return Ok(T::new(K::TableRef(None), true).expect("leaf type")),
        _ => return Err(ValueComparisonError::NotComparable),
    };
    Ok(T::from_scalar(scalar).expect("unbounded scalar type"))
}
