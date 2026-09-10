//! A hash join compares across its two inputs, never within one input. Each
//! position retains the observed family choices so nulls remain flexible.

use super::{
    ExecutorError,
    comparison_domain::{incomparable, leaf_type},
};
use selene_core::{ComparisonMode, DbString, Record, StructuralType, Value};
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct JoinDomain(Vec<Choices>);

impl JoinDomain {
    pub(crate) fn observe(&mut self, row: &[Value]) -> Result<(), ExecutorError> {
        self.0.resize_with(row.len(), Choices::default);
        for (choices, value) in self.0.iter_mut().zip(row) {
            choices.observe(value, 1)?;
        }
        Ok(())
    }

    pub(crate) fn compare(&self, rhs: &Self) -> Result<(), ExecutorError> {
        if self.0.iter().zip(&rhs.0).all(|(a, b)| a.comparable(b)) {
            Ok(())
        } else {
            Err(incomparable())
        }
    }
}

#[derive(Default)]
struct Choices {
    leaves: Vec<StructuralType>,
    list: Option<Vec<Self>>,
    record: Option<BTreeMap<DbString, Self>>,
    different_field_sets: bool,
}

impl Choices {
    fn observe(&mut self, value: &Value, depth: usize) -> Result<(), ExecutorError> {
        if depth > selene_core::MAX_STRUCTURAL_TYPE_DEPTH {
            return Err(incomparable());
        }
        match value {
            Value::Null => {}
            Value::List(values) => {
                let items = self.list.get_or_insert_default();
                if items.len() < values.len() {
                    items.resize_with(values.len(), Self::default);
                }
                for (item, value) in items.iter_mut().zip(values) {
                    item.observe(value, depth + 1)?;
                }
            }
            Value::Record(record) => {
                let Record::Open(fields) = record.as_ref() else {
                    return Err(incomparable());
                };
                let observed = self.record.get_or_insert_with(|| {
                    fields
                        .iter()
                        .map(|(name, _)| (name.clone(), Self::default()))
                        .collect()
                });
                if fields.len() != observed.len()
                    || fields.iter().any(|(name, _)| !observed.contains_key(name))
                {
                    self.different_field_sets = true;
                } else {
                    for (name, value) in fields {
                        observed
                            .get_mut(name)
                            .expect("checked field")
                            .observe(value, depth + 1)?;
                    }
                }
            }
            _ => {
                let ty = leaf_type(value)?;
                if !self.leaves.contains(&ty) {
                    self.leaves.push(ty);
                }
            }
        }
        Ok(())
    }

    fn empty(&self) -> bool {
        self.leaves.is_empty() && self.list.is_none() && self.record.is_none()
    }

    fn comparable(&self, rhs: &Self) -> bool {
        if self.empty() || rhs.empty() {
            return true;
        }
        if self.list.is_some() && (!rhs.leaves.is_empty() || rhs.record.is_some())
            || rhs.list.is_some() && (!self.leaves.is_empty() || self.record.is_some())
            || self.record.is_some() && !rhs.leaves.is_empty()
            || rhs.record.is_some() && !self.leaves.is_empty()
        {
            return false;
        }
        if !self.leaves.iter().all(|a| {
            rhs.leaves
                .iter()
                .all(|b| a.comparable_with(b, ComparisonMode::PredicateEquality))
        }) {
            return false;
        }
        if let (Some(a), Some(b)) = (&self.list, &rhs.list)
            && !a.iter().zip(b).all(|(a, b)| a.comparable(b))
        {
            return false;
        }
        if let (Some(a), Some(b)) = (&self.record, &rhs.record) {
            return !self.different_field_sets
                && !rhs.different_field_sets
                && a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|((an, a), (bn, b))| an == bn && a.comparable(b));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_domain_matches_pairwise_join_predicates_with_nulls() {
        let integer = Value::Int(1);
        let text = Value::String(selene_core::db_string("one").unwrap());
        for nested in [false, true] {
            let wrap = |value| {
                if nested {
                    Value::List(vec![value])
                } else {
                    value
                }
            };
            let build = [wrap(integer.clone()), wrap(text.clone())];
            for probe in [wrap(Value::Null), wrap(integer.clone())] {
                let mut domain = JoinDomain::default();
                for value in &build {
                    domain.observe(std::slice::from_ref(value)).unwrap();
                }
                let mut rhs = JoinDomain::default();
                rhs.observe(std::slice::from_ref(&probe)).unwrap();
                let oracle = build.iter().all(|value| {
                    crate::runtime::pattern::key_values_equal(
                        std::slice::from_ref(value),
                        std::slice::from_ref(&probe),
                    )
                    .is_ok()
                });
                assert_eq!(domain.compare(&rhs).is_ok(), oracle);
            }
        }
    }
}
