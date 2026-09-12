//! Batch sorting, bounded top-K, and order-carrier trimming (F04-PR04).
//!
//! [`BatchSort`] carries one [`PipelineOp::OrderBy`](crate::PipelineOp::OrderBy)
//! into batches, [`BatchTopK`] carries the optimizer-fused
//! [`PipelineOp::TopK`](crate::PipelineOp::TopK), and
//! [`BatchTrimCarriers`] carries
//! [`PipelineOp::TrimOrderCarriers`](crate::PipelineOp::TrimOrderCarriers).
//! Sort and top-K materialize the child through the pull protocol, then run
//! the ordering combinatorics over the materialized rows and serve the
//! output in policy-sized slices, mirroring
//! [`BatchHashJoin`](super::join)'s materialize-then-slice shape. Trimming
//! streams: it truncates each child batch positionally without
//! materializing.
//!
//! Ordering semantics reuse the row path's semantic services by
//! construction: keys evaluate through the shared sort-key evaluator,
//! tuple comparison runs the shared [`compare_key_tuples`] (per-key
//! direction, explicit `NULLS FIRST`/`LAST` with the same
//! direction-dependent defaults, and the selected binary string
//! collation), cross-value compatibility is enforced by the shared
//! [`ComparisonDomain`] in `Ordering` mode, and sorting is stable on both
//! engines, so ties keep input order and no implicit total ordering is
//! invented. Top-K replicates the row heap sequence including its
//! input-sequence tiebreak, which selects the same deterministic window a
//! full stable sort plus page would; the operator is only ever built from
//! the optimizer-fused `TopK` op, never invented from an unordered input.
//!
//! Memory is accounted before allocation: sort buffers and the top-K heap
//! reserve their estimates up front (the heap reserves only its retained
//! window, which is the point of the fused shape), and a bounded budget
//! fails with the typed resource error (`5GQL1`) instead of truncating. The
//! operators hold their output reservations until `close`.

use std::{cmp::Ordering, cmp::Reverse, collections::BinaryHeap, sync::Arc};

use selene_core::Value;

use crate::{
    LimitAmount, OrderKey,
    plan::BindingTableSchema,
    runtime::{
        Binding, EvalCtx, ExecutorError,
        comparison_domain::ComparisonDomain,
        pipeline::{
            order_by::{compare_key_tuples, evaluate_key_tuple},
            resolve_amount, u64_to_bounded_usize,
        },
    },
};

use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    join::{reserve_rows, row_bytes_estimate},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};

/// One materialized row with its evaluated sort-key tuple.
struct KeyedRow {
    tuple: Vec<Value>,
    row: Binding,
}

