//! Batch grouping and aggregation (F04-PR04).
//!
//! [`BatchGroupBy`] carries one [`PipelineOp::GroupBy`](crate::PipelineOp::GroupBy)
//! into batches. The child runs to completion through the pull protocol,
//! then the grouping combinatorics run over the materialized rows and pulls
//! serve the output in policy-sized slices, mirroring
//! [`BatchHashJoin`](super::join)'s materialize-then-slice shape.
//!
//! Grouping semantics reuse the row path's semantic services by
//! construction: keys evaluate through the shared
//! [`evaluate`](crate::runtime::evaluator::evaluate) entry, group identity
//! uses [`RuntimeEqKey`] (the language grouping-equivalence relation —
//! nulls-together, cross-type numerics collapsed, records by field name —
//! never ordinary comparison predicates or serialized values),
//! cross-value compatibility is enforced by the shared [`ComparisonDomain`]
//! in `Distinctness` mode, and every aggregate application runs as a shared
//! [`AggregateSlot`] built from the same analyzed descriptor, so state
//! transitions, `COUNT(*)` versus `COUNT(expr)` treatment, null
//! elimination, `DISTINCT` handling, finalization, and type promotion agree
//! with the row path by construction. The grouping loop itself (hash index,
//! insertion-ordered emission, the specified empty-ungrouped-group rule, and
//! the production group cap) replicates the row `group_by` sequence.
//!
//! Memory is accounted before allocation: hash-state construction reserves
//! an input-proportional upper bound up front, the exact output fanout is
//! reserved before any output row materializes, and a bounded budget fails
//! with the typed resource error (`5GQL1`) instead of truncating. The
//! operator holds its output reservation until `close`.

use rustc_hash::FxHashMap;
use selene_core::Value;
use smallvec::SmallVec;

use crate::{
    Aggregate, ProjectExpr,
    plan::BindingTableSchema,
    runtime::{
        Binding, EvalCtx, ExecutorError,
        comparison_domain::ComparisonDomain,
        evaluator,
        pipeline::{
            aggregate::AggregateSlot,
            group_by::{group_by_key_cap_exceeded, output_schema},
        },
        value_key::RuntimeEqKey,
    },
};

use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    join::{reserve_rows, row_bytes_estimate},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};

/// Group materialized rows and finalize one output row per group.
///
/// `input_rows` are the child's rows in pull order; `input_schema` describes
/// them. Returns the finalized rows (first-emission group order, exactly as
/// the row path emits) plus the output reservation the caller holds until
/// the rows drop; transient hash state is released before returning.
///
/// # Errors
///
/// Returns evaluation and aggregate data exceptions, the group-cap resource
/// error, cancellation, or the memory-budget resource error. No partial
/// rows are returned on failure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn group_rows(
    keys: &[ProjectExpr],
    aggregates: &[Aggregate],
    input_schema: &BindingTableSchema,
    input_rows: Vec<Binding>,
    eval: &EvalCtx<'_, '_, '_, '_>,
    group_cap: usize,
    output_width: usize,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(Vec<Binding>, usize), ExecutorError> {
    // Upper bound before any group state exists: groups never outnumber
    // input rows, and one output-width row covers one group's state order.
    let build_reserved = reserve_rows(
        ctx,
        input_rows.len(),
        output_width,
        "batch group hash state exceeds the supported range",
    )?;
    let input_width = input_schema.columns.len();
    let grouped = accumulate_groups(
        keys,
        aggregates,
        input_schema,
        input_rows,
        eval,
        group_cap,
        ctx,
    );
    ctx.budget_mut().release(build_reserved);
    let mut groups = grouped?;
    // The specified empty-ungrouped rule: an ungrouped aggregation over an
    // empty input still yields one group, so `COUNT(*)` reports zero rather
    // than vanishing. A grouped aggregation over an empty input yields no
    // groups (and therefore no rows), and an all-null key tuple forms one
    // ordinary group because nulls-together is grouping equivalence.
    if keys.is_empty() && groups.is_empty() {
        groups.push(GroupAccum::empty_ungrouped(input_width, aggregates)?);
    }
    let reserved = reserve_rows(
        ctx,
        groups.len(),
        output_width,
        "batch group fanout exceeds the supported range",
    )?;
    let rows = finalize_groups(groups, ctx).inspect_err(|_| {
        ctx.budget_mut().release(reserved);
    })?;
    Ok((rows, reserved))
}

/// Accumulate input rows into insertion-ordered groups with a hash index.
///
/// Replicates the row grouping sequence: per-row key evaluation,
/// `Distinctness` domain observation, [`RuntimeEqKey`] lookup, the
/// production group cap, and per-group aggregate observation.
fn accumulate_groups<'plan>(
    keys: &'plan [ProjectExpr],
    aggregates: &'plan [Aggregate],
    input_schema: &BindingTableSchema,
    input_rows: Vec<Binding>,
    eval: &EvalCtx<'_, '_, '_, '_>,
    group_cap: usize,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<Vec<GroupAccum<'plan>>, ExecutorError> {
    use selene_core::ComparisonMode;
    let span = crate::SourceSpan::default();
    let mut groups = Vec::<GroupAccum<'_>>::new();
    let mut group_index = FxHashMap::<RuntimeEqKey, usize>::default();
    let mut domains = ComparisonDomain::default();
    let mut since_check = 0usize;
    for row in &input_rows {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        let key = evaluate_key_tuple(keys, row, input_schema, eval)?;
        domains.observe(&key, ComparisonMode::Distinctness)?;
        let probe = RuntimeEqKey::from_row(key);
        let index = match group_index.get(&probe) {
            Some(index) => *index,
            None => {
                if groups.len() >= group_cap {
                    return Err(group_by_key_cap_exceeded());
                }
                let index = groups.len();
                groups.push(GroupAccum::new(row.clone(), aggregates)?);
                group_index.insert(probe, index);
                index
            }
        };
        groups[index].observe(row, input_schema, eval)?;
    }
    Ok(groups)
}

