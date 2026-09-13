//! Batch left-outer join for `OPTIONAL MATCH`.
//!
//! [`BatchOuterJoin`] carries one `JoinTree::Outer` into batches. The left
//! input materializes through the pull protocol; the right subtree then
//! evaluates once per left row with that row as the correlated seed (through
//! [`trace_subtree`](super::tree::trace_subtree)), so right-side scans unify
//! seed bindings and right-side filters observe left bindings exactly as the
//! row outer join does. Per-left-row evaluation also keeps correlated
//! bindings from leaking across input rows: every right evaluation starts
//! from its own left row and nothing else.
//!
//! Each right row passes the clause-scoped `right_filters` and then the
//! shared-key match ([`rows_match_on_resolved_key`](super::super::pattern::rows_match_on_resolved_key),
//! where null keys never match). Matched pairs merge with
//! [`merge_rows`](super::super::pattern::merge_rows); a left row with no
//! match is preserved as-is, keeping the no-match versus
//! null-containing-row distinction. Output order is left-major with right
//! walk order inside each left row, exactly as the row operator emits.
//!
//! Memory is accounted before allocation: every emitted row reserves its
//! estimate before it is pushed, so a bounded budget fails with the typed
//! resource error (`5GQL1`) instead of truncating. The operator holds its
//! output reservation until `close`.

use selene_core::{DbString, Value};

use crate::{
    FilterPredicate, JoinTree, PatternPlan,
    plan::BindingTableSchema,
    runtime::{Binding, EvalCtx, ExecutorError},
};

use super::super::pattern;
use super::tree;
use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    join::row_bytes_estimate,
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};

/// Pull-based left-outer join: materialized left input plus per-row
/// correlated right evaluation.
///
/// Output rows keep the pattern schema both sides share.
pub(crate) struct BatchOuterJoin<'x, 'a, 'ctx, 'g, 'plan> {
    left: Box<dyn PhysicalOperator + 'x>,
    right: &'plan JoinTree,
    pattern: &'plan PatternPlan,
    key: &'plan [DbString],
    right_filters: &'plan [FilterPredicate],
    schema: BindingTableSchema,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
    rows: Vec<Vec<Value>>,
    reserved_bytes: usize,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl<'x, 'a, 'ctx, 'g, 'plan> BatchOuterJoin<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct an outer join over `left` with a correlated `right` subtree.
    ///
    /// Correlated evaluation builds `right` through the single physical tree
    /// assembler. Malformed IR reports an error, never a second executor.
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn new(
        left: Box<dyn PhysicalOperator + 'x>,
        right: &'plan JoinTree,
        pattern: &'plan PatternPlan,
        key: &'plan [DbString],
        right_filters: &'plan [FilterPredicate],
        schema: BindingTableSchema,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        Self {
            left,
            right,
            pattern,
            key,
            right_filters,
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

    /// Return the number of batches produced so far.
    ///
    /// Test seam for pull-count assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn batches_produced(&self) -> u64 {
        self.batches_produced
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
                detail: "batch outer join init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.left.init(ctx)?;
        let left_rows = materialize_left(&mut self.left, ctx)?;
        let key_indexes = pattern::resolve_key(&self.schema, self.key)?;
        let width = self.schema.columns.len();
        // Bound the materialized left side before the per-row loop; the
        // output reservation below accumulates per emitted row.
        let left_reserved = super::join::reserve_rows(
            ctx,
            left_rows.len(),
            width,
            "batch outer join build exceeds the supported range",
        )?;
        let outcome = self.join_all(&left_rows, &key_indexes, width, ctx);
        ctx.budget_mut().release(left_reserved);
        let (rows, reserved) = outcome?;
        self.rows = rows;
        self.reserved_bytes = reserved;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
    }

    fn join_all(
        &mut self,
        left_rows: &[Binding],
        key_indexes: &[usize],
        width: usize,
        ctx: &mut BatchExecutionContext<'_>,
    ) -> Result<(Vec<Vec<Value>>, usize), ExecutorError> {
        let span = crate::SourceSpan::default();
        let mut output: Vec<Vec<Value>> = Vec::new();
        let mut reserved = 0usize;
        let mut since_check = 0usize;
        // Failing to reserve an emitted row aborts with no partial output:
        // `output` stays local and is dropped on error.
        let emit = |values: Vec<Value>,
                    output: &mut Vec<Vec<Value>>,
                    reserved: &mut usize,
                    ctx: &mut BatchExecutionContext<'_>| {
            let bytes = super::join::reserve_rows(
                ctx,
                1,
                width,
                "batch outer join fanout exceeds the supported range",
            )?;
            *reserved = reserved.saturating_add(bytes);
            output.push(values);
            Ok::<(), ExecutorError>(())
        };
        for left_row in left_rows {
            if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
                ctx.check_cancel(span)?;
                since_check = 0;
            }
            since_check += 1;
            debug_assert!(left_row.insert_sites().is_empty());
            let right_rows = tree::trace_subtree(
                self.right,
                self.pattern,
                &self.schema,
                Some(left_row.clone()),
                self.eval,
                self.policy,
                ctx,
            )?;
            let mut matched = false;
            for right_row in right_rows {
                if !pattern::filter_predicates_pass(
                    self.right_filters,
                    self.pattern,
                    &right_row,
                    &self.schema,
                    &self.eval,
                )? {
                    continue;
                }
                if !pattern::rows_match_on_resolved_key(left_row, &right_row, key_indexes)? {
                    continue;
                }
                let merged = pattern::merge_rows(left_row, &right_row, &self.schema);
                emit(merged.values().to_vec(), &mut output, &mut reserved, ctx)
                    .inspect_err(|_| ctx.budget_mut().release(reserved))?;
                matched = true;
            }
            if !matched {
                emit(left_row.values().to_vec(), &mut output, &mut reserved, ctx)
                    .inspect_err(|_| ctx.budget_mut().release(reserved))?;
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
        let width = self.schema.columns.len();
        let take = self
            .policy
            .rows_per_batch(row_bytes_estimate(width).max(1))
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
                        detail: "batch outer join built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch =
            BindingBatch::from_batch_columns(self.schema.clone(), batch_columns).map_err(|_| {
                ExecutorError::ImplementationDefined {
                    detail: "batch outer join built a malformed batch",
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

impl PhysicalOperator for BatchOuterJoin<'_, '_, '_, '_, '_> {
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
                detail: "batch outer join pull is legal only while Open",
            });
        }
        let outcome = self.pull_inner(ctx, buffer);
        if outcome.is_err() {
            self.state = OperatorState::Failed;
        }
        outcome
    }

    fn close(&mut self, ctx: &mut BatchExecutionContext<'_>) {
        self.left.close(ctx);
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
        &self.schema
    }
}
