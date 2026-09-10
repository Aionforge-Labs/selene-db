use crate::runtime::comparison_domain::ComparisonDomain;
use selene_core::ComparisonMode;
use std::{cmp::Ordering, cmp::Reverse, collections::BinaryHeap, sync::Arc};

use selene_core::Value;

use crate::{
    LimitAmount, OrderKey,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, TxContext},
};

use super::{limit, order_by};

pub(super) fn execute(
    keys: &[OrderKey],
    offset: &LimitAmount,
    count: &LimitAmount,
    table: BindingTable,
    ctx: &TxContext<'_, '_>,
    eval_ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let offset = limit::resolve_amount(offset, ctx)?;
    let count = limit::resolve_amount(count, ctx)?;
    let (schema, rows) = table.into_parts();
    let retained = limit::u64_to_bounded_usize(offset.saturating_add(count), rows.len());
    if retained == 0 {
        return Ok(BindingTable::new(schema, Vec::new()));
    }

    let keys = Arc::<[OrderKey]>::from(keys.to_vec());
    let mut heap = BinaryHeap::<Reverse<RankedRow>>::with_capacity(retained.saturating_add(1));
    let mut domains = ComparisonDomain::default();
    let mut rows_since_check = 0;
    for (sequence, row) in rows.into_iter().enumerate() {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let tuple = order_by::evaluate_key_tuple(&keys, &row, &schema, eval_ctx)?;
        domains.observe(&tuple, ComparisonMode::Ordering)?;
        heap.push(Reverse(RankedRow {
            tuple,
            sequence,
            row,
            keys: Arc::clone(&keys),
        }));
        if heap.len() > retained {
            heap.pop();
        }
    }

    let mut retained_rows = heap.into_iter().map(|Reverse(row)| row).collect::<Vec<_>>();
    retained_rows.sort_by(|lhs, rhs| lhs.desired_cmp(rhs));

    let start = limit::u64_to_bounded_usize(offset, retained_rows.len());
    Ok(BindingTable::new(
        schema,
        retained_rows
            .into_iter()
            .skip(start)
            .map(|row| row.row)
            .collect(),
    ))
}

struct RankedRow {
    tuple: Vec<Value>,
    sequence: usize,
    row: Binding,
    keys: Arc<[OrderKey]>,
}

impl RankedRow {
    fn desired_cmp(&self, rhs: &Self) -> Ordering {
        order_by::compare_key_tuples(&self.tuple, &rhs.tuple, &self.keys)
            .then_with(|| self.sequence.cmp(&rhs.sequence))
    }
}

impl Eq for RankedRow {}

impl PartialEq for RankedRow {
    fn eq(&self, rhs: &Self) -> bool {
        self.sequence == rhs.sequence
    }
}

impl Ord for RankedRow {
    fn cmp(&self, rhs: &Self) -> Ordering {
        self.desired_cmp(rhs).reverse()
    }
}

impl PartialOrd for RankedRow {
    fn partial_cmp(&self, rhs: &Self) -> Option<Ordering> {
        Some(self.cmp(rhs))
    }
}