/// Sort materialized rows with the shared key evaluator and comparator.
///
/// `input_rows` are the child's rows in pull order; the stable sort keeps
/// ties in that order, exactly as the row path does. Returns the ordered
/// rows plus the output reservation the caller holds until the rows drop;
/// transient key tuples are released before returning.
///
/// # Errors
///
/// Returns evaluation and ordering data exceptions, cancellation, or the
/// memory-budget resource error. No partial rows are returned on failure.
pub(crate) fn sort_rows(
    keys: &[OrderKey],
    input_schema: &BindingTableSchema,
    input_rows: Vec<Binding>,
    eval: &EvalCtx<'_, '_, '_, '_>,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(Vec<Binding>, usize), ExecutorError> {
    let span = crate::SourceSpan::default();
    let width = input_schema.columns.len();
    // Sort buffers hold every key tuple alongside its row for the whole
    // sort, so the estimate covers both before any tuple evaluates.
    let build_reserved = reserve_rows(
        ctx,
        input_rows.len(),
        width.saturating_add(keys.len()),
        "batch sort buffer exceeds the supported range",
    )?;
    let ordered = key_rows(keys, input_schema, input_rows, eval, ctx);
    ctx.budget_mut().release(build_reserved);
    let mut keyed = ordered?;
    ctx.check_cancel(span)?;
    keyed.sort_by(|lhs, rhs| compare_key_tuples(&lhs.tuple, &rhs.tuple, keys));
    ctx.check_cancel(span)?;
    let reserved = reserve_rows(
        ctx,
        keyed.len(),
        width,
        "batch sort fanout exceeds the supported range",
    )?;
    Ok((keyed.into_iter().map(|row| row.row).collect(), reserved))
}

/// Evaluate one sort-key tuple per row with domain observation.
///
/// Replicates the row accumulation sequence: stride cancellation, shared
/// key evaluation, and `Ordering` domain observation in pull order.
fn key_rows(
    keys: &[OrderKey],
    input_schema: &BindingTableSchema,
    input_rows: Vec<Binding>,
    eval: &EvalCtx<'_, '_, '_, '_>,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<Vec<KeyedRow>, ExecutorError> {
    use selene_core::ComparisonMode;
    let span = crate::SourceSpan::default();
    let mut keyed = Vec::with_capacity(input_rows.len());
    let mut domains = ComparisonDomain::default();
    let mut since_check = 0usize;
    for row in input_rows {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        let tuple = evaluate_key_tuple(keys, &row, input_schema, eval)?;
        domains.observe(&tuple, ComparisonMode::Ordering)?;
        keyed.push(KeyedRow { tuple, row });
    }
    Ok(keyed)
}

/// Retain the top-K window of materialized rows through a bounded heap.
///
/// `offset`/`count` are the already-resolved window amounts from the fused
/// `TopK` op. Replicates the row top-K sequence exactly: stride
/// cancellation, shared key evaluation, `Ordering` domain observation, a
/// heap capped at the retained window with input-sequence tiebreak, and a
/// final `(keys, sequence)` sort before slicing the window. Returns the
/// window rows plus the output reservation the caller holds.
///
/// # Errors
///
/// Returns evaluation and ordering data exceptions, cancellation, or the
/// memory-budget resource error. No partial rows are returned on failure.
pub(crate) fn top_k_rows(
    keys: &[OrderKey],
    offset: u64,
    count: u64,
    input_schema: &BindingTableSchema,
    input_rows: Vec<Binding>,
    eval: &EvalCtx<'_, '_, '_, '_>,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(Vec<Binding>, usize), ExecutorError> {
    let span = crate::SourceSpan::default();
    let width = input_schema.columns.len();
    let retained = u64_to_bounded_usize(offset.saturating_add(count), input_rows.len());
    if retained == 0 {
        return Ok((Vec::new(), 0));
    }
    // The heap holds at most the retained window (tuples plus rows), which
    // is the fused shape's memory point over a full sort; the estimate
    // covers exactly that live set.
    let build_reserved = reserve_rows(
        ctx,
        retained,
        width.saturating_add(keys.len()),
        "batch top-k heap exceeds the supported range",
    )?;
    let ranked = rank_rows(keys, input_schema, input_rows, eval, retained, ctx);
    ctx.budget_mut().release(build_reserved);
    let mut ranked = ranked?;
    ranked.sort_by(|lhs, rhs| lhs.desired_cmp(rhs));
    ctx.check_cancel(span)?;
    let start = u64_to_bounded_usize(offset, ranked.len());
    let rows = ranked
        .into_iter()
        .skip(start)
        .map(|row| row.row)
        .collect::<Vec<_>>();
    let reserved = reserve_rows(
        ctx,
        rows.len(),
        width,
        "batch top-k fanout exceeds the supported range",
    )?;
    Ok((rows, reserved))
}

/// Fill the bounded heap with ranked rows in pull order.
fn rank_rows(
    keys: &[OrderKey],
    input_schema: &BindingTableSchema,
    input_rows: Vec<Binding>,
    eval: &EvalCtx<'_, '_, '_, '_>,
    retained: usize,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<Vec<RankedRow>, ExecutorError> {
    use selene_core::ComparisonMode;
    let span = crate::SourceSpan::default();
    let keys = Arc::<[OrderKey]>::from(keys.to_vec());
    let mut heap = BinaryHeap::<Reverse<RankedRow>>::with_capacity(retained.saturating_add(1));
    let mut domains = ComparisonDomain::default();
    let mut since_check = 0usize;
    for (sequence, row) in input_rows.into_iter().enumerate() {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        let tuple = evaluate_key_tuple(&keys, &row, input_schema, eval)?;
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
    Ok(heap.into_iter().map(|Reverse(row)| row).collect())
}

/// One heap-ranked row: the row, its key tuple, its input sequence, and the
/// shared keys for comparison.
///
/// The `(keys, sequence)` order is total, so the retained window is
/// deterministic: among tied keys the earliest input rows win, exactly as
/// the row path's heap does.
struct RankedRow {
    tuple: Vec<Value>,
    sequence: usize,
    row: Binding,
    keys: Arc<[OrderKey]>,
}

impl RankedRow {
    fn desired_cmp(&self, rhs: &Self) -> Ordering {
        compare_key_tuples(&self.tuple, &rhs.tuple, &self.keys)
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

/// Pull one child operator to fully materialized row storage.
///
/// Consumed batches return their storage to a local buffer and release their
/// budget claims, so only the returned rows stay live. Cancellation is
/// checked per pull.
fn materialize_child(
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

/// Slice finalized rows into policy-sized batches.
///
/// Shared pull shape for the materializing sort operators (sort, top-K,
/// distinct): policy-sized takes, buffer-recycled columns, per-batch budget
/// claims, and completion telemetry. Returns `Ok(None)` (and marks
/// exhaustion) when the cursor reaches the end.
///
/// # Errors
///
/// Returns generation, cancellation, budget, or invariant errors without
/// exposing partial operator state to the caller.
pub(crate) fn slice_rows(
    schema: &BindingTableSchema,
    rows: &[Binding],
    cursor: &mut usize,
    policy: BatchPolicy,
    ctx: &mut BatchExecutionContext<'_>,
    buffer: &mut BatchBuffer,
    what: &'static str,
) -> Result<Option<BindingBatch>, ExecutorError> {
    ctx.ensure_generation()?;
    if *cursor >= rows.len() {
        return Ok(None);
    }
    let span = crate::SourceSpan::default();
    ctx.check_cancel(span)?;
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
    ctx.finish_batch(take);
    let batch_columns = columns
        .into_iter()
        .map(|(values, nulls)| {
            BatchColumn::from_parts(values, nulls)
                .map_err(|_| ExecutorError::ImplementationDefined { detail: what })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let batch = BindingBatch::from_batch_columns(schema.clone(), batch_columns)
        .map_err(|_| ExecutorError::ImplementationDefined { detail: what })?;
    ctx.budget_mut()
        .reserve(batch.estimated_bytes())
        .map_err(|err| err.into_executor_error(span))?;
    Ok(Some(batch))
}

/// Pull-based sort over a child operator's batches.
///
/// The child materializes fully at `init` (ordering needs every row before
/// the first output, as on the row path); pulls then slice the ordered
/// rows. Output schema is the child's schema.
pub(crate) struct BatchSort<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
    keys: &'plan [OrderKey],
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
    schema: BindingTableSchema,
    rows: Vec<Binding>,
    reserved_bytes: usize,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl<'x, 'a, 'ctx, 'g, 'plan> BatchSort<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct a sort over `child` with `keys`.
    pub(crate) fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        keys: &'plan [OrderKey],
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        let schema = child.output_schema().clone();
        Self {
            child,
            keys,
            eval,
            policy,
            schema,
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
                detail: "batch sort init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.child.init(ctx)?;
        let input_schema = self.child.output_schema().clone();
        let input_rows = materialize_child(&mut self.child, ctx)?;
        let (rows, reserved) = sort_rows(self.keys, &input_schema, input_rows, &self.eval, ctx)?;
        self.rows = rows;
        self.reserved_bytes = reserved;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
    }
}

impl PhysicalOperator for BatchSort<'_, '_, '_, '_, '_> {
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
                detail: "batch sort pull is legal only while Open",
            });
        }
        let outcome = slice_rows(
            &self.schema,
            &self.rows,
            &mut self.cursor,
            self.policy,
            ctx,
            buffer,
            "batch sort built a malformed batch",
        );
        match &outcome {
            Ok(None) => self.state = OperatorState::Exhausted,
            Ok(Some(_)) => self.batches_produced += 1,
            Err(_) => self.state = OperatorState::Failed,
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
        &self.schema
    }
}

/// Pull-based bounded top-K over a child operator's batches.
///
/// Built only from the optimizer-fused `TopK` op (adjacent `OrderBy` plus a
/// bounded `Limit`), so the retained window carries the fused
/// `ORDER BY`/`OFFSET`/`LIMIT` semantics including ties. Amounts resolve
/// through the row path's resolver before construction, so parameter
/// diagnostics agree exactly. Output schema is the child's schema.
pub(crate) struct BatchTopK<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
    keys: &'plan [OrderKey],
    offset: u64,
    count: u64,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
    schema: BindingTableSchema,
    rows: Vec<Binding>,
    reserved_bytes: usize,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl<'x, 'a, 'ctx, 'g, 'plan> BatchTopK<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct a top-K over `child` retaining the resolved window.
    pub(crate) fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        keys: &'plan [OrderKey],
        offset: u64,
        count: u64,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        let schema = child.output_schema().clone();
        Self {
            child,
            keys,
            offset,
            count,
            eval,
            policy,
            schema,
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
                detail: "batch top-k init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.child.init(ctx)?;
        let input_schema = self.child.output_schema().clone();
        let input_rows = materialize_child(&mut self.child, ctx)?;
        let (rows, reserved) = top_k_rows(
            self.keys,
            self.offset,
            self.count,
            &input_schema,
            input_rows,
            &self.eval,
            ctx,
        )?;
        self.rows = rows;
        self.reserved_bytes = reserved;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
    }
}

impl PhysicalOperator for BatchTopK<'_, '_, '_, '_, '_> {
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
                detail: "batch top-k pull is legal only while Open",
            });
        }
        let outcome = slice_rows(
            &self.schema,
            &self.rows,
            &mut self.cursor,
            self.policy,
            ctx,
            buffer,
            "batch top-k built a malformed batch",
        );
        match &outcome {
            Ok(None) => self.state = OperatorState::Exhausted,
            Ok(Some(_)) => self.batches_produced += 1,
            Err(_) => self.state = OperatorState::Failed,
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
        &self.schema
    }
}

