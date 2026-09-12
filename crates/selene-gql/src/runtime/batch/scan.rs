//! Batch scan: node and edge access lowered into batches.
//!
//! [`BatchScan`] covers every [`ScanAccess`](crate::ScanAccess) shape the row
//! executor supports (linear enumeration, label bitmaps, typed ranges, bitmap
//! unions, composite lookups) over both node and edge scans. Candidates
//! resolve exactly once in [`PhysicalOperator::init`] through the shared
//! candidate entry the row scan uses, against the context's pinned snapshot,
//! with the snapshot binding captured alongside the stable identities; pulls
//! then filter and slice the captured identities into batches. Label
//! pre-checks, residual property predicates, and seed-slot unification run
//! through the same row helpers (`label_matches_scan`, `predicate_passes`,
//! `binding_for_scan`), so indexed and linear paths agree with the row
//! executor by construction.
//!
//! This operator never evaluates filter expressions itself: predicate checks
//! go through the shared predicate helpers with identical three-valued
//! semantics (only `True` retained). Each output value is a stable graph
//! identity (`Value::NodeRef`/`Value::EdgeRef`); batch positions never appear
//! in output.

use std::mem::size_of;

use selene_core::Value;

use crate::{
    NodeOrEdgeScan, PatternPlan, ScanAccess, SourceSpan,
    plan::BindingTableSchema,
    runtime::{Binding, EvalCtx, ExecutorError},
};

use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    candidates::{ResolvedCandidates, entity_value},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};

use super::super::scan::{label_matches_scan, predicates_pass, single_label};
use super::super::scan_bind::{ScanSlots, binding_for_scan};
use super::super::scan_seed::try_seeded_scan;

/// Pull-based scan producing pattern-schema batches for one scan.
///
/// The operator captures stable entity identities at `init` and serves them
/// in policy-sized slices. It holds no snapshot of its own: every pull goes
/// through the execution context, which enforces the pinned generation and
/// cancellation checkpoints. Output rows are full pattern-schema rows with
/// the scanned binding materialized and every other column null, exactly as
/// the row scan builds them.
///
/// An optional seed row (correlated execution: `OPTIONAL MATCH` right sides,
/// non-leading `MATCH`, per-row `NEXT` blocks) threads through the same row
/// helpers the seeded row scan uses. A seed that already binds the scanned
/// variable short-circuits to that single entity without resolving or
/// charging candidates; otherwise candidates resolve once and each entity
/// unifies against the seed, so correlated bindings can never leak across
/// input rows.
pub(crate) struct BatchScan<'a, 'ctx, 'g, 'plan> {
    scan: &'plan NodeOrEdgeScan,
    pattern: &'plan PatternPlan,
    schema: BindingTableSchema,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
    seed: Option<Binding>,
    slots: Option<ScanSlots>,
    label_prechecked: bool,
    candidates: Option<ResolvedCandidates>,
    seeded_rows: Option<Vec<Binding>>,
    seeded_cursor: usize,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl<'a, 'ctx, 'g, 'plan> BatchScan<'a, 'ctx, 'g, 'plan> {
    /// Construct a scan over `scan` with `policy` sizing.
    ///
    /// `schema` is the pattern schema the scan materializes into; `pattern`
    /// supplies binding identities for slot resolution and predicate checks.
    pub(crate) const fn new(
        scan: &'plan NodeOrEdgeScan,
        pattern: &'plan PatternPlan,
        schema: BindingTableSchema,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        Self {
            scan,
            pattern,
            schema,
            eval,
            policy,
            seed: None,
            slots: None,
            label_prechecked: false,
            candidates: None,
            seeded_rows: None,
            seeded_cursor: 0,
            cursor: 0,
            state: OperatorState::Created,
            batches_produced: 0,
        }
    }

    /// Attach a correlated seed row evaluated against this scan.
    ///
    /// The seed carries outer bindings in this operator's output-schema
    /// coordinates (shared columns keep their indexes because pattern target
    /// schemas always append new columns). Each produced row starts from the
    /// seed with the scanned binding unified, exactly as the row scan's
    /// seed path builds them.
    #[must_use]
    pub(crate) fn with_seed(mut self, seed: Binding) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Return the number of batches produced so far.
    ///
    /// Test seam for pull-count assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn batches_produced(&self) -> u64 {
        self.batches_produced
    }

