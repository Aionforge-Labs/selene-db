//! Schema and seed mapping for correlated physical patterns.
use crate::{BindingTableSchema, PatternPlan, runtime::Binding};
use selene_core::Value;

pub(crate) fn target_schema(
    input: &BindingTableSchema,
    pattern: &PatternPlan,
) -> BindingTableSchema {
    crate::runtime::plan_runner::target_schema(input, pattern)
}

pub(crate) fn seed_row(
    row: &Binding,
    input: &BindingTableSchema,
    target: &BindingTableSchema,
) -> Binding {
    let mut values = vec![Value::Null; target.columns.len()];
    for (source_index, source) in input.columns.iter().enumerate() {
        if let Some(target_index) = target
            .columns
            .iter()
            .position(|column| column.name == source.name && column.hidden == source.hidden)
        {
            values[target_index] = row.get(source_index).cloned().unwrap_or(Value::Null);
        }
    }
    Binding::with_insert_sites(values, row.insert_sites().iter().copied().collect())
}
