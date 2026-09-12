//! Set-composition pipeline operator.
//!
//! # `LIMIT` precedence under `UNION ALL`
//!
//! `LIMIT N` is a pipeline statement that **statically attaches to the
//! `query_pipeline` arm it sits within** — *not* a planner-level fanout
//! over the union. Per `grammar.pest:99,125-126`, a `composite_query`
//! alternates `set_op` between `query_pipeline` productions; a trailing
//! `LIMIT N` is absorbed into the last arm's `pipeline_statement+` rather
//! than the surrounding union. To limit the union total, wrap the
//! composite in a `CALL { ... }` table subquery and apply `LIMIT` to the
//! outer pipeline. See `docs/gql-reference.md` §4 "Set composition" for
//! the user-facing description.
//!
//! Two empirical regression fixtures in
//! `crates/selene-gql/tests/exec_pipeline_union.rs` pin this behaviour:
//!
//! - `union_of_populated_and_empty_returns_populated` —
//!   `RETURN 1 AS n UNION ALL RETURN 2 AS n LIMIT 0` returns `[1]`. The
//!   `LIMIT 0` clamps arm B only; arm A is unaffected.
//! - `union_rhs_pipeline_op_id_high_water_consistent_after_executor_runs`
//!   — `RETURN 1 AS n LIMIT 1 UNION ALL RETURN 2 AS n LIMIT 1` returns
//!   `[1, 2]`, confirming each arm carries its own LIMIT.
//! - `three_arm_union_with_middle_limit_is_per_arm_static_attachment`
//!   (added in BRIEF-155) — `A UNION ALL B LIMIT N UNION ALL C` limits
//!   only arm B; arms A and C run unlimited.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    BindingTableSchema, ExecutionPlan, SetOp, SourceSpan,
    runtime::{
        Binding, BindingTable, DataExceptionSubclass, ExecutorError, TxContext, execute_plan,
        plan_runner, value_key::RuntimeEqKey,
    },
};

use super::{distinct, row_key};

pub(super) fn execute(
    op: SetOp,
    rhs: &ExecutionPlan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    if matches!(op, SetOp::Otherwise) {
        return execute_otherwise(rhs, table, ctx);
    }
    let rhs_table = execute_plan(rhs, ctx)?;
    execute_with_rhs(op, table, rhs_table, ctx)
}

pub(super) fn execute_read_only(
    op: SetOp,
    rhs: &ExecutionPlan,
    table: BindingTable,
    ctx: &TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    if matches!(op, SetOp::Otherwise) {
        return execute_otherwise_read_only(rhs, table, ctx);
    }
    let rhs_table = plan_runner::execute_plan_read_only(rhs, ctx)?;
    execute_with_rhs(op, table, rhs_table, ctx)
}

pub(super) fn execute_with_rhs(
    op: SetOp,
    table: BindingTable,
    rhs_table: BindingTable,
    ctx: &TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    match op {
        SetOp::Union | SetOp::UnionAll => {
            assert_compatible_schemas("UNION", table.schema(), rhs_table.schema())?;
            let (schema, mut rows) = table.into_parts();
            ctx.check_cancellation()?;
            let (_, rhs_rows) = rhs_table.into_parts();
            rows.extend(rhs_rows);
            let combined = BindingTable::new(schema, rows);
            if matches!(op, SetOp::Union) {
                distinct::execute(combined, ctx)
            } else {
                Ok(combined)
            }
        }
        SetOp::Intersect | SetOp::IntersectAll | SetOp::Except | SetOp::ExceptAll => {
            assert_compatible_schemas(op_name(op), table.schema(), rhs_table.schema())?;
            execute_counted(op, table, &rhs_table, ctx)
        }
        SetOp::Otherwise => {
            unreachable!(
                "Otherwise prefiltered by execute/execute_read_only before execute_with_rhs"
            )
        }
    }
}

fn execute_otherwise(
    rhs: &ExecutionPlan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let (schema, rows) = table.into_parts();
    assert_compatible_schemas("OTHERWISE", &schema, &rhs.output_schema)?;
    if rows.is_empty() {
        let rhs_table = execute_plan(rhs, ctx)?;
        assert_compatible_schemas("OTHERWISE", &schema, rhs_table.schema())?;
        let (_, rhs_rows) = rhs_table.into_parts();
        Ok(BindingTable::new(schema, rhs_rows))
    } else {
        Ok(BindingTable::new(schema, rows))
    }
}

