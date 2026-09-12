//! Batch set-composition (`UNION`/`INTERSECT`/`EXCEPT` and `OTHERWISE`).
//!
//! [`BatchSet`] carries one [`PipelineOp::Union`](crate::PipelineOp::Union)
//! into batches. The left input materializes through the pull protocol while
//! the right arm executes through the read-only plan runner (which routes
//! batch-coverable arms through this same driver and every other arm through
//! the row path, so arms containing aggregation, sorting, or other
//! not-yet-batched families keep their exact row behavior). The
//! set-composition combinatorics themselves always run here natively: the
//! row union adapter is out of production use for batch-accepted shapes.
//!
//! Combination reuses the row path's semantic service by construction:
//! [`assert_compatible_schemas`](super::super::pipeline::union::assert_compatible_schemas)
//! keeps the positional column-count contract, [`ComparisonDomain`] validates
//! the distinctness relation over the same row order, and
//! [`RuntimeEqKey`] keys agree with the language equality relation
//! (cross-type numerics collapse; records, lists, and references use the
//! runtime-equality regime — never serialized values or debug strings).
//! Deduplicating (`UNION`/`INTERSECT`/`EXCEPT`) versus multiset
//! (`UNION ALL`/`INTERSECT ALL`/`EXCEPT ALL`) variants keep deliberately
//! different duplicate counts with left-arm order preserved, and the
//! production `set_op_key_cap` bounds counted keys with the identical
//! `5GQL1` diagnostic. `OTHERWISE` runs its right arm only when the left
//! input is empty.
//!
//! Memory is accounted before allocation: output rows reserve their estimate
//! before they are pushed and counted-key insertions reserve before the
//! entry allocates, so a bounded budget fails with the typed resource error
//! instead of truncating. The operator holds its output reservation until
//! `close`.

use rustc_hash::{FxHashMap, FxHashSet};
use selene_core::Value;

use crate::{
    ExecutionPlan, SetOp,
    plan::{BindingTableSchema, ImplDefinedCaps},
    runtime::{Binding, EvalCtx, ExecutorError},
};

use super::super::{
    comparison_domain::ComparisonDomain,
    pipeline::{assert_compatible_schemas, set_op_key_cap_exceeded},
    plan_runner,
    value_key::RuntimeEqKey,
};
use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    join::reserve_rows,
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};