/// Finalize every group into its output row, in first-emission order.
///
/// Cancellation is checked on the same stride cadence as accumulation, so a
/// cancelled finalization fails without exposing a truncated group set.
fn finalize_groups(
    groups: Vec<GroupAccum<'_>>,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<Vec<Binding>, ExecutorError> {
    let span = crate::SourceSpan::default();
    let mut rows = Vec::with_capacity(groups.len());
    let mut since_check = 0usize;
    for group in groups {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        rows.push(group.finalize()?);
    }
    Ok(rows)
}

/// One open group: its first row's values plus one shared slot per aggregate.
struct GroupAccum<'plan> {
    representative: Binding,
    slots: Vec<AggregateSlot<'plan>>,
}

impl<'plan> GroupAccum<'plan> {
    /// Open a group over `representative`'s values with empty aggregate states.
    fn new(representative: Binding, aggregates: &'plan [Aggregate]) -> Result<Self, ExecutorError> {
        let slots = aggregates
            .iter()
            .map(AggregateSlot::new)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            representative,
            slots,
        })
    }

    /// Open the specified empty-ungrouped group: one all-null representative
    /// row over the input width with empty aggregate states, so
    /// finalization yields the specified empty-input results (`COUNT(*)`
    /// zero, `SUM` zero, nullable aggregates null), exactly as the row
    /// path's null representative does.
    fn empty_ungrouped(
        input_width: usize,
        aggregates: &'plan [Aggregate],
    ) -> Result<Self, ExecutorError> {
        let slots = aggregates
            .iter()
            .map(AggregateSlot::new)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            representative: Binding::new(vec![Value::Null; input_width]),
            slots,
        })
    }

    /// Observe one member row through every aggregate slot.
    fn observe(
        &mut self,
        row: &Binding,
        schema: &BindingTableSchema,
        eval: &EvalCtx<'_, '_, '_, '_>,
    ) -> Result<(), ExecutorError> {
        for slot in &mut self.slots {
            slot.observe(row, schema, eval)?;
        }
        Ok(())
    }

    /// Finalize into the output row: representative values plus one value
    /// per aggregate, in discovery order.
    fn finalize(self) -> Result<Binding, ExecutorError> {
        let mut values = self.representative.cloned_values();
        for slot in self.slots {
            values.push(slot.finalize_value()?);
        }
        Ok(Binding::from_parts(values, SmallVec::new()))
    }
}

/// Evaluate one row's grouping-key tuple in key order through the shared
/// evaluator, so computed keys and their errors agree with the row path.
fn evaluate_key_tuple(
    keys: &[ProjectExpr],
    row: &Binding,
    schema: &BindingTableSchema,
    eval: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Vec<Value>, ExecutorError> {
    keys.iter()
        .map(|key| evaluator::evaluate(&key.expr, row, schema, eval))
        .collect()
}

/// Pull-based grouping and aggregation over a child operator's batches.
///
/// The child materializes fully at `init` (every member must be observed
/// before the first group finalizes, as on the row path); pulls then slice
/// the finalized rows. Output schema is the row path's grouping schema for
/// the same keys and aggregates (input columns plus synthesized aggregate
/// columns), so empty results keep full descriptors.
pub(crate) struct BatchGroupBy<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
    keys: &'plan [ProjectExpr],
    aggregates: &'plan [Aggregate],
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
    output_schema: BindingTableSchema,
    rows: Vec<Binding>,
    reserved_bytes: usize,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl<'x, 'a, 'ctx, 'g, 'plan> BatchGroupBy<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct a grouping over `child` with `keys` and `aggregates`.
    ///
    /// The output schema is derived up front from the child's schema, so it
    /// is available before any pull (empty results keep it).
    pub(crate) fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        keys: &'plan [ProjectExpr],
        aggregates: &'plan [Aggregate],
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        let output_schema = output_schema(child.output_schema(), aggregates);
        Self {
            child,
            keys,
            aggregates,
            eval,
            policy,
            output_schema,
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

    /// Return the synthesized aggregate output names in discovery order.
    ///
    /// Test seam proving descriptor derivation matches the row path.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn aggregate_names(&self) -> Vec<selene_core::DbString> {
        use crate::runtime::pipeline::aggregate::output_names;

        self.aggregates.iter().flat_map(output_names).collect()
    }

    fn init_inner(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch group init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.child.init(ctx)?;
        let input_schema = self.child.output_schema().clone();
        let input_rows = materialize_child(&mut self.child, ctx)?;
        let output_width = self.output_schema.columns.len();
        let group_cap = self.eval.tx.impl_defined_caps().group_by_key_cap();
        let (rows, reserved) = group_rows(
            self.keys,
            self.aggregates,
            &input_schema,
            input_rows,
            &self.eval,
            group_cap,
            output_width,
            ctx,
        )?;
        self.rows = rows;
        self.reserved_bytes = reserved;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
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
        let width = self.output_schema.columns.len();
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
                        detail: "batch group built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch = BindingBatch::from_batch_columns(self.output_schema.clone(), batch_columns)
            .map_err(|_| ExecutorError::ImplementationDefined {
                detail: "batch group built a malformed batch",
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

impl PhysicalOperator for BatchGroupBy<'_, '_, '_, '_, '_> {
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
                detail: "batch group pull is legal only while Open",
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
        &self.output_schema
    }
}
