//! Batch correlated pipeline composition.
//!
//! This module carries the per-input-row pipeline family into batches:
//! non-leading [`PipelineOp::Match`](crate::PipelineOp::Match) /
//! [`PipelineOp::OptionalMatch`](crate::PipelineOp::OptionalMatch) and
//! `NEXT`-chained [`PipelineOp::Chain`](crate::PipelineOp::Chain) /
//! [`PipelineOp::CorrelatedChain`](crate::PipelineOp::CorrelatedChain)
//! blocks. The row adapters for these shapes are out of production use for
//! batch-accepted plans.
//!
//! [`BatchMatch`] materializes its input, then evaluates the inner pattern
//! once per input row with that row as the correlated seed (through
//! [`trace_subtree`](super::tree::trace_subtree)). Seeding reuses the row
//! path's schema and seed authorities
//! ([`target_schema`](super::super::pipeline::target_schema) /
//! [`seed_row`](super::super::pipeline::seed_row)), so correlated bindings
//! unify per row and never leak across input rows; the optional variant
//! preserves unmatched seeds exactly as the row operator does.
//!
//! [`BatchChain`] drains (but discards) its input before running the right
//! block once, preserving the row path's error order: left-side failures
//! surface before the right block executes. [`BatchCorrelatedChain`] runs
//! the right block once per input row with a single-row seed table through
//! the driver's seeded-subplan helper, which batch-routes the block's
//! pattern and prefix exactly as the top-level driver does.
//!
//! Right blocks are gated read-only by the driver (see the effect gate in
//! [`query`](super::query)), so read-only block execution is exact. Every
//! emitted row reserves its estimate before it is pushed; a bounded budget
//! fails with the typed resource error instead of truncating.

use selene_core::Value;

use crate::{
    ExecutionPlan, PatternPlan,
    plan::BindingTableSchema,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError},
};

use super::super::{pattern, pipeline, plan_runner};
use super::tree;
use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    join::{reserve_rows, row_bytes_estimate},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};

/// Pull-based non-leading match over a materialized input.
///
/// `optional` selects inner versus left-outer seed preservation. Output
/// schema is the match target schema (input columns plus new pattern
/// columns).
pub(crate) struct BatchMatch<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
    pattern: &'plan PatternPlan,
    target: BindingTableSchema,
    input_schema: BindingTableSchema,
    optional: bool,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
    rows: Vec<Binding>,
    reserved_bytes: usize,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl<'x, 'a, 'ctx, 'g, 'plan> BatchMatch<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct a match over `child` extending into `target`.
    ///
    /// `target` must be the row path's target schema for this input (see
    /// [`target_schema`](super::super::pipeline::target_schema)); the driver
    /// computes it from the child's declared output schema.
    pub(crate) const fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        pattern: &'plan PatternPlan,
        target: BindingTableSchema,
        input_schema: BindingTableSchema,
        optional: bool,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        Self {
            child,
            pattern,
            target,
            input_schema,
            optional,
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

    fn init_inner(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch match init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.child.init(ctx)?;
        let input = materialize_input(&mut self.child, ctx)?;
        let width = self.target.columns.len();
        let input_reserved = reserve_rows(
            ctx,
            input.len(),
            self.input_schema.columns.len(),
            "batch match build exceeds the supported range",
        )?;
        let outcome = self.match_all(&input, width, ctx);
        ctx.budget_mut().release(input_reserved);
        let (rows, reserved) = outcome?;
        self.rows = rows;
        self.reserved_bytes = reserved;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
    }

    fn match_all(
        &mut self,
        input: &[Binding],
        width: usize,
        ctx: &mut BatchExecutionContext<'_>,
    ) -> Result<(Vec<Binding>, usize), ExecutorError> {
        let span = crate::SourceSpan::default();
        let mut output: Vec<Binding> = Vec::new();
        let mut reserved = 0usize;
        let mut since_check = 0usize;
        for row in input {
            if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
                ctx.check_cancel(span)?;
                since_check = 0;
            }
            since_check += 1;
            // Batch prefixes never carry mutation insert sites (mutations
            // always end the prefix), so seeded evaluation starts site-free
            // exactly as the row path's site-free pattern rows do.
            debug_assert!(row.insert_sites().is_empty());
            // Seed in input coordinates; the inner evaluation writes into
            // target coordinates with shared columns index-stable.
            let seed = pipeline::seed_row(row, &self.input_schema, &self.target);
            let inner = tree::trace_subtree(
                &self.pattern.join_tree,
                self.pattern,
                &self.target,
                Some(seed.clone()),
                self.eval,
                self.policy,
                ctx,
            )?;
            let mut kept = 0usize;
            for matched in inner {
                if !pattern::filter_predicates_pass(
                    &self.pattern.filters,
                    self.pattern,
                    &matched,
                    &self.target,
                    &self.eval,
                )? {
                    continue;
                }
                let bytes = reserve_rows(
                    ctx,
                    1,
                    width,
                    "batch match fanout exceeds the supported range",
                )
                .inspect_err(|_| ctx.budget_mut().release(reserved))?;
                reserved = reserved.saturating_add(bytes);
                output.push(matched);
                kept += 1;
            }
            if kept == 0 && self.optional {
                let bytes = reserve_rows(
                    ctx,
                    1,
                    width,
                    "batch match fanout exceeds the supported range",
                )
                .inspect_err(|_| ctx.budget_mut().release(reserved))?;
                reserved = reserved.saturating_add(bytes);
                output.push(seed);
            }
        }
        Ok((output, reserved))
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
        pull_rows(
            &mut self.cursor,
            &mut self.batches_produced,
            &self.rows,
            &self.target,
            self.policy,
            ctx,
            buffer,
            "batch match built a malformed batch",
        )
    }
}

