//! Batch deduplication (F04-PR04).
//!
//! [`BatchDistinct`] carries one [`PipelineOp::Distinct`](crate::PipelineOp::Distinct)
//! into batches. The child runs to completion through the pull protocol,
//! then first-occurrence deduplication runs over the materialized rows and
//! pulls serve the output in policy-sized slices, mirroring
//! [`BatchHashJoin`](super::join)'s materialize-then-slice shape.
//!
//! Deduplication semantics reuse the row path's semantic services by
//! construction: cross-value compatibility is enforced by the shared
//! [`ComparisonDomain`] in `Distinctness` mode over the same row order, and
//! row identity uses [`RuntimeEqKey`] so internal hash keys agree with the
//! language equality relation (cross-type numerics collapse, records
//! compare by field name, lists element-wise — never serialized values or
//! debug strings). First-seen rows are retained in input order, exactly as
//! the row path retains them.
//!
//! Memory is accounted before allocation: the key set reserves an
//! input-proportional upper bound up front, the exact output is reserved
//! before any output row materializes, and a bounded budget fails with the
//! typed resource error (`5GQL1`) instead of truncating. The operator holds
//! its output reservation until `close`.

use rustc_hash::FxHashSet;

use crate::{
    plan::BindingTableSchema,
    runtime::{
        Binding, ExecutorError, comparison_domain::ComparisonDomain, value_key::RuntimeEqKey,
    },
};

use super::{
    binding_batch::{BatchBuffer, BindingBatch},
    join::reserve_rows,
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
    sort::slice_rows,
};

/// Deduplicate materialized rows keeping first occurrences in pull order.
///
/// Replicates the row distinct sequence: per-row `Distinctness` domain
/// observation with first-seen retention keyed by [`RuntimeEqKey`].
/// Returns the deduplicated rows plus the output reservation the caller
/// holds until the rows drop; transient key storage is released before
/// returning.
///
/// # Errors
///
/// Returns distinctness data exceptions, cancellation, or the
/// memory-budget resource error. No partial rows are returned on failure.
pub(crate) fn distinct_rows(
    input_rows: Vec<Binding>,
    width: usize,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(Vec<Binding>, usize), ExecutorError> {
    // Upper bound before any key state exists: keys never outnumber input
    // rows, and one row-width entry covers one key order.
    let build_reserved = reserve_rows(
        ctx,
        input_rows.len(),
        width,
        "batch distinct key set exceeds the supported range",
    )?;
    let deduped = dedup_rows(input_rows, ctx);
    ctx.budget_mut().release(build_reserved);
    let output = deduped?;
    let reserved = reserve_rows(
        ctx,
        output.len(),
        width,
        "batch distinct fanout exceeds the supported range",
    )?;
    Ok((output, reserved))
}

/// Run the first-seen retention pass over materialized rows.
fn dedup_rows(
    input_rows: Vec<Binding>,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<Vec<Binding>, ExecutorError> {
    use selene_core::ComparisonMode;
    let span = crate::SourceSpan::default();
    let mut seen: FxHashSet<RuntimeEqKey> = FxHashSet::default();
    let mut domains = ComparisonDomain::default();
    let mut output = Vec::new();
    let mut since_check = 0usize;
    for row in input_rows {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        domains.observe(row.values(), ComparisonMode::Distinctness)?;
        if seen.insert(RuntimeEqKey::from_row(row.values().to_vec())) {
            output.push(row);
        }
    }
    // The output reservation transfers to the caller; the transient key
    // set drops here, peak-recorded.
    Ok(output)
}

/// Pull-based deduplication over a child operator's batches.
///
/// The child materializes fully at `init` (dedup needs every row before
/// the first output, as on the row path); pulls then slice the retained
/// rows. Output schema is the child's schema.
pub(crate) struct BatchDistinct<'x> {
    child: Box<dyn PhysicalOperator + 'x>,
    policy: BatchPolicy,
    schema: BindingTableSchema,
    rows: Vec<Binding>,
    reserved_bytes: usize,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl<'x> BatchDistinct<'x> {
    /// Construct a deduplication over `child`.
    pub(crate) fn new(child: Box<dyn PhysicalOperator + 'x>, policy: BatchPolicy) -> Self {
        let schema = child.output_schema().clone();
        Self {
            child,
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
                detail: "batch distinct init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.child.init(ctx)?;
        let width = self.child.output_schema().columns.len();
        let input_rows = materialize_child(&mut self.child, ctx)?;
        let (rows, reserved) = distinct_rows(input_rows, width, ctx)?;
        self.rows = rows;
        self.reserved_bytes = reserved;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
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

impl PhysicalOperator for BatchDistinct<'_> {
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
                detail: "batch distinct pull is legal only while Open",
            });
        }
        let outcome = slice_rows(
            &self.schema,
            &self.rows,
            &mut self.cursor,
            self.policy,
            ctx,
            buffer,
            "batch distinct built a malformed batch",
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
