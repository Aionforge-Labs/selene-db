//! Non-leading MATCH pipeline operator.

use selene_core::Value;

use crate::{
    BindingTableColumn, BindingTableSchema, PatternPlan, SubqueryRegistry,
    analyze::ExprIdLookup,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, TxContext, pattern},
};

pub(super) fn execute(
    pattern_plan: &PatternPlan,
    table: BindingTable,
    ctx: &TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    let (input_schema, input_rows) = table.into_parts();
    let target_schema = target_schema(&input_schema, pattern_plan);
    let eval_ctx = EvalCtx {
        tx: ctx,
        expr_ids,
        subqueries,
    };
    let mut output = Vec::new();
    let mut rows_since_check = 0;

    for row in input_rows {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let seed = seed_row(&row, &input_schema, &target_schema);
        let matched = pattern::execute_pattern_with_seed_and_schema(
            pattern_plan,
            Some(&seed),
            target_schema.clone(),
            &eval_ctx,
        )?;
        for matched_row in matched.rows() {
            let values = matched_row.values().to_vec();
            output.push(Binding::with_insert_sites(
                values,
                row.insert_sites().iter().copied().collect(),
            ));
        }
    }

    Ok(BindingTable::new(target_schema, output))
}

pub(super) fn execute_optional(
    pattern_plan: &PatternPlan,
    table: BindingTable,
    ctx: &TxContext<'_, '_>,
    expr_ids: &ExprIdLookup,
    subqueries: &SubqueryRegistry,
) -> Result<BindingTable, ExecutorError> {
    let (input_schema, input_rows) = table.into_parts();
    let target_schema = target_schema(&input_schema, pattern_plan);
    let eval_ctx = EvalCtx {
        tx: ctx,
        expr_ids,
        subqueries,
    };
    let mut output = Vec::new();
    let mut rows_since_check = 0;

    for row in input_rows {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let seed = seed_row(&row, &input_schema, &target_schema);
        let matched = pattern::execute_pattern_with_seed_and_schema(
            pattern_plan,
            Some(&seed),
            target_schema.clone(),
            &eval_ctx,
        )?;
        if matched.is_empty() {
            output.push(seed);
            continue;
        }
        for matched_row in matched.rows() {
            let values = matched_row.values().to_vec();
            output.push(Binding::with_insert_sites(
                values,
                row.insert_sites().iter().copied().collect(),
            ));
        }
    }

    Ok(BindingTable::new(target_schema, output))
}

/// Target schema for a non-leading match: input columns plus pattern columns.
///
/// Shared with the batch match operator so both paths extend schemas
/// identically.
pub(crate) fn target_schema(
    input: &BindingTableSchema,
    pattern_plan: &PatternPlan,
) -> BindingTableSchema {
    let mut schema = input.clone();
    for column in pattern::schema_for_pattern(pattern_plan).columns {
        if column_exists(&schema, &column) {
            continue;
        }
        schema.columns.push(column);
    }
    schema
}

fn column_exists(schema: &BindingTableSchema, column: &BindingTableColumn) -> bool {
    match (column.name.clone(), column.hidden) {
        (Some(name), _) => schema
            .columns
            .iter()
            .any(|candidate| candidate.name == Some(name.clone())),
        (None, Some(hidden)) => schema
            .columns
            .iter()
            .any(|candidate| candidate.hidden == Some(hidden)),
        (None, None) => false,
    }
}

/// Map one input row into target-schema coordinates for seeded evaluation.
///
/// Shared with the batch match operator so both paths seed identically.
pub(crate) fn seed_row(
    row: &Binding,
    input_schema: &BindingTableSchema,
    target_schema: &BindingTableSchema,
) -> Binding {
    let mut values = vec![Value::Null; target_schema.columns.len()];
    for (source_index, source_column) in input_schema.columns.iter().enumerate() {
        let Some(target_index) = target_schema.columns.iter().position(|target| {
            target.name == source_column.name && target.hidden == source_column.hidden
        }) else {
            continue;
        };
        values[target_index] = row.get(source_index).cloned().unwrap_or(Value::Null);
    }
    Binding::with_insert_sites(values, row.insert_sites().iter().copied().collect())
}