/// Pull one child operator to fully materialized row storage.
///
/// Consumed batches return their storage to a local buffer and release their
/// budget claims, so only the returned rows stay live.
fn materialize_input(
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

/// Serve policy-sized batches from materialized rows.
///
/// Shared pull body for the correlated operators: slices `rows` at `cursor`,
/// advances the produced counter, and claims the batch budget.
#[allow(clippy::too_many_arguments)]
fn pull_rows(
    cursor: &mut usize,
    produced: &mut u64,
    rows: &[Binding],
    schema: &BindingTableSchema,
    policy: BatchPolicy,
    ctx: &mut BatchExecutionContext<'_>,
    buffer: &mut BatchBuffer,
    malformed: &'static str,
) -> Result<Option<BindingBatch>, ExecutorError> {
    let width = schema.columns.len();
    let take = policy
        .rows_per_batch(row_bytes_estimate(width).max(1))
        .min(rows.len() - *cursor);
    let mut columns: Vec<(Vec<Value>, Vec<bool>)> = Vec::with_capacity(width);
    for _ in 0..width {
        let mut values = buffer.take_values();
        let mut nulls = buffer.take_nulls();
        values.clear();
        nulls.clear();
        columns.push((values, nulls));
    }
    for row in &rows[*cursor..*cursor + take] {
        for (slot, (column, nulls)) in columns.iter_mut().enumerate() {
            let value = row.get(slot).cloned().unwrap_or(Value::Null);
            nulls.push(value == Value::Null);
            column.push(value);
        }
    }
    *cursor += take;
    *produced += 1;
    ctx.finish_batch(take);
    let batch_columns = columns
        .into_iter()
        .map(|(values, nulls)| {
            BatchColumn::from_parts(values, nulls)
                .map_err(|_| ExecutorError::ImplementationDefined { detail: malformed })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let batch = BindingBatch::from_batch_columns(schema.clone(), batch_columns)
        .map_err(|_| ExecutorError::ImplementationDefined { detail: malformed })?;
    ctx.budget_mut()
        .reserve(batch.estimated_bytes())
        .map_err(|err| err.into_executor_error(crate::SourceSpan::default()))?;
    Ok(Some(batch))
}

impl PhysicalOperator for BatchMatch<'_, '_, '_, '_, '_> {
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
                detail: "batch match pull is legal only while Open",
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
        self.rows.shrink_to_fit();
        if self.reserved_bytes > 0 {
            ctx.budget_mut().release(self.reserved_bytes);
            self.reserved_bytes = 0;
        }
        self.cursor = 0;
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.target
    }
}

/// Pull-based uncorrelated `NEXT` chain: discard the input, run the block.
///
/// The child drains fully before the right block executes so left-side
/// failures surface first, exactly as the row path orders them. Output
/// schema is the right block's output schema.
pub(crate) struct BatchChain<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
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

impl<'x, 'a, 'ctx, 'g, 'plan> BatchChain<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct a chain over `child` into right block `rhs`.
    pub(crate) fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        rhs: &'plan ExecutionPlan,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        let schema = rhs.output_schema.clone();
        Self {
            child,
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

    fn init_inner(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch chain init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.child.init(ctx)?;
        drain_child(&mut self.child, ctx)?;
        let rhs_table = plan_runner::execute_plan_read_only(self.rhs, self.eval.tx)?;
        let width = self.schema.columns.len();
        let (_, rhs_rows) = rhs_table.into_parts();
        let reserved = reserve_rows(
            ctx,
            rhs_rows.len(),
            width,
            "batch chain fanout exceeds the supported range",
        )?;
        self.rows = rhs_rows;
        self.reserved_bytes = reserved;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
    }
}

/// Drain one child operator, discarding rows but surfacing its errors.
///
/// Preserves the row path's left-before-right failure order for chains.
fn drain_child(
    child: &mut Box<dyn PhysicalOperator + '_>,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(), ExecutorError> {
    let mut buffer = BatchBuffer::new();
    while let Some(batch) = child.next_batch(ctx, &mut buffer)? {
        ctx.budget_mut().release(batch.estimated_bytes());
        batch.recycle(&mut buffer);
    }
    Ok(())
}

impl PhysicalOperator for BatchChain<'_, '_, '_, '_, '_> {
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
                detail: "batch chain pull is legal only while Open",
            });
        }
        ctx.ensure_generation()?;
        if self.cursor >= self.rows.len() {
            self.state = OperatorState::Exhausted;
            return Ok(None);
        }
        let span = crate::SourceSpan::default();
        ctx.check_cancel(span)?;
        pull_rows(
            &mut self.cursor,
            &mut self.batches_produced,
            &self.rows,
            &self.schema,
            self.policy,
            ctx,
            buffer,
            "batch chain built a malformed batch",
        )
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

