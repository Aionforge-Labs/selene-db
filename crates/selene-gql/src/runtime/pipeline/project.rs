use crate::{
    BindingTableColumn, BindingTableSchema, ProjectExpr,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, evaluator},
};
use selene_core::Value;
use smallvec::SmallVec;

pub(super) fn execute(
    items: &[ProjectExpr],
    table: BindingTable,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let (input_schema, input_rows) = table.into_parts();
    let output_schema = schema_for_items(items);
    let mut rows = Vec::with_capacity(input_rows.len());
    let mut rows_since_check = 0;
    for row in input_rows {
        ctx.tx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let mut values = SmallVec::<[Value; 8]>::with_capacity(items.len());
        for item in items {
            values.push(project_value(item, &row, &input_schema, ctx)?);
        }
        rows.push(Binding::from_parts(values, SmallVec::new()));
    }
    Ok(BindingTable::new(output_schema, rows))
}

fn project_value(
    item: &ProjectExpr,
    row: &Binding,
    schema: &BindingTableSchema,
    ctx: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Value, ExecutorError> {
    evaluator::evaluate(&item.expr, row, schema, ctx)
}

/// Derive the output schema for a projection item list.
///
/// Shared with the batch project operator so both paths declare identical
/// column names, order, and types for the same plan.
pub(crate) fn schema_for_items(items: &[ProjectExpr]) -> BindingTableSchema {
    BindingTableSchema {
        columns: items
            .iter()
            .map(|item| BindingTableColumn {
                name: item.alias.clone(),
                hidden: None,
                ty: item.ty.clone(),
            })
            .collect(),
    }
}
