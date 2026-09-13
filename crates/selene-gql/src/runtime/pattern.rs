//! Pattern schemas and binding operations shared by physical operators.

use std::collections::BTreeMap;

use selene_core::{DbString, NodeId, Value};
use smallvec::SmallVec;

use crate::{
    AnalyzedType, BindingElement, BindingId, BindingTableColumn, BindingTableSchema,
    FilterPredicate, GqlType, HiddenBindingId, JoinTree, PatternPlan, ScanKind, SubqueryRegistry,
    analyze::ExprIdLookup,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, TxContext},
};

use super::{evaluator, scan, value_compare};

/// Execute a pattern plan and produce its initial binding table.
pub fn execute_pattern(
    pattern: &PatternPlan,
    ctx: &TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let expr_ids = ExprIdLookup::default();
    let subqueries = SubqueryRegistry::default();
    let (expr_ids, subqueries) = ctx.plan_metadata().unwrap_or((&expr_ids, &subqueries));
    let eval_ctx = EvalCtx {
        tx: ctx,
        expr_ids,
        subqueries,
    };
    execute_pattern_with_seed(pattern, None, &eval_ctx)
}

/// Execute a pattern plan with an optional row seed for correlated subqueries.
pub(crate) fn execute_pattern_with_seed(
    pattern: &PatternPlan,
    seed: Option<&Binding>,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let schema = schema_for_pattern(pattern);
    execute_pattern_with_seed_and_schema(pattern, seed, schema, ctx)
}

pub(crate) fn execute_pattern_with_seed_and_schema(
    pattern: &PatternPlan,
    seed: Option<&Binding>,
    schema: BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<BindingTable, ExecutorError> {
    execute_pattern_with_seed_schema_and_limit(pattern, seed, schema, ctx, None)
}

pub(crate) fn execute_pattern_with_seed_schema_and_limit(
    pattern: &PatternPlan,
    seed: Option<&Binding>,
    schema: BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
    row_limit: Option<usize>,
) -> Result<BindingTable, ExecutorError> {
    super::batch::query::execute_pattern(
        pattern,
        schema,
        seed.cloned(),
        *ctx,
        super::batch::policy::BatchPolicy::default_policy(),
        row_limit,
    )
}

pub(crate) fn schema_for_pattern(pattern: &PatternPlan) -> BindingTableSchema {
    let mut columns = pattern
        .bindings
        .iter()
        .filter(|binding| {
            matches!(
                binding.element,
                BindingElement::Node | BindingElement::Edge | BindingElement::Path
            )
        })
        .map(|binding| BindingTableColumn {
            name: Some(binding.name.clone()),
            hidden: None,
            ty: binding.ty.clone(),
        })
        .collect::<Vec<_>>();
    let mut hidden = BTreeMap::new();
    collect_hidden_slots(&pattern.join_tree, &mut hidden);
    columns.extend(hidden.into_iter().map(|(hidden, ty)| BindingTableColumn {
        name: None,
        hidden: Some(hidden),
        ty,
    }));
    BindingTableSchema { columns }
}

fn collect_hidden_slots(tree: &JoinTree, slots: &mut BTreeMap<HiddenBindingId, AnalyzedType>) {
    match tree {
        JoinTree::Unit => {}
        JoinTree::Scan(scan) => {
            insert_hidden(slots, scan.hidden_binding, scan.kind);
        }
        JoinTree::Expand { child, edge, .. } => {
            collect_hidden_slots(child, slots);
            insert_hidden(slots, edge.left_hidden_binding, ScanKind::Node);
            insert_hidden(slots, edge.hidden_binding, ScanKind::Edge);
            insert_hidden(slots, edge.right_hidden_binding, ScanKind::Node);
        }
        JoinTree::Paths(_) => {}
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            collect_hidden_slots(left, slots);
            collect_hidden_slots(right, slots);
        }
        JoinTree::WorstCaseOptimal { intersection, .. } => {
            for tree in intersection {
                collect_hidden_slots(tree, slots);
            }
        }
        JoinTree::Subplan(_) => {}
        JoinTree::DisjunctiveScan { branches, .. } => {
            for branch in branches {
                insert_hidden(slots, branch.hidden_binding, branch.kind);
            }
        }
    }
}