/// Pull-based correlated `NEXT` chain: run the block once per input row.
///
/// Each input row becomes a single-row seed table for the right block,
/// evaluated through the driver's seeded-subplan helper. Output rows
/// concatenate in input order with the right block's output schema.
pub(crate) struct BatchCorrelatedChain<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
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

impl<'x, 'a, 'ctx, 'g, 'plan> BatchCorrelatedChain<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct a correlated chain over `child` into right block `rhs`.
    pub(crate) fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        rhs: &'plan ExecutionPlan,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        let schema = rhs.output_schema.clone();
        Self {
            child,
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

    fn init_inner(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch correlated chain init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.child.init(ctx)?;
        let input_schema = self.child.output_schema().clone();
        let input = materialize_input(&mut self.child, ctx)?;
        let width = self.schema.columns.len();
        let input_reserved = reserve_rows(
            ctx,
            input.len(),
            input_schema.columns.len(),
            "batch chain build exceeds the supported range",
        )?;
        let outcome = self.chain_all(&input, &input_schema, width, ctx);
        ctx.budget_mut().release(input_reserved);
        let (rows, reserved) = outcome?;
        self.rows = rows;
        self.reserved_bytes = reserved;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
    }

    fn chain_all(
        &mut self,
        input: &[Binding],
        input_schema: &BindingTableSchema,
        width: usize,
        ctx: &mut BatchExecutionContext<'_>,
    ) -> Result<(Vec<Binding>, usize), ExecutorError> {
        let span = crate::SourceSpan::default();
        let mut output: Vec<Binding> = Vec::new();
        let mut reserved = 0usize;
        let mut since_check = 0usize;
        for row in input {
            if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
                ctx.check_cancel(span)?;
                since_check = 0;
            }
            since_check += 1;
            debug_assert!(row.insert_sites().is_empty());
            // One single-row seed table per input row, as the row operator
            // builds it: correlated bindings cannot cross input rows because
            // each block evaluation observes exactly one seed.
            let seed = BindingTable::new(input_schema.clone(), vec![row.clone()]);
            let table =
                super::query::execute_seeded_subplan(self.rhs, seed, self.eval.tx, self.policy)?;
            let (_, rows) = table.into_parts();
            for block_row in rows {
                let bytes = reserve_rows(
                    ctx,
                    1,
                    width,
                    "batch chain fanout exceeds the supported range",
                )
                .inspect_err(|_| ctx.budget_mut().release(reserved))?;
                reserved = reserved.saturating_add(bytes);
                output.push(block_row);
            }
        }
        Ok((output, reserved))
    }
}

impl PhysicalOperator for BatchCorrelatedChain<'_, '_, '_, '_, '_> {
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
                detail: "batch correlated chain pull is legal only while Open",
            });
        }
        ctx.ensure_generation()?;
        if self.cursor >= self.rows.len() {
            self.state = OperatorState::Exhausted;
            return Ok(None);
        }
        let span = crate::SourceSpan::default();
        ctx.check_cancel(span)?;
        pull_rows(
            &mut self.cursor,
            &mut self.batches_produced,
            &self.rows,
            &self.schema,
            self.policy,
            ctx,
            buffer,
            "batch chain built a malformed batch",
        )
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
