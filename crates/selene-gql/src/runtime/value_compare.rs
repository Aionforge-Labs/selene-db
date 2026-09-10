//! Runtime predicate, grouping, and ordering operations.
//!
//! Numeric semantics come from the core's exact numeric representation, not
//! lossless conversion gates or rounded decimal approximations of binary values.

use std::cmp::Ordering;

use selene_core::{NumericKey, Record, RecordTyped, Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub(crate) enum NullSortOrder {
    First,
    Last,
}

pub(crate) fn equal_non_null(lhs: &Value, rhs: &Value) -> bool {
    debug_assert!(!matches!(lhs, Value::Null));
    debug_assert!(!matches!(rhs, Value::Null));
    if let (Some(lhs), Some(rhs)) = (NumericKey::of(lhs), NumericKey::of(rhs)) {
        return lhs == rhs;
    }
    match (lhs, rhs) {
        (Value::Duration(lhs), Value::Duration(rhs)) => {
            selene_core::duration_order_key(lhs) == selene_core::duration_order_key(rhs)
        }
        (Value::Record(lhs), Value::Record(rhs)) => match (lhs.as_ref(), rhs.as_ref()) {
            (Record::Open(lhs), Record::Open(rhs)) => {
                lhs.len() == rhs.len()
                    && lhs.iter().all(|(name, value)| {
                        rhs.iter()
                            .find(|(other, _)| name == other)
                            .is_some_and(|(_, other)| value_key_equal(value, other))
                    })
            }
            _ => lhs == rhs,
        },
        (Value::Path(lhs), Value::Path(rhs)) => path_compare(lhs, rhs).is_eq(),
        (Value::List(lhs), Value::List(rhs)) => {
            lhs.len() == rhs.len() && lhs.iter().zip(rhs).all(|(a, b)| value_key_equal(a, b))
        }
        _ => lhs == rhs,
    }
}

fn value_key_equal(lhs: &Value, rhs: &Value) -> bool {
    match (lhs, rhs) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => equal_non_null(lhs, rhs),
    }
}

pub(crate) fn gql_equal_non_null(lhs: &Value, rhs: &Value) -> Option<bool> {
    debug_assert!(!matches!(lhs, Value::Null));
    debug_assert!(!matches!(rhs, Value::Null));
    if let (Some(lhs), Some(rhs)) = (NumericKey::of(lhs), NumericKey::of(rhs)) {
        return lhs.predicate_cmp(rhs).map(Ordering::is_eq);
    }
    match (lhs, rhs) {
        (Value::List(lhs), Value::List(rhs)) => {
            if lhs.len() != rhs.len() {
                return Some(false);
            }
            combine_equal(lhs.iter().zip(rhs).map(|(a, b)| gql_equal(a, b)))
        }
        (Value::Record(lhs), Value::Record(rhs)) => match (lhs.as_ref(), rhs.as_ref()) {
            (Record::Open(lhs), Record::Open(rhs)) => {
                if lhs.len() != rhs.len() {
                    return Some(false);
                }
                combine_equal(lhs.iter().map(|(name, value)| {
                    rhs.iter()
                        .find(|(other, _)| name == other)
                        .map_or(Some(false), |(_, other)| gql_equal(value, other))
                }))
            }
            _ => Some(false),
        },
        (Value::RecordTyped(lhs), Value::RecordTyped(rhs)) => {
            if lhs.type_id != rhs.type_id || lhs.values.len() != rhs.values.len() {
                return Some(false);
            }
            combine_equal(
                lhs.values
                    .iter()
                    .zip(&rhs.values)
                    .map(|(a, b)| match (a, b) {
                        (Some(a), Some(b)) => gql_equal(a, b),
                        _ => None,
                    }),
            )
        }
        _ => Some(equal_non_null(lhs, rhs)),
    }
}

fn combine_equal(pairs: impl Iterator<Item = Option<bool>>) -> Option<bool> {
    let mut unknown = false;
    for pair in pairs {
        match pair {
            Some(false) => return Some(false),
            None => unknown = true,
            Some(true) => {}
        }
    }
    if unknown { None } else { Some(true) }
}