fn execute_otherwise_read_only(
    rhs: &ExecutionPlan,
    table: BindingTable,
    ctx: &TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let (schema, rows) = table.into_parts();
    assert_compatible_schemas("OTHERWISE", &schema, &rhs.output_schema)?;
    if rows.is_empty() {
        let rhs_table = plan_runner::execute_plan_read_only(rhs, ctx)?;
        assert_compatible_schemas("OTHERWISE", &schema, rhs_table.schema())?;
        let (_, rhs_rows) = rhs_table.into_parts();
        Ok(BindingTable::new(schema, rhs_rows))
    } else {
        Ok(BindingTable::new(schema, rows))
    }
}

fn execute_counted(
    op: SetOp,
    lhs: BindingTable,
    rhs: &BindingTable,
    ctx: &TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    let mut domains = crate::runtime::comparison_domain::ComparisonDomain::default();
    for row in lhs.rows().iter().chain(rhs.rows()) {
        ctx.check_cancellation()?;
        domains.observe(row.values(), selene_core::ComparisonMode::Distinctness)?;
    }
    let mut rhs_counts = count_rows(rhs.rows(), ctx)?;
    let (schema, lhs_rows) = lhs.into_parts();
    let mut output = Vec::new();
    let mut seen = FxHashSet::default();
    let mut rows_since_check = 0;

    for row in lhs_rows {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        match op {
            SetOp::IntersectAll => {
                let key = row_key(&row);
                if let Some(count) = rhs_counts.get_mut(&key)
                    && *count > 0
                {
                    *count -= 1;
                    output.push(row);
                }
            }
            SetOp::Intersect => {
                let key = row_key(&row);
                if rhs_counts.contains_key(&key) && insert_seen(&mut seen, key, ctx)? {
                    output.push(row);
                }
            }
            SetOp::ExceptAll => {
                let key = row_key(&row);
                if let Some(count) = rhs_counts.get_mut(&key)
                    && *count > 0
                {
                    *count -= 1;
                    continue;
                }
                output.push(row);
            }
            SetOp::Except => {
                let key = row_key(&row);
                if !rhs_counts.contains_key(&key) && insert_seen(&mut seen, key, ctx)? {
                    output.push(row);
                }
            }
            SetOp::Union | SetOp::UnionAll | SetOp::Otherwise => unreachable!("set op prefiltered"),
        }
    }

    Ok(BindingTable::new(schema, output))
}

fn count_rows(
    rows: &[Binding],
    ctx: &TxContext<'_, '_>,
) -> Result<FxHashMap<RuntimeEqKey, usize>, ExecutorError> {
    let mut counts = FxHashMap::default();
    let mut rows_since_check = 0;
    for row in rows {
        ctx.check_cancellation_stride(&mut rows_since_check, 1)?;
        let key = row_key(row);
        if !counts.contains_key(&key) && counts.len() >= ctx.impl_defined_caps().set_op_key_cap() {
            return Err(set_op_key_cap_exceeded());
        }
        *counts.entry(key).or_insert(0) += 1;
    }
    Ok(counts)
}

fn insert_seen(
    seen: &mut FxHashSet<RuntimeEqKey>,
    key: RuntimeEqKey,
    ctx: &TxContext<'_, '_>,
) -> Result<bool, ExecutorError> {
    if seen.contains(&key) {
        return Ok(false);
    }
    if seen.len() >= ctx.impl_defined_caps().set_op_key_cap() {
        return Err(set_op_key_cap_exceeded());
    }
    Ok(seen.insert(key))
}

/// Typed resource error for set-operation key-cap exhaustion.
///
/// Shared with the batch set operator so both paths report the identical
/// `5GQL1` diagnostic.
pub(crate) fn set_op_key_cap_exceeded() -> ExecutorError {
    ExecutorError::ProgramLimitExceeded {
        detail: "set-op key cap exceeded",
        span: SourceSpan::default(),
    }
}

/// Enforce positional column-count compatibility between set arms.
///
/// Shared with the batch set operator so both paths report the identical
/// data exception for mismatched arms.
pub(crate) fn assert_compatible_schemas(
    op_name: &'static str,
    lhs: &BindingTableSchema,
    rhs: &BindingTableSchema,
) -> Result<(), ExecutorError> {
    let lhs_len = lhs.columns.len();
    let rhs_len = rhs.columns.len();
    if lhs_len != rhs_len {
        return Err(ExecutorError::DataException {
            subclass: DataExceptionSubclass::InvalidValueType,
            message: format!(
                "{op_name} arms have differing column counts: lhs={lhs_len}, rhs={rhs_len}"
            ),
            span: SourceSpan::default(),
        });
    }
    Ok(())
}

fn op_name(op: SetOp) -> &'static str {
    match op {
        SetOp::Union | SetOp::UnionAll => "UNION",
        SetOp::Intersect | SetOp::IntersectAll => "INTERSECT",
        SetOp::Except | SetOp::ExceptAll => "EXCEPT",
        SetOp::Otherwise => "OTHERWISE",
    }
}
