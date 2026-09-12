use crate::runtime::comparison_domain::ComparisonDomain;
use selene_core::ComparisonMode;
use std::cmp::Ordering;

use selene_core::Value;

use crate::{
    NullsPolicy, OrderDirection, OrderKey,
    runtime::{
        Binding, BindingTable, EvalCtx, ExecutorError, evaluator,
        value_compare::{self, NullSortOrder},
    },
};

pub(super) fn execute(
    keys: &[OrderKey],
    table: BindingTable,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let (schema, rows) = table.into_parts();
    let mut keyed_rows = Vec::with_capacity(rows.len());
    let mut domains = ComparisonDomain::default();
    let mut rows_since_check = 0;
    for row in rows {
        ctx.tx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let tuple = evaluate_key_tuple(keys, &row, &schema, ctx)?;
        domains.observe(&tuple, ComparisonMode::Ordering)?;
        keyed_rows.push(KeyedRow { tuple, row });
    }

    // Phase A deliberately ignores OrderKey.access. That planner hint is
    // reserved for the Phase B scan-order shortcut; executor sorting remains
    // explicit and stable here.
    ctx.tx.check_cancellation()?;
    keyed_rows.sort_by(|lhs, rhs| compare_key_tuples(&lhs.tuple, &rhs.tuple, keys));

    Ok(BindingTable::new(
        schema,
        keyed_rows.into_iter().map(|row| row.row).collect(),
    ))
}

struct KeyedRow {
    tuple: Vec<Value>,
    row: Binding,
}

/// Drop the carrier columns the planner appended so `ORDER BY` could reach a
/// binding the `RETURN` discards.
///
/// Carriers are always appended after the projected columns, so this is a
/// truncation. A width already at or below `projected_width` means the planner
/// added no carriers for this plan, and truncating is then a no-op rather than
/// an error — the op is emitted from one place and its own tests pin the pairing.
pub(super) fn trim_carriers(projected_width: usize, table: BindingTable) -> BindingTable {
    let (mut schema, rows) = table.into_parts();
    if schema.columns.len() <= projected_width {
        return BindingTable::new(schema, rows);
    }
    schema.columns.truncate(projected_width);
    let rows = rows
        .into_iter()
        .map(|row| {
            let (mut values, insert_sites) = row.into_parts();
            values.truncate(projected_width);
            Binding::from_parts(values, insert_sites)
        })
        .collect();
    BindingTable::new(schema, rows)
}

/// Evaluate one row's sort-key tuple in key order.
///
/// Shared with the batch sort operator so both engines sort the same key
/// values with the same evaluation errors.
pub(crate) fn evaluate_key_tuple(
    keys: &[OrderKey],
    row: &Binding,
    schema: &crate::BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Value>, ExecutorError> {
    keys.iter()
        .map(|key| evaluator::evaluate(&key.expr, row, schema, ctx))
        .collect()
}

/// Compare two sort-key tuples key by key with per-key direction and null
/// ordering.
///
/// Shared with the batch sort operator so ties, null placement, and the
/// selected (binary) string collation agree exactly. The comparison is
/// stable by construction: equal tuples compare `Equal`, and both engines
/// use stable sorts, so ties keep input order and no implicit total order
/// is invented.
pub(crate) fn compare_key_tuples(lhs: &[Value], rhs: &[Value], keys: &[OrderKey]) -> Ordering {
    lhs.iter()
        .zip(rhs.iter())
        .zip(keys.iter())
        .map(|((lhs, rhs), key)| compare_key_value(lhs, rhs, key))
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

fn compare_key_value(lhs: &Value, rhs: &Value, key: &OrderKey) -> Ordering {
    let nulls = null_sort_order(key);
    let ordering = value_compare::compare_for_sort(lhs, rhs, nulls);
    match key.direction {
        OrderDirection::Asc => ordering,
        OrderDirection::Desc => ordering.reverse(),
    }
}

fn null_sort_order(key: &OrderKey) -> NullSortOrder {
    let desired = match key.nulls {
        Some(NullsPolicy::NullsFirst) => NullSortOrder::First,
        Some(NullsPolicy::NullsLast) => NullSortOrder::Last,
        None => match key.direction {
            OrderDirection::Asc => NullSortOrder::Last,
            OrderDirection::Desc => NullSortOrder::First,
        },
    };
    match (key.direction, desired) {
        (OrderDirection::Asc, order) => order,
        (OrderDirection::Desc, NullSortOrder::First) => NullSortOrder::Last,
        (OrderDirection::Desc, NullSortOrder::Last) => NullSortOrder::First,
    }
}