/// Resolve a fused top-K window through the row path's amount resolver.
///
/// Split-time resolution (like [`BatchPage`](super::page::BatchPage)) so
/// parameter diagnostics agree exactly; see the driver for the one ordering
/// difference versus the row path.
///
/// # Errors
///
/// Returns the row path's null, negative, mistyped, out-of-range, and
/// unbound parameter diagnostics.
pub(crate) fn resolve_top_k_window(
    offset: &LimitAmount,
    count: &LimitAmount,
    ctx: &crate::runtime::TxContext<'_, '_>,
) -> Result<(u64, u64), ExecutorError> {
    Ok((resolve_amount(offset, ctx)?, resolve_amount(count, ctx)?))
}

/// Pull-based order-carrier trim over a child operator's batches.
///
/// Carriers are always appended after the projected columns, so trimming is
/// a positional truncation to `projected_width`. A width already at or
/// below the output width means the planner added no carriers, and
/// truncating is then a no-op rather than an error — the same pairing rule
/// as the row path. Output schema is the child's schema truncated the same
/// way, so empty results keep the projected descriptor.
pub(crate) struct BatchTrimCarriers<'x> {
    child: Box<dyn PhysicalOperator + 'x>,
    output_schema: BindingTableSchema,
    state: OperatorState,
}

