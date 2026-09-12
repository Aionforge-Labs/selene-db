//! Planned expression-subquery IR.

use std::collections::BTreeMap;

use selene_core::DbString;

use crate::{
    SourceSpan,
    analyze::{BindingId, ExprId},
};

use super::{BindingTableColumn, ExecutionPlan, PatternPlan};

/// Planned subquery referenced by an expression ID in a containing plan.
///
/// `Exists` follows ISO/IEC 39075:2024 section 19.4.
#[derive(Clone, Debug)]
pub struct PlannedSubquery {
    /// Subquery expression kind.
    pub kind: SubqueryKind,
    /// Lowered body executed for the subquery.
    pub body: SubqueryBody,
    /// Outer-scope bindings referenced by the inner pattern, sorted and deduped.
    pub outer_binding_refs: Vec<OuterBindingRef>,
    /// Source span of the subquery expression.
    pub span: SourceSpan,
}

/// One outer-scope binding referenced from inside a planned subquery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OuterBindingRef {
    /// Analyzer binding ID for the outer declaration.
    pub binding: BindingId,
    /// Database-string source name used to project the value into the subquery seed.
    pub name: DbString,
}

/// Planned expression-subquery kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubqueryKind {
    /// ISO GQL `EXISTS` predicate.
    Exists {
        /// Whether the source was `NOT EXISTS`.
        negated: bool,
    },
    /// ISO GQL `VALUE { ... }` scalar value query expression.
    Value,
}

/// Lowered expression-subquery body.
#[derive(Clone, Debug)]
pub enum SubqueryBody {
    /// Existing single-MATCH pattern body used by EXISTS.
    Pattern(Box<PatternPlan>),
    /// Full query pipeline body used by VALUE.
    Plan(Box<ExecutionPlan>),
}

/// Planned inline `CALL { ... }` table subquery.
#[derive(Clone, Debug)]
pub struct PlannedTableSubquery {
    /// Whether an empty body result preserves the input row with null yields.
    pub optional: bool,
    /// Full query body executed once per input row.
    pub body: Box<ExecutionPlan>,
    /// Outer-scope bindings referenced by the body, sorted and deduped.
    pub outer_binding_refs: Vec<OuterBindingRef>,
    /// Source-to-output yield projection. Empty means preserve no inner columns.
    pub yield_items: Vec<PlannedTableSubqueryYield>,
    /// Columns appended to the outer binding table.
    pub yield_schema: Vec<BindingTableColumn>,
    /// Source span of the inline CALL.
    pub span: SourceSpan,
}

/// One yielded column from a planned inline CALL body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedTableSubqueryYield {
    /// Column name in the inner body result.
    pub source: DbString,
    /// Output column name after optional `AS` rename.
    pub output: DbString,
    /// Source span of the yield item.
    pub span: SourceSpan,
}

/// Plan-level registry of expression subqueries indexed by AST expression ID.
///
/// The analyzer allocates expression IDs, lowering clones that lookup table
/// onto the execution plan, and runtime evaluation uses this registry as the
/// single source of truth for subquery bodies.
#[derive(Clone, Debug, Default)]
pub struct SubqueryRegistry {
    by_expr_id: BTreeMap<ExprId, PlannedSubquery>,
}

impl SubqueryRegistry {
    /// Insert or replace the planned subquery for `expr_id`.
    pub fn insert(&mut self, expr_id: ExprId, subquery: PlannedSubquery) {
        self.by_expr_id.insert(expr_id, subquery);
    }

    /// Return the planned subquery for `expr_id`, if any.
    #[must_use]
    pub fn get(&self, expr_id: ExprId) -> Option<&PlannedSubquery> {
        self.by_expr_id.get(&expr_id)
    }

    /// Iterate planned subqueries in expression-id order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &PlannedSubquery> {
        self.by_expr_id.values()
    }

    /// Return true when no subqueries were planned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_expr_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        PatternPlan, SourceSpan,
        analyze::ExprId,
        plan::{JoinTree, PlannedSubquery, SubqueryBody, SubqueryKind, SubqueryRegistry},
    };

    fn planned_subquery() -> PlannedSubquery {
        PlannedSubquery {
            kind: SubqueryKind::Exists { negated: false },
            body: SubqueryBody::Pattern(Box::new(PatternPlan {
                bindings: Vec::new(),
                join_tree: JoinTree::WorstCaseOptimal {
                    intersection: Vec::new(),
                    node_id_ordering: Vec::new(),
                },
                filters: Vec::new(),
            })),
            outer_binding_refs: Vec::new(),
            span: SourceSpan::default(),
        }
    }

    #[test]
    fn registry_insert_get_and_is_empty() {
        let mut registry = SubqueryRegistry::default();
        let expr_id = ExprId::new(7);

        assert!(registry.is_empty());
        registry.insert(expr_id, planned_subquery());

        assert!(!registry.is_empty());
        assert!(matches!(
            registry.get(expr_id).map(|subquery| subquery.kind),
            Some(SubqueryKind::Exists { negated: false })
        ));
        assert!(registry.get(ExprId::new(8)).is_none());
    }
}