    /// Return the total captured candidate count.
    ///
    /// Test seam for candidate assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn candidate_count(&self) -> usize {
        self.candidates.as_ref().map_or(0, ResolvedCandidates::len)
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
                detail: "batch scan init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(SourceSpan::default())?;
        let span = SourceSpan::default();
        let slots = ScanSlots::resolve(self.scan, self.pattern, &self.schema)?;
        // Seeded short-circuit first, exactly as the row scan orders it: a
        // seed that already binds this scan's variable resolves to at most
        // one row with no candidate resolution and no scan-budget charge.
        if let Some(seed) = self.seed.as_ref()
            && let Some(rows) = try_seeded_scan(
                self.scan,
                self.pattern,
                &self.schema,
                seed,
                slots,
                &self.eval,
            )?
        {
            self.slots = Some(slots);
            self.seeded_rows = Some(rows);
            self.seeded_cursor = 0;
            self.cursor = 0;
            self.state = OperatorState::Open;
            return Ok(());
        }
        let resolved = ResolvedCandidates::resolve(self.scan, &self.eval)?;
        // The snapshot binding travels with the candidates: a resolution from
        // any other graph or generation fails here instead of rebinding.
        resolved.binding().validate(ctx.snapshot()?)?;
        self.label_prechecked = label_matched_by_access(self.scan);
        self.slots = Some(slots);
        ctx.note_nodes_scanned(resolved.len(), span)?;
        self.cursor = 0;
        self.candidates = Some(resolved);
        self.state = OperatorState::Open;
        Ok(())
    }

    fn pull_inner(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        ctx.ensure_generation()?;
        if self.seeded_rows.is_some() {
            return self.pull_seeded(ctx, buffer);
        }
        // Windows cover candidates, not output rows: residual predicates may
        // drop entities, so a window can yield an empty batch that the loop
        // skips. Progress is guaranteed because every window advances the
        // cursor by at least one candidate.
        loop {
            let total = self.candidates.as_ref().map_or(0, ResolvedCandidates::len);
            if self.cursor >= total {
                self.state = OperatorState::Exhausted;
                return Ok(None);
            }
            let span = SourceSpan::default();
            ctx.check_cancel(span)?;
            let width = self.schema.columns.len();
            let rows = self
                .policy
                .rows_per_batch(
                    width
                        .saturating_mul(size_of::<Value>().saturating_add(1))
                        .max(1),
                )
                .min(total - self.cursor);
            let batch = self.materialize_window(ctx, buffer, rows)?;
            if batch.logical_rows() > 0 {
                return Ok(Some(batch));
            }
            ctx.budget_mut().release(batch.estimated_bytes());
            batch.recycle(buffer);
        }
    }

    /// Serve one policy-sized batch from short-circuited seeded rows.
    ///
    /// Fast-path rows are fully formed bindings in output-schema order;
    /// serving only slices them into batches with the usual budget claim.
    fn pull_seeded(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        let span = SourceSpan::default();
        ctx.check_cancel(span)?;
        let rows = self
            .seeded_rows
            .as_ref()
            .expect("seeded rows resolved at init");
        if self.seeded_cursor >= rows.len() {
            self.state = OperatorState::Exhausted;
            return Ok(None);
        }
        let width = self.schema.columns.len();
        let take = self
            .policy
            .rows_per_batch(
                width
                    .saturating_mul(size_of::<Value>().saturating_add(1))
                    .max(1),
            )
            .min(rows.len() - self.seeded_cursor);
        let mut columns: Vec<(Vec<Value>, Vec<bool>)> = Vec::with_capacity(width);
        for _ in 0..width {
            let mut values = buffer.take_values();
            let mut nulls = buffer.take_nulls();
            values.clear();
            nulls.clear();
            columns.push((values, nulls));
        }
        for row in &rows[self.seeded_cursor..self.seeded_cursor + take] {
            for (slot, (column, nulls)) in columns.iter_mut().enumerate() {
                let value = row.get(slot).cloned().unwrap_or(Value::Null);
                nulls.push(value == Value::Null);
                column.push(value);
            }
        }
        self.seeded_cursor += take;
        self.batches_produced += 1;
        ctx.finish_batch(take);
        let batch_columns = columns
            .into_iter()
            .map(|(values, nulls)| {
                BatchColumn::from_parts(values, nulls).map_err(|_| {
                    ExecutorError::ImplementationDefined {
                        detail: "batch scan built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch =
            BindingBatch::from_batch_columns(self.schema.clone(), batch_columns).map_err(|_| {
                ExecutorError::ImplementationDefined {
                    detail: "batch scan built a malformed batch",
                }
            })?;
        ctx.budget_mut()
            .reserve(batch.estimated_bytes())
            .map_err(|err| err.into_executor_error(span))?;
        Ok(Some(batch))
    }

    fn materialize_window(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
        rows: usize,
    ) -> Result<BindingBatch, ExecutorError> {
        let span = SourceSpan::default();
        let width = self.schema.columns.len();
        let slots = self.slots.expect("scan slots resolved at init");
        let start = self.cursor;
        let end = start + rows;
        self.cursor = end;
        let mut columns: Vec<(Vec<Value>, Vec<bool>)> = Vec::with_capacity(width);
        for _ in 0..width {
            let mut values = buffer.take_values();
            let mut nulls = buffer.take_nulls();
            values.clear();
            nulls.clear();
            columns.push((values, nulls));
        }
        let candidates = self
            .candidates
            .as_ref()
            .expect("candidates resolved at init");
        let mut kept = 0usize;
        for index in start..end {
            let entity = candidates.entities()[index];
            if !self.label_prechecked && !label_matches_scan(self.scan, entity, &self.eval) {
                continue;
            }
            let value = entity_value(entity);
            let Some(binding) =
                binding_for_scan(&self.schema, self.seed.as_ref(), value.clone(), slots)
            else {
                continue;
            };
            if !predicates_pass(
                self.scan,
                self.pattern,
                &binding,
                &self.schema,
                &value,
                &self.eval,
            )? {
                continue;
            }
            let values = binding.values();
            debug_assert_eq!(values.len(), width);
            for (slot, (column, nulls)) in columns.iter_mut().enumerate() {
                let value = values.get(slot).cloned().unwrap_or(Value::Null);
                nulls.push(value == Value::Null);
                column.push(value);
            }
            kept += 1;
        }
        self.batches_produced += 1;
        ctx.finish_batch(kept);
        let batch_columns = columns
            .into_iter()
            .map(|(values, nulls)| {
                BatchColumn::from_parts(values, nulls).map_err(|_| {
                    ExecutorError::ImplementationDefined {
                        detail: "batch scan built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch =
            BindingBatch::from_batch_columns(self.schema.clone(), batch_columns).map_err(|_| {
                ExecutorError::ImplementationDefined {
                    detail: "batch scan built a malformed batch",
                }
            })?;
        // Account for the batch this pull keeps alive. The driver releases
        // the reservation when it recycles the batch, so the budget bounds
        // live plus retained storage instead of accumulating closed history.
        // A failed reservation drops the batch (freeing its fresh growth)
        // and reports without producing rows.
        ctx.budget_mut()
            .reserve(batch.estimated_bytes())
            .map_err(|err| err.into_executor_error(span))?;
        Ok(batch)
    }
}

impl PhysicalOperator for BatchScan<'_, '_, '_, '_> {
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
        // Pulls after exhaustion report end of input: parents pull
        // speculatively (a page or filter may not know the child ended
        // until it asks once more), so only Created/Failed/Closed pulls
        // are contract violations.
        if self.state == OperatorState::Exhausted {
            return Ok(None);
        }
        if self.state != OperatorState::Open {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch scan pull is legal only while Open",
            });
        }
        let outcome = self.pull_inner(ctx, buffer);
        if outcome.is_err() {
            self.state = OperatorState::Failed;
        }
        outcome
    }

    fn close(&mut self, ctx: &mut BatchExecutionContext<'_>) {
        self.candidates = None;
        self.seeded_rows = None;
        self.seeded_cursor = 0;
        self.seed = None;
        self.slots = None;
        self.cursor = 0;
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.schema
    }
}

/// Whether the row scan's access path already proved the label predicate.
///
/// Mirrors the row scan: only a label-index access over a single label skips
/// the per-entity label recheck.
fn label_matched_by_access(scan: &NodeOrEdgeScan) -> bool {
    matches!(scan.access, ScanAccess::LabelIndex { .. })
        && single_label(&scan.label_predicate).is_some()
}