impl<'x> BatchTrimCarriers<'x> {
    /// Construct a carrier trim over `child` keeping `projected_width`
    /// leading columns.
    pub(crate) fn new(child: Box<dyn PhysicalOperator + 'x>, projected_width: usize) -> Self {
        let mut output_schema = child.output_schema().clone();
        output_schema.columns.truncate(projected_width);
        Self {
            child,
            output_schema,
            state: OperatorState::Created,
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
                detail: "batch trim init is legal only once from Created",
            });
        }
        self.child.init(ctx)?;
        self.state = OperatorState::Open;
        Ok(())
    }

    fn pull_inner(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        ctx.ensure_generation()?;
        let span = crate::SourceSpan::default();
        ctx.check_cancel(span)?;
        let Some(child) = self.child.next_batch(ctx, buffer)? else {
            self.state = OperatorState::Exhausted;
            return Ok(None);
        };
        let width = self.output_schema.columns.len();
        let mut columns: Vec<(Vec<Value>, Vec<bool>)> = Vec::with_capacity(width);
        for _ in 0..width {
            let mut values = buffer.take_values();
            let mut nulls = buffer.take_nulls();
            values.clear();
            nulls.clear();
            columns.push((values, nulls));
        }
        for index in 0..child.logical_rows() {
            let row = child.logical_row(index);
            for (slot, (column, nulls)) in columns.iter_mut().enumerate() {
                let value = row.get(slot).cloned().unwrap_or(Value::Null);
                nulls.push(value == Value::Null);
                column.push(value);
            }
        }
        let rows = child.logical_rows();
        ctx.budget_mut().release(child.estimated_bytes());
        child.recycle(buffer);
        // Zero-width projections still validate: the output schema must
        // agree with the truncation, so a mismatch fails here rather than
        // producing a silently misdescribed batch.
        let batch_columns = columns
            .into_iter()
            .map(|(values, nulls)| {
                BatchColumn::from_parts(values, nulls).map_err(|_| {
                    ExecutorError::ImplementationDefined {
                        detail: "batch trim built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch = BindingBatch::from_batch_columns(self.output_schema.clone(), batch_columns)
            .map_err(|_| ExecutorError::ImplementationDefined {
                detail: "batch trim output disagrees with its schema",
            })?;
        ctx.finish_batch(rows);
        ctx.budget_mut()
            .reserve(batch.estimated_bytes())
            .map_err(|err| err.into_executor_error(span))?;
        Ok(Some(batch))
    }
}

impl PhysicalOperator for BatchTrimCarriers<'_> {
    fn init(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        let outcome = self.init_inner(ctx);
        if outcome.is_err() {
            self.state = OperatorState::Failed;
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
                detail: "batch trim pull is legal only while Open",
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
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.output_schema
    }
}