fn insert_hidden(
    slots: &mut BTreeMap<HiddenBindingId, AnalyzedType>,
    hidden: Option<HiddenBindingId>,
    kind: ScanKind,
) {
    let Some(hidden) = hidden else {
        return;
    };
    slots.entry(hidden).or_insert_with(|| {
        AnalyzedType::Resolved(match kind {
            ScanKind::Node => GqlType::NodeRef,
            ScanKind::Edge => GqlType::EdgeRef,
        })
    });
}

pub(crate) fn binding_index(
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    binding_id: BindingId,
) -> Option<usize> {
    let binding = pattern
        .bindings
        .iter()
        .find(|candidate| candidate.binding == binding_id)?;
    column_index(schema, &binding.name)
}

pub(crate) fn column_index(schema: &BindingTableSchema, name: &DbString) -> Option<usize> {
    schema
        .columns
        .iter()
        .position(|column| column.name.as_ref() == Some(name))
}

pub(crate) fn hidden_index(schema: &BindingTableSchema, hidden: HiddenBindingId) -> Option<usize> {
    schema
        .columns
        .iter()
        .position(|column| column.hidden == Some(hidden))
}

#[derive(Clone, Copy)]
pub(crate) struct ColumnSlot {
    index: Option<usize>,
}

impl ColumnSlot {
    pub(crate) fn binding(
        pattern: &PatternPlan,
        schema: &BindingTableSchema,
        binding_id: Option<BindingId>,
        detail: &'static str,
    ) -> Result<Self, ExecutorError> {
        let Some(binding_id) = binding_id else {
            return Ok(Self { index: None });
        };
        let Some(index) = binding_index(pattern, schema, binding_id) else {
            return Err(ExecutorError::ImplementationDefined { detail });
        };
        Ok(Self { index: Some(index) })
    }

    pub(crate) fn hidden(
        schema: &BindingTableSchema,
        hidden: Option<HiddenBindingId>,
        detail: &'static str,
    ) -> Result<Self, ExecutorError> {
        let Some(hidden) = hidden else {
            return Ok(Self { index: None });
        };
        let Some(index) = hidden_index(schema, hidden) else {
            return Err(ExecutorError::ImplementationDefined { detail });
        };
        Ok(Self { index: Some(index) })
    }

    pub(crate) fn set(self, values: &mut [Value], value: Value) -> bool {
        let Some(index) = self.index else {
            return true;
        };
        if !matches!(values[index], Value::Null)
            && !value_compare::equal_non_null(&values[index], &value)
        {
            return false;
        }
        values[index] = value;
        true
    }

    pub(crate) fn index(self) -> Option<usize> {
        self.index
    }
}

pub(crate) fn source_index(
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    binding: Option<BindingId>,
    hidden: Option<HiddenBindingId>,
    detail: &'static str,
) -> Result<usize, ExecutorError> {
    if let Some(binding) = binding {
        return binding_index(pattern, schema, binding)
            .ok_or(ExecutorError::ImplementationDefined { detail });
    }
    if let Some(hidden) = hidden {
        return hidden_index(schema, hidden).ok_or(ExecutorError::ImplementationDefined { detail });
    }
    Err(ExecutorError::ImplementationDefined { detail })
}

pub(crate) fn node_at_index(
    row: &Binding,
    index: usize,
    wrong_type_detail: &'static str,
) -> Result<Option<NodeId>, ExecutorError> {
    match row.get(index).cloned().unwrap_or(Value::Null) {
        Value::NodeRef(id) => Ok(Some(id)),
        Value::Null => Ok(None),
        _ => Err(ExecutorError::ImplementationDefined {
            detail: wrong_type_detail,
        }),
    }
}

