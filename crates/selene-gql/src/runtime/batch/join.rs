//! Batch inner hash join with a nested-loop path for small inputs.
//!
//! [`BatchHashJoin`] carries one `JoinTree::HashJoin` (comma-separated graph
//! patterns and successive leading `MATCH` clauses sharing bindings) into
//! batches. Both children run to completion through the pull protocol, then
//! the join combinatorics run over the materialized rows and pulls serve the
//! output in policy-sized slices, mirroring [`BatchExpand`](super::expand)'s
//! materialize-then-slice shape.
//!
//! Join semantics reuse the row path's semantic service by construction:
//! keys resolve by binding name through [`pattern::resolve_key`](super::super::pattern::resolve_key),
//! null keys never match
//! ([`key_values_at`](super::super::pattern::key_values_at) returns `None`),
//! cross-input comparability is enforced by the shared [`JoinDomain`], the
//! probe self-check runs the shared
//! [`key_values_equal`](super::super::pattern::key_values_equal), and bucketing
//! uses [`RuntimeEqKey`] so internal hash keys agree with the language
//! equality relation (cross-type numerics collapse, records compare by field
//! name, lists element-wise). No serialized values or debug strings are ever
//! compared. Many-to-many multiplicity is preserved and output order is
//! probe-major with build insertion order inside each bucket, exactly as the
//! row hash join emits.
//!
//! Two execution paths share that contract: a hash path for general inputs
//! and a simple nested-loop path for empty keys (degenerate single-bucket
//! cross products) and tiny inputs where hashing buys nothing. Both paths run
//! the identical domain/key/merge sequence, so path selection never changes
//! rows, order, or errors; kernel tests assert path parity directly.
//!
//! Memory is accounted before allocation: the build side reserves its
//! estimate up front, the output fanout is counted with checked arithmetic
//! and reserved before any output row materializes, and a bounded budget
//! fails with the typed resource error (`5GQL1`) instead of truncating. The
//! operator holds its output reservation until `close`.

use std::mem::size_of;

use rustc_hash::FxHashMap;
use selene_core::{DbString, Value};

use crate::{
    BuildSide,
    plan::BindingTableSchema,
    runtime::{Binding, ExecutorError},
};

use super::super::{join_domain::JoinDomain, pattern, value_key::RuntimeEqKey};
use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};

/// Pairwise-comparison ceiling for the nested-loop path.
///
/// At or below this many build-by-probe pairs the linear scan avoids hash
/// allocation with identical rows, order, and errors (asserted by kernel
/// tests); above it the hash path serves probes in O(1) amortized lookups.
/// An observed low-overhead default, not a release promise.
const NESTED_LOOP_PAIR_CEILING: usize = 1_024;

/// Estimated resident bytes for one binding-table row of `width` columns.
///
/// Same convention as the scan sizing estimate: one engine value plus one
/// null bit per column. Estimates bound operator behavior; they do not
/// measure the allocator.
#[must_use]
pub(crate) fn row_bytes_estimate(width: usize) -> usize {
    width.saturating_mul(size_of::<Value>().saturating_add(1))
}