fn gql_equal(lhs: &Value, rhs: &Value) -> Option<bool> {
    match (lhs, rhs) {
        (Value::Null, _) | (_, Value::Null) => None,
        _ => gql_equal_non_null(lhs, rhs),
    }
}

pub(crate) fn compare_non_null(lhs: &Value, rhs: &Value) -> Option<Ordering> {
    debug_assert!(!matches!(lhs, Value::Null));
    debug_assert!(!matches!(rhs, Value::Null));
    compare_value_pair(lhs, rhs)
}

pub(crate) fn compare_for_sort(lhs: &Value, rhs: &Value, nulls: NullSortOrder) -> Ordering {
    match (lhs, rhs) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => match nulls {
            NullSortOrder::First => Ordering::Less,
            NullSortOrder::Last => Ordering::Greater,
        },
        (_, Value::Null) => match nulls {
            NullSortOrder::First => Ordering::Greater,
            NullSortOrder::Last => Ordering::Less,
        },
        _ => {
            if let (Some(lhs), Some(rhs)) = (NumericKey::of(lhs), NumericKey::of(rhs)) {
                return lhs.sort_cmp(rhs);
            }
            match (lhs, rhs) {
                (Value::List(lhs), Value::List(rhs)) => sort_list(lhs, rhs),
                (Value::Record(lhs), Value::Record(rhs)) => {
                    let (Record::Open(lhs), Record::Open(rhs)) = (lhs.as_ref(), rhs.as_ref())
                    else {
                        unreachable!("record domain validated");
                    };
                    let mut lhs: Vec<_> = lhs.iter().collect();
                    let mut rhs: Vec<_> = rhs.iter().collect();
                    lhs.sort_by(|a, b| a.0.cmp(&b.0));
                    rhs.sort_by(|a, b| a.0.cmp(&b.0));
                    lhs.iter()
                        .zip(&rhs)
                        .map(|((an, av), (bn, bv))| {
                            an.cmp(bn)
                                .then_with(|| compare_for_sort(av, bv, NullSortOrder::Last))
                        })
                        .find(|order| !order.is_eq())
                        .unwrap_or_else(|| lhs.len().cmp(&rhs.len()))
                }
                _ => {
                    compare_value_pair(lhs, rhs).expect("sort domains validated before comparison")
                }
            }
        }
    }
}

fn compare_value_pair(lhs: &Value, rhs: &Value) -> Option<Ordering> {
    if let (Some(lhs), Some(rhs)) = (NumericKey::of(lhs), NumericKey::of(rhs)) {
        return lhs.predicate_cmp(rhs);
    }
    Some(match (lhs, rhs) {
        (Value::Bool(lhs), Value::Bool(rhs)) => lhs.cmp(rhs),
        (Value::String(lhs), Value::String(rhs)) => lhs.as_str().cmp(rhs.as_str()),
        (Value::Date(lhs), Value::Date(rhs)) => lhs.cmp(rhs),
        (Value::LocalDateTime(lhs), Value::LocalDateTime(rhs)) => lhs.cmp(rhs),
        (Value::ZonedDateTime(lhs), Value::ZonedDateTime(rhs)) => lhs.cmp(rhs),
        (Value::LocalTime(lhs), Value::LocalTime(rhs)) => lhs.cmp(rhs),
        (Value::ZonedTime(lhs), Value::ZonedTime(rhs)) => lhs.cmp(rhs),
        (Value::Duration(lhs), Value::Duration(rhs)) => {
            selene_core::duration_order_key(lhs).cmp(&selene_core::duration_order_key(rhs))
        }
        (Value::Bytes(lhs), Value::Bytes(rhs)) => lhs.as_ref().cmp(rhs.as_ref()),
        (Value::Uuid(lhs), Value::Uuid(rhs)) => lhs.cmp(rhs),
        (Value::NodeRef(lhs), Value::NodeRef(rhs)) => lhs.cmp(rhs),
        (Value::EdgeRef(lhs), Value::EdgeRef(rhs)) => lhs.cmp(rhs),
        (Value::GraphRef(lhs), Value::GraphRef(rhs)) => lhs.cmp(rhs),
        (Value::TableRef(lhs), Value::TableRef(rhs)) => lhs.cmp(rhs),
        (Value::Path(lhs), Value::Path(rhs)) => path_compare(lhs, rhs),
        (Value::List(lhs), Value::List(rhs)) => return list_compare(lhs, rhs),
        (Value::Record(lhs), Value::Record(rhs)) => return record_compare(lhs, rhs),
        (Value::RecordTyped(lhs), Value::RecordTyped(rhs)) => {
            return typed_record_compare(lhs, rhs);
        }
        (Value::Vector(lhs), Value::Vector(rhs)) => {
            for (a, b) in lhs.as_slice().iter().zip(rhs.as_slice()) {
                if a != b {
                    return Some(a.total_cmp(b));
                }
            }
            lhs.dimension().cmp(&rhs.dimension())
        }
        _ => return None,
    })
}

