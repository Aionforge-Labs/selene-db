//! Removable comparison-domain evidence. Cost follows the changed value's shape,
//! not the number of elements already in the constraint index.

use immutable_chunkmap::map::MapM;
use selene_core::{
    ComparisonMode, DbString, StructuralType, Value, ValueComparisonError as E,
    comparison_leaf_type,
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Step {
    Component(usize),
    Position(usize),
    Field(DbString),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Shape {
    Leaf(StructuralType),
    List,
    Record(Vec<DbString>),
}

impl Shape {
    fn compatible(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Leaf(a), Self::Leaf(b)) => a.comparable_with(b, ComparisonMode::Distinctness),
            _ => self == other,
        }
    }
}

/// At any path there is one compatible family (plus the zero-duration wildcard).
/// The small vector retains counts so deleting the last witness releases its
/// domain, including nested list positions and record fields.
#[derive(Clone, Debug, Default)]
pub(super) struct Domains(MapM<Vec<Step>, Vec<(Shape, usize)>>);

impl Domains {
    pub(super) fn change(&mut self, values: &[&Value], insert: bool) -> Result<(), E> {
        for (component, value) in values.iter().enumerate() {
            self.visit(value, &mut vec![Step::Component(component)], insert, 1)?;
        }
        Ok(())
    }

    fn visit(
        &mut self,
        value: &Value,
        path: &mut Vec<Step>,
        insert: bool,
        depth: usize,
    ) -> Result<(), E> {
        if depth > selene_core::MAX_STRUCTURAL_TYPE_DEPTH {
            return Err(E::TooDeep);
        }
        let shape = match value {
            Value::Null => return Ok(()),
            Value::List(_) => Shape::List,
            Value::Record(record) => {
                let selene_core::Record::Open(fields) = record.as_ref() else {
                    return Err(E::NotComparable);
                };
                let mut names: Vec<_> = fields.iter().map(|(key, _)| key.clone()).collect();
                names.sort();
                if names.windows(2).any(|p| p[0] == p[1]) {
                    return Err(E::NotComparable);
                }
                Shape::Record(names)
            }
            _ => Shape::Leaf(comparison_leaf_type(value)?),
        };
        if !shape.compatible(&shape) {
            return Err(E::NotComparable);
        }
        let mut counts = self.0.get(path).cloned().unwrap_or_default();
        if insert {
            if counts.iter().any(|(prior, _)| !prior.compatible(&shape)) {
                return Err(E::NotComparable);
            }
            if let Some((_, count)) = counts.iter_mut().find(|(prior, _)| prior == &shape) {
                *count += 1;
            } else {
                counts.push((shape, 1));
            }
        } else {
            let position = counts
                .iter()
                .position(|(prior, _)| prior == &shape)
                .expect("complete constraint domain contains the removed value");
            counts[position].1 -= 1;
            if counts[position].1 == 0 {
                counts.swap_remove(position);
            }
        }
        if counts.is_empty() {
            self.0.remove_cow(path);
        } else {
            self.0.insert_cow(path.clone(), counts);
        }
        match value {
            Value::List(values) => {
                for (position, value) in values.iter().enumerate() {
                    path.push(Step::Position(position));
                    self.visit(value, path, insert, depth + 1)?;
                    path.pop();
                }
            }
            Value::Record(record) => {
                let selene_core::Record::Open(fields) = record.as_ref() else {
                    return Err(E::NotComparable);
                };
                for (name, value) in fields {
                    path.push(Step::Field(name.clone()));
                    self.visit(value, path, insert, depth + 1)?;
                    path.pop();
                }
            }
            _ => {}
        }
        Ok(())
    }
}
