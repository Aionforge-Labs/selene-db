//! Physical path payload: immutable logical automata and resolved predicates.

use crate::{BindingId, BindingTableSchema, PathAutomaton, ValueExpr};
use selene_core::DbString;

/// One MATCH clause's path program, transported from logical planning.
#[derive(Clone, Debug)]
pub struct PathProgram {
    /// Complete clause automata, in source pattern order.
    pub automata: Vec<PathAutomaton>,
    /// Analyzer identities in `schema` order.
    pub bindings: Vec<BindingId>,
    /// Pattern bindings declared outside this clause (correlated inputs).
    pub input_bindings: Vec<BindingId>,
    /// Typed named bindings, including conditional singletons and groups.
    pub schema: BindingTableSchema,
    /// Per-pattern, per-element predicates resolved against semantic IDs.
    pub conditions: Vec<Vec<PathConditions>>,
}

/// Predicates evaluated against a complete binding before path selection.
#[derive(Clone, Debug)]
pub struct PathConditions {
    /// Property equality tests, evaluated for each traversed element.
    pub properties: Vec<(DbString, ValueExpr)>,
    /// Inline condition, evaluated once at the element's binding degree.
    pub inline: Option<ValueExpr>,
}

impl PathConditions {
    pub(crate) fn is_empty(&self) -> bool {
        self.properties.is_empty() && self.inline.is_none()
    }
}