fn record_compare(lhs: &Record, rhs: &Record) -> Option<Ordering> {
    match (lhs, rhs) {
        (Record::Open(lhs), Record::Open(rhs)) => {
            let mut lhs: Vec<_> = lhs.iter().collect();
            let mut rhs: Vec<_> = rhs.iter().collect();
            lhs.sort_by(|a, b| a.0.cmp(&b.0));
            rhs.sort_by(|a, b| a.0.cmp(&b.0));
            for ((an, av), (bn, bv)) in lhs.iter().zip(&rhs) {
                let names = an.cmp(bn);
                if !names.is_eq() {
                    return Some(names);
                }
                let values = compare_values(av, bv)?;
                if !values.is_eq() {
                    return Some(values);
                }
            }
            Some(lhs.len().cmp(&rhs.len()))
        }
        _ => None,
    }
}

fn list_compare(lhs: &[Value], rhs: &[Value]) -> Option<Ordering> {
    for (a, b) in lhs.iter().zip(rhs) {
        let order = compare_values(a, b)?;
        if !order.is_eq() {
            return Some(order);
        }
    }
    Some(lhs.len().cmp(&rhs.len()))
}

fn typed_record_compare(lhs: &RecordTyped, rhs: &RecordTyped) -> Option<Ordering> {
    let types = lhs.type_id.cmp(&rhs.type_id);
    if !types.is_eq() {
        return Some(types);
    }
    for (a, b) in lhs.values.iter().zip(&rhs.values) {
        let (Some(a), Some(b)) = (a, b) else {
            return None;
        };
        let order = compare_values(a, b)?;
        if !order.is_eq() {
            return Some(order);
        }
    }
    Some(lhs.values.len().cmp(&rhs.values.len()))
}

fn compare_values(lhs: &Value, rhs: &Value) -> Option<Ordering> {
    match (lhs, rhs) {
        (Value::Null, _) | (_, Value::Null) => None,
        _ => compare_non_null(lhs, rhs),
    }
}

fn sort_list(lhs: &[Value], rhs: &[Value]) -> Ordering {
    lhs.iter()
        .zip(rhs)
        .map(|(a, b)| compare_for_sort(a, b, NullSortOrder::Last))
        .find(|order| !order.is_eq())
        .unwrap_or_else(|| lhs.len().cmp(&rhs.len()))
}

fn path_compare(lhs: &selene_core::Path, rhs: &selene_core::Path) -> Ordering {
    lhs.graph
        .cmp(&rhs.graph)
        .then_with(|| lhs.start.cmp(&rhs.start))
        .then_with(|| {
            lhs.segments
                .iter()
                .zip(&rhs.segments)
                .map(|(a, b)| a.edge.cmp(&b.edge).then_with(|| a.node.cmp(&b.node)))
                .find(|order| !order.is_eq())
                .unwrap_or_else(|| lhs.segments.len().cmp(&rhs.segments.len()))
        })
}

#[cfg(test)]
#[path = "value_compare_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "value_compare_family_tests.rs"]
mod family_tests;