pub(crate) fn merge_rows(left: &Binding, right: &Binding, schema: &BindingTableSchema) -> Binding {
    let mut values = SmallVec::<[Value; 8]>::with_capacity(schema.columns.len());
    for index in 0..schema.columns.len() {
        let left_value = left.get(index).cloned().unwrap_or(Value::Null);
        // Null is the row-local unbound sentinel; prefer the bound side.
        if matches!(left_value, Value::Null) {
            values.push(right.get(index).cloned().unwrap_or(Value::Null));
        } else {
            values.push(left_value);
        }
    }
    Binding::from_parts(values, SmallVec::new())
}

pub(crate) fn resolve_key(
    schema: &BindingTableSchema,
    key: &[DbString],
) -> Result<Vec<usize>, ExecutorError> {
    let mut indexes = Vec::with_capacity(key.len());
    for name in key {
        let Some(index) = column_index(schema, name) else {
            return Err(ExecutorError::ImplementationDefined {
                detail: "join key column missing from pattern schema",
            });
        };
        indexes.push(index);
    }
    Ok(indexes)
}

pub(crate) fn key_values_at(row: &Binding, indexes: &[usize]) -> Option<Vec<Value>> {
    let mut values = Vec::with_capacity(indexes.len());
    for index in indexes {
        let value = row.get(*index).cloned().unwrap_or(Value::Null);
        if matches!(value, Value::Null) {
            return None;
        }
        values.push(value);
    }
    Some(values)
}

pub(crate) fn key_values_equal(lhs: &[Value], rhs: &[Value]) -> Result<bool, ExecutorError> {
    for (lhs, rhs) in lhs.iter().zip(rhs) {
        super::comparison_domain::ensure_pair(
            lhs,
            rhs,
            selene_core::ComparisonMode::PredicateEquality,
            crate::SourceSpan::default(),
        )?;
    }
    Ok(lhs.len() == rhs.len()
        && lhs.iter().zip(rhs).all(|(lhs, rhs)| {
            !matches!(lhs, Value::Null)
                && !matches!(rhs, Value::Null)
                && value_compare::gql_equal_non_null(lhs, rhs) == Some(true)
        }))
}

pub(crate) fn rows_match_on_resolved_key(
    left: &Binding,
    right: &Binding,
    indexes: &[usize],
) -> Result<bool, ExecutorError> {
    let Some(left_key) = key_values_at(left, indexes) else {
        return Ok(false);
    };
    let Some(right_key) = key_values_at(right, indexes) else {
        return Ok(false);
    };
    key_values_equal(&left_key, &right_key)
}

pub(crate) fn resolve_projection(
    source_schema: &BindingTableSchema,
    target_schema: &BindingTableSchema,
) -> Vec<Option<usize>> {
    target_schema
        .columns
        .iter()
        .map(|target_column| {
            target_column
                .name
                .as_ref()
                .and_then(|name| column_index(source_schema, name))
        })
        .collect()
}

pub(crate) fn project_row_with_projection(
    row: &Binding,
    target_schema: &BindingTableSchema,
    projection: &[Option<usize>],
    seed: Option<&Binding>,
) -> Binding {
    let mut values = seed
        .map(|row| row.values().to_vec())
        .unwrap_or_else(|| vec![Value::Null; target_schema.columns.len()]);
    values.resize(target_schema.columns.len(), Value::Null);
    for (target_index, source_index) in projection.iter().enumerate() {
        let Some(source_index) = source_index else {
            continue;
        };
        values[target_index] = row.get(*source_index).cloned().unwrap_or(Value::Null);
    }
    Binding::new(values)
}

pub(crate) fn filter_predicates_pass(
    predicates: &[FilterPredicate],
    pattern: &PatternPlan,
    row: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    for predicate in predicates {
        if !filter_predicate_passes(predicate, pattern, row, schema, ctx)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn filter_predicate_passes(
    predicate: &FilterPredicate,
    pattern: &PatternPlan,
    row: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    if predicate.index_consumed {
        return Ok(true);
    }
    match predicate.kind {
        crate::FilterPredicateKind::Expression => {
            let value = evaluator::evaluate(&predicate.expr, row, schema, ctx)?;
            Ok(matches!(value, Value::Bool(true)))
        }
        crate::FilterPredicateKind::PropertyEquals { .. } => {
            scan::predicate_passes(predicate, pattern, row, schema, &Value::Null, ctx)
        }
    }
}