/// Reserve budget for `rows` materialized rows of `width` columns.
///
/// The fanout product uses checked arithmetic: an overflowing amplification
/// fails here with the typed resource error rather than wrapping into a
/// smaller reservation. Callers hold the returned byte count and release it
/// when the materialized rows drop.
///
/// # Errors
///
/// Returns `ProgramLimitExceeded` (`5GQL1`) when the product overflows or
/// the budget cap would be exceeded.
pub(crate) fn reserve_rows(
    ctx: &mut BatchExecutionContext<'_>,
    rows: usize,
    width: usize,
    detail: &'static str,
) -> Result<usize, ExecutorError> {
    let bytes =
        rows.checked_mul(row_bytes_estimate(width))
            .ok_or(ExecutorError::ProgramLimitExceeded {
                detail,
                span: crate::SourceSpan::default(),
            })?;
    ctx.budget_mut()
        .reserve(bytes)
        .map_err(|err| err.into_executor_error(crate::SourceSpan::default()))?;
    Ok(bytes)
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

/// True when the nested-loop path is the correct low-overhead choice.
///
/// Empty keys degenerate the hash table to one bucket (a cross product), and
/// tiny pair counts never amortize hash allocation. Either case runs the
/// linear path with identical observable behavior.
fn use_nested_loop(build_len: usize, probe_len: usize, key_len: usize) -> bool {
    if key_len == 0 {
        return true;
    }
    build_len
        .checked_mul(probe_len)
        .is_some_and(|pairs| pairs <= NESTED_LOOP_PAIR_CEILING)
}

/// Inner hash join over materialized rows: the general path.
///
/// Replicates the row `execute_ordered` sequence exactly: build keys observe
/// the shared [`JoinDomain`] and fill insertion-ordered buckets (null keys
/// skipped), then each probe key in walk order passes the domain comparison
/// and the probe self-check before emitting every bucket row probe-major.
/// Output fanout is counted with checked arithmetic and reserved before any
/// output row materializes; the caller holds the returned reservation.
///
/// # Errors
///
/// Returns domain data exceptions, cancellation, or the typed resource error
/// for overflowing fanout or an exhausted budget.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hash_join_rows(
    build_rows: &[Binding],
    probe_rows: &[Binding],
    key_indexes: &[usize],
    build_is_left: bool,
    schema: &BindingTableSchema,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(Vec<Vec<Value>>, usize), ExecutorError> {
    let width = schema.columns.len();
    let build_reserved = reserve_rows(
        ctx,
        build_rows.len(),
        width,
        "batch join build exceeds the supported range",
    )?;
    let joined = hash_join_inner(
        build_rows,
        probe_rows,
        key_indexes,
        build_is_left,
        schema,
        ctx,
    );
    ctx.budget_mut().release(build_reserved);
    let (pairs, output) = joined?;
    let output_reserved = reserve_rows(
        ctx,
        pairs,
        width,
        "batch join fanout exceeds the supported range",
    )?;
    Ok((output, output_reserved))
}

#[allow(clippy::too_many_arguments)]
fn hash_join_inner(
    build_rows: &[Binding],
    probe_rows: &[Binding],
    key_indexes: &[usize],
    build_is_left: bool,
    schema: &BindingTableSchema,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(usize, Vec<Vec<Value>>), ExecutorError> {
    let span = crate::SourceSpan::default();
    let mut domains = JoinDomain::default();
    let mut buckets: FxHashMap<RuntimeEqKey, Vec<usize>> = FxHashMap::default();
    let mut since_check = 0usize;
    for (index, row) in build_rows.iter().enumerate() {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        if let Some(key_values) = pattern::key_values_at(row, key_indexes) {
            domains.observe(&key_values)?;
            buckets
                .entry(RuntimeEqKey::from_row(key_values))
                .or_default()
                .push(index);
        }
    }
    // Count the fanout before materializing: a many-to-many blowup is
    // counted here with no per-match allocation, so the caller reserves the
    // exact output before any output row exists.
    let total = count_hash_matches(&buckets, &domains, probe_rows, key_indexes, ctx)?;
    let mut output = Vec::with_capacity(total);
    since_check = 0;
    for probe in probe_rows {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        // Deterministic repetition of the counting-pass checks: data errors
        // cannot appear here after passing there, and cancellation aborts
        // with no partial output either way.
        let Some(probe_eq) = checked_probe_key(probe, key_indexes, &domains)? else {
            continue;
        };
        if let Some(bucket) = buckets.get(&probe_eq) {
            for build_index in bucket {
                let build = &build_rows[*build_index];
                let merged = if build_is_left {
                    pattern::merge_rows(build, probe, schema)
                } else {
                    pattern::merge_rows(probe, build, schema)
                };
                output.push(merged.values().to_vec());
            }
        }
    }
    debug_assert_eq!(output.len(), total);
    Ok((total, output))
}

/// Run one probe's shared key checks: null-key skip, cross-input domain
/// comparison, and the probe self-check.
///
/// Returns the runtime-equality probe key when the probe may match, `None`
/// when it matches nothing. Pure over its inputs (data errors only), so
/// counting and emitting passes repeat it deterministically.
fn checked_probe_key(
    probe: &Binding,
    key_indexes: &[usize],
    domains: &JoinDomain,
) -> Result<Option<RuntimeEqKey>, ExecutorError> {
    let Some(probe_key) = pattern::key_values_at(probe, key_indexes) else {
        return Ok(None);
    };
    let mut probe_domain = JoinDomain::default();
    probe_domain.observe(&probe_key)?;
    domains.compare(&probe_domain)?;
    if !pattern::key_values_equal(&probe_key, &probe_key)? {
        return Ok(None);
    }
    Ok(Some(RuntimeEqKey::from_row(probe_key)))
}

/// Count probe-major hash matches without allocating per-match storage.
///
/// Runs the shared probe checks in walk order, accumulating bucket sizes
/// with checked arithmetic.
fn count_hash_matches(
    buckets: &FxHashMap<RuntimeEqKey, Vec<usize>>,
    domains: &JoinDomain,
    probe_rows: &[Binding],
    key_indexes: &[usize],
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<usize, ExecutorError> {
    let span = crate::SourceSpan::default();
    let mut total = 0usize;
    let mut since_check = 0usize;
    for probe in probe_rows {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        let Some(probe_eq) = checked_probe_key(probe, key_indexes, domains)? else {
            continue;
        };
        if let Some(bucket) = buckets.get(&probe_eq) {
            total = total
                .checked_add(bucket.len())
                .ok_or(ExecutorError::ProgramLimitExceeded {
                    detail: "batch join fanout exceeds the supported range",
                    span,
                })?;
        }
    }
    Ok(total)
}

/// Inner nested-loop join over materialized rows: the small-input path.
///
/// Runs the identical domain/key/merge sequence as [`hash_join_rows`] with a
/// linear build scan per probe instead of hash buckets, so rows, order, and
/// errors agree exactly while tiny inputs skip hash allocation. Fanout is
/// counted before materializing, as on the hash path.
///
/// # Errors
///
/// Returns the same domain, cancellation, and resource errors as the hash
/// path for the same inputs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nested_loop_join_rows(
    build_rows: &[Binding],
    probe_rows: &[Binding],
    key_indexes: &[usize],
    build_is_left: bool,
    schema: &BindingTableSchema,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(Vec<Vec<Value>>, usize), ExecutorError> {
    let width = schema.columns.len();
    let build_reserved = reserve_rows(
        ctx,
        build_rows.len(),
        width,
        "batch join build exceeds the supported range",
    )?;
    let joined = nested_loop_inner(
        build_rows,
        probe_rows,
        key_indexes,
        build_is_left,
        schema,
        ctx,
    );
    ctx.budget_mut().release(build_reserved);
    let (pairs, output) = joined?;
    let output_reserved = reserve_rows(
        ctx,
        pairs,
        width,
        "batch join fanout exceeds the supported range",
    )?;
    Ok((output, output_reserved))
}

#[allow(clippy::too_many_arguments)]
fn nested_loop_inner(
    build_rows: &[Binding],
    probe_rows: &[Binding],
    key_indexes: &[usize],
    build_is_left: bool,
    schema: &BindingTableSchema,
    ctx: &mut BatchExecutionContext<'_>,
) -> Result<(usize, Vec<Vec<Value>>), ExecutorError> {
    let span = crate::SourceSpan::default();
    // Precompute build keys once (null-key rows never match); per-probe
    // equality then runs the runtime-equality relation without hashing.
    let mut domains = JoinDomain::default();
    let mut build_keys: Vec<Option<RuntimeEqKey>> = Vec::with_capacity(build_rows.len());
    let mut since_check = 0usize;
    for row in build_rows {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        if let Some(key_values) = pattern::key_values_at(row, key_indexes) {
            domains.observe(&key_values)?;
            build_keys.push(Some(RuntimeEqKey::from_row(key_values)));
        } else {
            build_keys.push(None);
        }
    }
    let mut total = 0usize;
    since_check = 0;
    for probe in probe_rows {
        if since_check >= super::super::context::CANCEL_CHECK_STRIDE {
            ctx.check_cancel(span)?;
            since_check = 0;
        }
        since_check += 1;
        let Some(probe_eq) = checked_probe_key(probe, key_indexes, &domains)? else {
            continue;
        };
        let mut count = 0usize;
        for build_key in build_keys.iter() {
            if build_key.as_ref() == Some(&probe_eq) {
                count += 1;
            }
        }
        if count > 0 {
            total = total
                .checked_add(count)
                .ok_or(ExecutorError::ProgramLimitExceeded {
                    detail: "batch join fanout exceeds the supported range",
                    span,
                })?;
        }
    }
    // Emit pass over the counted fanout, repeating the deterministic probe
    // checks; the caller reserved the exact output before this allocates.
    let mut output = Vec::with_capacity(total);
    for probe in probe_rows {
        let Some(probe_eq) = checked_probe_key(probe, key_indexes, &domains)? else {
            continue;
        };
        for (index, build_key) in build_keys.iter().enumerate() {
            if build_key.as_ref() == Some(&probe_eq) {
                let build = &build_rows[index];
                let merged = if build_is_left {
                    pattern::merge_rows(build, probe, schema)
                } else {
                    pattern::merge_rows(probe, build, schema)
                };
                output.push(merged.values().to_vec());
            }
        }
    }
    debug_assert_eq!(output.len(), total);
    Ok((total, output))
}

/// Pull-based inner hash join over two child operators' batches.
///
/// Children materialize fully at `init` (both sides must be complete before
/// the first probe, as on the row path); pulls then slice the joined rows.
/// Output schema is the pattern schema both children share.
pub(crate) struct BatchHashJoin<'x, 'plan> {
    left: Box<dyn PhysicalOperator + 'x>,
    right: Box<dyn PhysicalOperator + 'x>,
    key: &'plan [DbString],
    build_side: BuildSide,
    schema: BindingTableSchema,
    policy: BatchPolicy,
    key_indexes: Vec<usize>,
    rows: Vec<Vec<Value>>,
    reserved_bytes: usize,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl<'x, 'plan> BatchHashJoin<'x, 'plan> {
    /// Construct a join over `left`/`right` on shared binding `key`.
    ///
    /// `schema` is the pattern schema both children materialize. Children
    /// pass positionally as left/right; the planner-selected `build_side`
    /// recovers probe-major output order.
    pub(crate) const fn new(
        left: Box<dyn PhysicalOperator + 'x>,
        right: Box<dyn PhysicalOperator + 'x>,
        key: &'plan [DbString],
        build_side: BuildSide,
        schema: BindingTableSchema,
        policy: BatchPolicy,
    ) -> Self {
        Self {
            left,
            right,
            key,
            build_side,
            schema,
            policy,
            key_indexes: Vec::new(),
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
                detail: "batch join init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.left.init(ctx)?;
        let left_rows = materialize_child(&mut self.left, ctx)?;
        self.right.init(ctx)?;
        let right_rows = materialize_child(&mut self.right, ctx)?;
        self.key_indexes = pattern::resolve_key(&self.schema, self.key)?;
        // The planner-selected build side also selects probe-major output
        // order, exactly as the row `execute_ordered` does.
        let (build_rows, probe_rows, build_is_left) = match self.build_side {
            BuildSide::Left => (left_rows, right_rows, true),
            BuildSide::Right => (right_rows, left_rows, false),
        };
        let (rows, reserved) =
            if use_nested_loop(build_rows.len(), probe_rows.len(), self.key_indexes.len()) {
                nested_loop_join_rows(
                    &build_rows,
                    &probe_rows,
                    &self.key_indexes,
                    build_is_left,
                    &self.schema,
                    ctx,
                )?
            } else {
                hash_join_rows(
                    &build_rows,
                    &probe_rows,
                    &self.key_indexes,
                    build_is_left,
                    &self.schema,
                    ctx,
                )?
            };
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
                        detail: "batch join built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch =
            BindingBatch::from_batch_columns(self.schema.clone(), batch_columns).map_err(|_| {
                ExecutorError::ImplementationDefined {
                    detail: "batch join built a malformed batch",
                }
            })?;
        ctx.budget_mut()
            .reserve(batch.estimated_bytes())
            .map_err(|err| err.into_executor_error(span))?;
        Ok(Some(batch))
    }
}

impl PhysicalOperator for BatchHashJoin<'_, '_> {
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
                detail: "batch join pull is legal only while Open",
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
        self.right.close(ctx);
        self.rows.clear();
        self.rows.shrink_to_fit();
        if self.reserved_bytes > 0 {
            ctx.budget_mut().release(self.reserved_bytes);
            self.reserved_bytes = 0;
        }
        self.key_indexes.clear();
        self.cursor = 0;
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.schema
    }
}