/// Combine two materialized arms under one set operator.
///
/// `lhs` is the left input in output order; `rhs` is the right arm in output
/// order. Returns the combined rows plus the output reservation the caller
/// holds until the rows drop; transient key storage is released before
/// returning. `OTHERWISE` never reaches here (the operator executes it
/// conditionally).
///
/// # Errors
///
/// Returns distinctness data exceptions, cancellation, the set-op key-cap
/// resource error, or the memory-budget resource error. No partial rows are
/// returned on failure.
pub(crate) fn combine_set_rows(
    op: SetOp,
    lhs: Vec<Binding>,
    rhs: &[Binding],
    caps: &ImplDefinedCaps,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(Vec<Binding>, usize), ExecutorError> {
    match op {
        SetOp::UnionAll => {
            let total = lhs.len().saturating_add(rhs.len());
            let width = lhs
                .first()
                .or(rhs.first())
                .map_or(0, |row| row.values().len());
            let reserved = reserve_rows(
                ctx,
                total,
                width,
                "batch set fanout exceeds the supported range",
            )?;
            let mut output = Vec::with_capacity(total.min(lhs.len().saturating_add(1)));
            output.extend(lhs);
            output.extend(rhs.iter().cloned());
            Ok((output, reserved))
        }
        SetOp::Union => union_distinct_rows(lhs, rhs, ctx),
        SetOp::Intersect | SetOp::IntersectAll | SetOp::Except | SetOp::ExceptAll => {
            counted_set_rows(op, lhs, rhs, caps, ctx)
        }
        SetOp::Otherwise => Err(ExecutorError::ImplementationDefined {
            detail: "batch set operator handles OTHERWISE conditionally, not by combining",
        }),
    }
}

/// `UNION` (deduplicating): first-occurrence order over both arms.
///
/// Replicates the row concat-then-distinct sequence: per-row distinctness
/// observation in output order with first-seen retention. Uncapped, as on
/// the row path; the memory budget still bounds output amplification.
fn union_distinct_rows(
    lhs: Vec<Binding>,
    rhs: &[Binding],
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(Vec<Binding>, usize), ExecutorError> {
    use selene_core::ComparisonMode;
    let span = crate::SourceSpan::default();
    let width = lhs
        .first()
        .or_else(|| rhs.first())
        .map_or(0, |row| row.values().len());
    let mut seen: FxHashSet<RuntimeEqKey> = FxHashSet::default();
    seen.reserve(lhs.len().saturating_add(rhs.len()).min(1_024));
    let mut domains = ComparisonDomain::default();
    let mut output = Vec::new();
    let mut since_check = 0usize;
    let mut reserved = 0usize;
    for row in lhs.into_iter().chain(rhs.iter().cloned()) {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        domains.observe(row.values(), ComparisonMode::Distinctness)?;
        if seen.insert(RuntimeEqKey::from_row(row.values().to_vec())) {
            let bytes = reserve_rows(
                ctx,
                1,
                width,
                "batch set fanout exceeds the supported range",
            )
            .inspect_err(|_| ctx.budget_mut().release(reserved))?;
            reserved = reserved.saturating_add(bytes);
            output.push(row);
        }
    }
    // The output reservation transfers to the caller; the transient key
    // set drops here, peak-recorded.
    Ok((output, reserved))
}

/// Counted set operators with multiset-aware duplicate handling.
///
/// Replicates the row `execute_counted` sequence exactly: distinctness
/// observation over left-then-right rows, right-arm counting bounded by the
/// production key cap, then the left-arm pass with per-operator semantics.
/// The key-cap diagnostic is byte-identical to the row path.
fn counted_set_rows(
    op: SetOp,
    lhs: Vec<Binding>,
    rhs: &[Binding],
    caps: &ImplDefinedCaps,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(Vec<Binding>, usize), ExecutorError> {
    use selene_core::ComparisonMode;
    let span = crate::SourceSpan::default();
    let width = lhs
        .first()
        .or_else(|| rhs.first())
        .map_or(0, |row| row.values().len());
    let mut domains = ComparisonDomain::default();
    for row in lhs.iter().chain(rhs.iter()) {
        domains.observe(row.values(), ComparisonMode::Distinctness)?;
    }
    let mut rhs_counts: FxHashMap<RuntimeEqKey, usize> = FxHashMap::default();
    let mut since_check = 0usize;
    for row in rhs {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        let key = RuntimeEqKey::from_row(row.values().to_vec());
        if !rhs_counts.contains_key(&key) && rhs_counts.len() >= caps.set_op_key_cap() {
            return Err(set_op_key_cap_exceeded());
        }
        *rhs_counts.entry(key).or_insert(0) += 1;
    }
    let mut output = Vec::new();
    let mut seen: FxHashSet<RuntimeEqKey> = FxHashSet::default();
    let mut reserved = 0usize;
    since_check = 0;
    for row in lhs {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        let key = RuntimeEqKey::from_row(row.values().to_vec());
        let keep = match op {
            SetOp::IntersectAll => {
                if let Some(count) = rhs_counts.get_mut(&key)
                    && *count > 0
                {
                    *count -= 1;
                    true
                } else {
                    false
                }
            }
            SetOp::Intersect => rhs_counts.contains_key(&key) && insert_seen(&mut seen, key, caps)?,
            SetOp::ExceptAll => {
                if let Some(count) = rhs_counts.get_mut(&key)
                    && *count > 0
                {
                    *count -= 1;
                    false
                } else {
                    true
                }
            }
            SetOp::Except => !rhs_counts.contains_key(&key) && insert_seen(&mut seen, key, caps)?,
            SetOp::Union | SetOp::UnionAll | SetOp::Otherwise => {
                return Err(ExecutorError::ImplementationDefined {
                    detail: "batch counted set reached a non-counted operator",
                });
            }
        };
        if keep {
            let bytes = reserve_rows(
                ctx,
                1,
                width,
                "batch set fanout exceeds the supported range",
            )
            .inspect_err(|_| ctx.budget_mut().release(reserved))?;
            reserved = reserved.saturating_add(bytes);
            output.push(row);
        }
    }
    Ok((output, reserved))
}

/// First-seen tracking for deduplicating set operators with the production cap.
fn insert_seen(
    seen: &mut FxHashSet<RuntimeEqKey>,
    key: RuntimeEqKey,
    caps: &ImplDefinedCaps,
) -> Result<bool, ExecutorError> {
    if seen.contains(&key) {
        return Ok(false);
    }
    if seen.len() >= caps.set_op_key_cap() {
        return Err(set_op_key_cap_exceeded());
    }
    Ok(seen.insert(key))
}

/// Name one set operator for schema-compatibility diagnostics.
///
/// Identical labels to the row path.
const fn op_name(op: SetOp) -> &'static str {
    match op {
        SetOp::Union | SetOp::UnionAll => "UNION",
        SetOp::Intersect | SetOp::IntersectAll => "INTERSECT",
        SetOp::Except | SetOp::ExceptAll => "EXCEPT",
        SetOp::Otherwise => "OTHERWISE",
    }
}

/// Pull-based set-composition over a materialized left input and a right plan.
///
/// The right arm executes once at `init` through the read-only plan runner;
/// `OTHERWISE` executes it only when the left input is empty. Output schema
/// is the left input schema, as on the row path.
pub(crate) struct BatchSet<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
    op: SetOp,
    rhs: &'plan ExecutionPlan,
    schema: BindingTableSchema,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
    rows: Vec<Binding>,
    reserved_bytes: usize,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl<'x, 'a, 'ctx, 'g, 'plan> BatchSet<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct a set-composition over `child` with right arm `rhs`.
    ///
    /// `schema` is the left input schema and the declared output schema.
    /// The driver guarantees the right arm is read-only (see the effect
    /// gate in [`query`](super::query)), so read-only arm execution is
    /// exact, never a narrowed fallback.
    pub(crate) const fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        op: SetOp,
        rhs: &'plan ExecutionPlan,
        schema: BindingTableSchema,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        Self {
            child,
            op,
            rhs,
            schema,
            eval,
            policy,
            rows: Vec::new(),
            reserved_bytes: 0,
            cursor: 0,
            state: OperatorState::Created,
            batches_produced: 0,
        }
    }

    /// Return the operator's lifecycle state.
    ///
    /// Test seam for lifecycle assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn state(&self) -> OperatorState {
        self.state
    }

    /// Return the number of batches produced so far.
    ///
    /// Test seam for pull-count assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn batches_produced(&self) -> u64 {
        self.batches_produced
    }

    fn init_inner(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch set init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.child.init(ctx)?;
        let lhs = materialize_left(&mut self.child, ctx)?;
        // Positional compatibility first, exactly as the row path orders it
        // (plan-time name equality already ran at lowering).
        assert_compatible_schemas(op_name(self.op), &self.schema, &self.rhs.output_schema)?;
        let width = self.schema.columns.len();
        // Bound the materialized arms before combining; the combined output
        // reservation below accumulates per emitted row.
        let arms_reserved = reserve_rows(
            ctx,
            lhs.len(),
            width,
            "batch set build exceeds the supported range",
        )?;
        let outcome = self.combine(lhs, width, ctx);
        ctx.budget_mut().release(arms_reserved);
        let (rows, reserved) = outcome?;
        self.rows = rows;
        self.reserved_bytes = reserved;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
    }

    fn combine(
        &mut self,
        lhs: Vec<Binding>,
        width: usize,
        ctx: &mut BatchExecutionContext<'_>,
    ) -> Result<(Vec<Binding>, usize), ExecutorError> {
        if matches!(self.op, SetOp::Otherwise) {
            return self.combine_otherwise(lhs, ctx);
        }
        let rhs_table = plan_runner::execute_plan_read_only(self.rhs, self.eval.tx)?;
        let (_, rhs_rows) = rhs_table.into_parts();
        let rhs_reserved = reserve_rows(
            ctx,
            rhs_rows.len(),
            width,
            "batch set build exceeds the supported range",
        )?;
        let outcome = combine_set_rows(
            self.op,
            lhs,
            &rhs_rows,
            self.eval.tx.impl_defined_caps(),
            ctx,
        );
        ctx.budget_mut().release(rhs_reserved);
        outcome
    }

    fn combine_otherwise(
        &mut self,
        lhs: Vec<Binding>,
        ctx: &mut BatchExecutionContext<'_>,
    ) -> Result<(Vec<Binding>, usize), ExecutorError> {
        if !lhs.is_empty() {
            let reserved = reserve_rows(
                ctx,
                lhs.len(),
                self.schema.columns.len(),
                "batch set fanout exceeds the supported range",
            )?;
            return Ok((lhs, reserved));
        }
        let rhs_table = plan_runner::execute_plan_read_only(self.rhs, self.eval.tx)?;
        assert_compatible_schemas(op_name(self.op), &self.schema, rhs_table.schema())?;
        let (_, rhs_rows) = rhs_table.into_parts();
        let reserved = reserve_rows(
            ctx,
            rhs_rows.len(),
            self.schema.columns.len(),
            "batch set fanout exceeds the supported range",
        )?;
        Ok((rhs_rows, reserved))
    }

    fn pull_inner(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        ctx.ensure_generation()?;
        if self.cursor >= self.rows.len() {
            self.state = OperatorState::Exhausted;
            return Ok(None);
        }
        let span = crate::SourceSpan::default();
        ctx.check_cancel(span)?;
        let width = self.schema.columns.len();
        let take = self
            .policy
            .rows_per_batch(super::join::row_bytes_estimate(width).max(1))
            .min(self.rows.len() - self.cursor);
        let mut columns: Vec<(Vec<Value>, Vec<bool>)> = Vec::with_capacity(width);
        for _ in 0..width {
            let mut values = buffer.take_values();
            let mut nulls = buffer.take_nulls();
            values.clear();
            nulls.clear();
            columns.push((values, nulls));
        }
        for row in &self.rows[self.cursor..self.cursor + take] {
            for (slot, (column, nulls)) in columns.iter_mut().enumerate() {
                let value = row.get(slot).cloned().unwrap_or(Value::Null);
                nulls.push(value == Value::Null);
                column.push(value);
            }
        }
        self.cursor += take;
        self.batches_produced += 1;
        ctx.finish_batch(take);
        let batch_columns = columns
            .into_iter()
            .map(|(values, nulls)| {
                BatchColumn::from_parts(values, nulls).map_err(|_| {
                    ExecutorError::ImplementationDefined {
                        detail: "batch set built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch =
            BindingBatch::from_batch_columns(self.schema.clone(), batch_columns).map_err(|_| {
                ExecutorError::ImplementationDefined {
                    detail: "batch set built a malformed batch",
                }
            })?;
        ctx.budget_mut()
            .reserve(batch.estimated_bytes())
            .map_err(|err| err.into_executor_error(span))?;
        Ok(Some(batch))
    }
}

/// Pull one child operator to fully materialized row storage.
///
/// Consumed batches return their storage to a local buffer and release their
/// budget claims, so only the returned rows stay live.
fn materialize_left(
    child: &mut Box<dyn PhysicalOperator + '_>,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<Vec<Binding>, ExecutorError> {
    let mut buffer = BatchBuffer::new();
    let mut rows = Vec::new();
    while let Some(batch) = child.next_batch(ctx, &mut buffer)? {
        rows.reserve(batch.logical_rows());
        for index in 0..batch.logical_rows() {
            rows.push(Binding::new(batch.logical_row(index)));
        }
        ctx.budget_mut().release(batch.estimated_bytes());
        batch.recycle(&mut buffer);
    }
    Ok(rows)
}

impl PhysicalOperator for BatchSet<'_, '_, '_, '_, '_> {
    fn init(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        let outcome = self.init_inner(ctx);
        if outcome.is_err() {
            self.state = OperatorState::Failed;
            if self.reserved_bytes > 0 {
                ctx.budget_mut().release(self.reserved_bytes);
                self.reserved_bytes = 0;
            }
        }
        outcome
    }

    fn next_batch(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        if self.state == OperatorState::Exhausted {
            return Ok(None);
        }
        if self.state != OperatorState::Open {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch set pull is legal only while Open",
            });
        }
        let outcome = self.pull_inner(ctx, buffer);
        if outcome.is_err() {
            self.state = OperatorState::Failed;
        }
        outcome
    }

    fn close(&mut self, ctx: &mut BatchExecutionContext<'_>) {
        self.child.close(ctx);
        self.rows.clear();
        if self.reserved_bytes > 0 {
            ctx.budget_mut().release(self.reserved_bytes);
            self.reserved_bytes = 0;
        }
        self.cursor = 0;
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.schema
    }
}
