use crate::runtime::comparison_domain::ComparisonDomain;
use rustc_hash::FxHashMap;
use selene_core::ComparisonMode;
use selene_core::Value;
use smallvec::SmallVec;

use crate::{
    Aggregate, BindingTableColumn, ProjectExpr, SourceSpan,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, evaluator, value_key::RuntimeEqKey},
};

use super::aggregate::{self, AggregateSlot};

pub(super) fn execute(
    keys: &[ProjectExpr],
    aggregates: &[Aggregate],
    table: BindingTable,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let (input_schema, input_rows) = table.into_parts();
    let output_schema = output_schema(&input_schema, aggregates);
    // Insertion-ordered groups (first-emission order preserved) + an
    // `FxHashMap<RuntimeEqKey, usize>` index so locating a row's group is O(1)
    // rather than the previous O(n_groups) linear scan per row. `RuntimeEqKey`
    // equality matches the prior `key_values_equal` exactly (Null/Null⇒equal,
    // Null/non-null⇒distinct, else `equal_non_null`), so grouping semantics are
    // unchanged; DISTINCT/set-ops already use the same key.
    let group_cap = ctx.tx.impl_defined_caps().group_by_key_cap();
    let hash_capacity = input_rows.len().min(group_cap);
    let mut groups = Vec::<Group<'_>>::with_capacity(hash_capacity);
    let mut group_index = FxHashMap::<RuntimeEqKey, usize>::default();
    group_index.reserve(hash_capacity);
    let mut domains = ComparisonDomain::default();
    let mut rows_since_check = 0;

    for row in &input_rows {
        ctx.tx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let key = evaluate_key_tuple(keys, row, &input_schema, ctx)?;
        domains.observe(&key, ComparisonMode::Distinctness)?;
        let probe = RuntimeEqKey::from_row(key);
        let index = match group_index.get(&probe) {
            Some(index) => *index,
            None => {
                if groups.len() >= group_cap {
                    return Err(group_by_key_cap_exceeded());
                }
                let index = groups.len();
                groups.push(Group::new(row.clone(), aggregates)?);
                group_index.insert(probe, index);
                index
            }
        };
        groups[index].observe(row, &input_schema, ctx)?;
    }

    if keys.is_empty() && groups.is_empty() {
        let representative = Binding::new(
            input_schema
                .columns
                .iter()
                .map(|_| Value::Null)
                .collect::<Vec<_>>(),
        );
        groups.push(Group::new(representative, aggregates)?);
    }

    let rows = groups
        .into_iter()
        .map(|group| {
            ctx.tx.check_cancellation_stride(&mut rows_since_check, 1)?;
            group.finalize()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BindingTable::new(output_schema, rows))
}

fn group_by_key_cap_exceeded() -> ExecutorError {
    ExecutorError::ProgramLimitExceeded {
        detail: "GROUP BY distinct-group cap exceeded",
        span: SourceSpan::default(),
    }
}

struct Group<'plan> {
    representative: Binding,
    aggregates: Vec<AggregateSlot<'plan>>,
}

impl<'plan> Group<'plan> {
    fn new(representative: Binding, aggregates: &'plan [Aggregate]) -> Result<Self, ExecutorError> {
        let aggregates = aggregates
            .iter()
            .map(AggregateSlot::new)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            representative,
            aggregates,
        })
    }

    fn observe(
        &mut self,
        row: &Binding,
        schema: &crate::BindingTableSchema,
        ctx: &EvalCtx<'_, '_, '_, '_>,
    ) -> Result<(), ExecutorError> {
        for aggregate in &mut self.aggregates {
            aggregate.observe(row, schema, ctx)?;
        }
        Ok(())
    }

    fn finalize(self) -> Result<Binding, ExecutorError> {
        let mut values = self.representative.cloned_values();
        for aggregate in self.aggregates {
            values.push(aggregate.finalize_value()?);
        }
        Ok(Binding::from_parts(values, SmallVec::new()))
    }
}

fn output_schema(
    input_schema: &crate::BindingTableSchema,
    aggregates: &[Aggregate],
) -> crate::BindingTableSchema {
    let mut schema = input_schema.clone();
    schema
        .columns
        .extend(aggregates.iter().flat_map(|aggregate| {
            aggregate::output_names(aggregate)
                .into_iter()
                .map(|name| BindingTableColumn {
                    name: Some(name),
                    hidden: None,
                    ty: aggregate.ty.clone(),
                })
                .collect::<Vec<_>>()
        }));
    schema
}

fn evaluate_key_tuple(
    keys: &[ProjectExpr],
    row: &Binding,
    schema: &crate::BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Value>, ExecutorError> {
    keys.iter()
        .map(|key| evaluator::evaluate(&key.expr, row, schema, ctx))
        .collect()
}
