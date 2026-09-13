//! Correlated table-call kernel for the physical batch extension.

use selene_core::Value;
use smallvec::SmallVec;

use crate::{
    BindingTableColumn, BindingTableSchema, ExecutionPlan, PatternPlan, PipelineOp,
    PlannedTableSubquery,
    runtime::{Binding, BindingTable, ExecutorError, TxContext, pattern},
};

pub(super) fn extend(
    call: &PlannedTableSubquery,
    row: &Binding,
    input_schema: &BindingTableSchema,
    ctx: &TxContext<'_, '_>,
    policy: super::policy::BatchPolicy,
    mut emit: impl FnMut(Binding) -> Result<(), ExecutorError>,
) -> Result<(), ExecutorError> {
    if null_outer_binding_is_plan_pattern_binding(call, row, input_schema)? {
        if call.optional {
            emit(optional_output_row(call, row))?;
        }
        return Ok(());
    }
    let target_schema = target_schema(call, input_schema)?;
    let seed = seed_binding(call, row, input_schema, &target_schema)?;
    let inner = super::query::execute_seeded_subplan(
        &call.body,
        BindingTable::new(target_schema, vec![seed]),
        ctx,
        policy,
    )?;
    let indices = yield_indices(call, inner.schema())?;
    if call.optional && inner.is_empty() {
        emit(optional_output_row(call, row))?;
    } else {
        for inner_row in inner.rows() {
            ctx.check_cancellation()?;
            emit(
                row.with_appended_values(
                    indices
                        .iter()
                        .map(|index| inner_row.get(*index).cloned().unwrap_or(Value::Null)),
                ),
            )?;
        }
    }
    Ok(())
}

fn optional_output_row(call: &PlannedTableSubquery, input: &Binding) -> Binding {
    input.with_appended_values(std::iter::repeat_n(Value::Null, call.yield_schema.len()))
}

fn target_schema(
    call: &PlannedTableSubquery,
    source_schema: &BindingTableSchema,
) -> Result<BindingTableSchema, ExecutorError> {
    let mut schema = call
        .body
        .pattern_plan
        .as_ref()
        .map(pattern::schema_for_pattern)
        .unwrap_or_else(|| BindingTableSchema {
            columns: Vec::new(),
        });
    for outer in &call.outer_binding_refs {
        if pattern::column_index(&schema, &outer.name).is_some() {
            continue;
        }
        let source_index = source_index(source_schema, outer.name.clone())?;
        let source_column = &source_schema.columns[source_index];
        schema.columns.push(BindingTableColumn {
            name: Some(outer.name.clone()),
            hidden: None,
            ty: source_column.ty.clone(),
        });
    }
    Ok(schema)
}

fn seed_binding(
    call: &PlannedTableSubquery,
    row: &Binding,
    source_schema: &BindingTableSchema,
    target_schema: &BindingTableSchema,
) -> Result<Binding, ExecutorError> {
    let mut values = SmallVec::<[Value; 8]>::new();
    values.resize(target_schema.columns.len(), Value::Null);
    for outer in &call.outer_binding_refs {
        let source_index = source_index(source_schema, outer.name.clone())?;
        let value = row.get(source_index).cloned().unwrap_or(Value::Null);
        let target_index = pattern::column_index(target_schema, &outer.name).ok_or(
            ExecutorError::ImplementationDefined {
                detail: "CALL subquery outer binding missing from target row",
            },
        )?;
        values[target_index] = value;
    }
    Ok(Binding::from_parts(values, SmallVec::new()))
}

fn null_outer_binding_is_plan_pattern_binding(
    call: &PlannedTableSubquery,
    row: &Binding,
    source_schema: &BindingTableSchema,
) -> Result<bool, ExecutorError> {
    for outer in &call.outer_binding_refs {
        if !plan_binds_name(&call.body, outer.name.clone()) {
            continue;
        }
        let source_index = source_index(source_schema, outer.name.clone())?;
        if matches!(row.get(source_index), Some(Value::Null) | None) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn plan_binds_name(plan: &ExecutionPlan, name: selene_core::DbString) -> bool {
    plan.pattern_plan
        .as_ref()
        .is_some_and(|pattern| pattern_binds_name(pattern, name.clone()))
        || plan
            .pipeline
            .iter()
            .any(|op| op_binds_name(op, name.clone()))
}

fn op_binds_name(op: &PipelineOp, name: selene_core::DbString) -> bool {
    match op {
        PipelineOp::Match(pattern) | PipelineOp::OptionalMatch(pattern) => {
            pattern_binds_name(pattern, name)
        }
        PipelineOp::Union { rhs, .. }
        | PipelineOp::Chain(rhs)
        | PipelineOp::CorrelatedChain(rhs)
        | PipelineOp::ExplainPlan { inner: rhs, .. } => plan_binds_name(rhs, name),
        PipelineOp::CallSubquery(subquery) => plan_binds_name(&subquery.body, name),
        _ => false,
    }
}

fn pattern_binds_name(pattern: &PatternPlan, name: selene_core::DbString) -> bool {
    pattern.bindings.iter().any(|binding| binding.name == name)
}

fn yield_indices(
    call: &PlannedTableSubquery,
    inner_schema: &BindingTableSchema,
) -> Result<Vec<usize>, ExecutorError> {
    call.yield_items
        .iter()
        .map(|item| source_index(inner_schema, item.source.clone()))
        .collect()
}

fn source_index(
    schema: &BindingTableSchema,
    name: selene_core::DbString,
) -> Result<usize, ExecutorError> {
    pattern::column_index(schema, &name).ok_or(ExecutorError::ImplementationDefined {
        detail: "CALL subquery binding missing from source row",
    })
}
