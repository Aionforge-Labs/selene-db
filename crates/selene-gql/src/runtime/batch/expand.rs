//! Batch one-hop expansion over mixed-edge adjacency.
//!
//! [`BatchExpand`] carries a single inner join-tree expansion
//! (`JoinTree::Expand`) into batches: for every child logical row it emits one
//! output row per adjacent edge that survives the edge-label, right-node, and
//! property-predicate checks, preserving parallel-edge multiplicity and the
//! mixed directed/undirected orientation union (including the directed-loop
//! dedup). Anonymous and named bindings land in the same pattern-schema slots
//! the row expansion uses.
//!
//! Order equivalence with the row path is structural, not hoped for: `init`
//! materializes the child input, then replays the row expansion's own branch
//! choice (indexed-edge iteration when the edge-candidate filter is no larger
//! than the child input, per-source adjacency iteration otherwise) with the
//! same emit sequence. Pulls then slice the materialized output. This keeps
//! memory at the row path's level for expansions; a streaming expand that
//! preserves the indexed-path order is follow-up work, not this slice.
//!
//! Optional (`?`) and outer (`OPTIONAL MATCH`) null extension stays with the
//! row executor in this slice: only inner one-hop expansion runs here.

use std::collections::BTreeMap;
use std::mem::size_of;

use selene_core::{EdgeId, NodeId, Value};
use selene_graph::CandidateSet;

use crate::{
    EdgeDirection, EdgeMatch, PatternPlan,
    plan::BindingTableSchema,
    runtime::{Binding, EvalCtx, ExecutorError},
};

use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};

use super::super::edge_access::{
    adjacent_edges, candidate_edge_filter, edge_filter_matches, next_node,
};
use super::super::pattern::{ColumnSlot, node_at_index, source_index};
use super::super::scan::{label_matches_edge, label_matches_node, predicate_passes};

/// Pull-based inner one-hop expansion over a child operator's batches.
///
/// Output rows keep the pattern schema: the edge and right-node slots are
/// filled per emitted edge while every other column carries the child row.
pub(crate) struct BatchExpand<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
    edge: &'plan EdgeMatch,
    direction: EdgeDirection,
    pattern: &'plan PatternPlan,
    schema: BindingTableSchema,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    policy: BatchPolicy,
    rows: Vec<Vec<Value>>,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl<'x, 'a, 'ctx, 'g, 'plan> BatchExpand<'x, 'a, 'ctx, 'g, 'plan> {
    /// Construct an expansion over `child` traversing `edge` in `direction`.
    pub(crate) const fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        edge: &'plan EdgeMatch,
        direction: EdgeDirection,
        pattern: &'plan PatternPlan,
        schema: BindingTableSchema,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        Self {
            child,
            edge,
            direction,
            pattern,
            schema,
            eval,
            policy,
            rows: Vec::new(),
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

    /// Return the total materialized output row count.
    ///
    /// Test seam for expansion assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn output_count(&self) -> usize {
        self.rows.len()
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
                detail: "batch expand init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.child.init(ctx)?;
        let child_rows = self.materialize_child(ctx)?;
        let snapshot = ctx.snapshot()?;
        let state = ExpandSlots::resolve(self.edge, self.pattern, &self.schema)?;
        let source = source_index(
            self.pattern,
            &self.schema,
            self.edge.left_binding,
            self.edge.left_hidden_binding,
            "expand source binding column missing",
        )?;
        // The edge-candidate filter resolves once against the pinned
        // snapshot, exactly as the row expansion resolves it per execution.
        let filter = candidate_edge_filter(self.edge, &self.eval)?;
        let mut output: Vec<Vec<Value>> = Vec::new();
        // Branch choice mirrors the row expansion so the emitted order is
        // identical: indexed-edge iteration only when the filter is no
        // larger than the child input.
        if filter
            .as_ref()
            .is_some_and(|set| set.len() <= child_rows.len())
        {
            expand_indexed(
                &child_rows,
                &filter,
                snapshot,
                self.direction,
                &state,
                self.edge,
                self.pattern,
                &self.schema,
                source,
                &self.eval,
                &mut output,
            )?;
        } else {
            expand_adjacent(
                &child_rows,
                snapshot,
                self.direction,
                &filter,
                &state,
                self.edge,
                self.pattern,
                &self.schema,
                source,
                &self.eval,
                &mut output,
            )?;
        }
        self.rows = output;
        self.cursor = 0;
        self.state = OperatorState::Open;
        Ok(())
    }

    fn materialize_child(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
    ) -> Result<Vec<Binding>, ExecutorError> {
        let mut buffer = BatchBuffer::new();
        let mut rows = Vec::new();
        while let Some(batch) = self.child.next_batch(ctx, &mut buffer)? {
            rows.reserve(batch.logical_rows());
            for index in 0..batch.logical_rows() {
                rows.push(Binding::new(batch.logical_row(index)));
            }
            ctx.budget_mut().release(batch.estimated_bytes());
            batch.recycle(&mut buffer);
        }
        Ok(rows)
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
        let rows = self
            .policy
            .rows_per_batch(
                width
                    .saturating_mul(size_of::<Value>().saturating_add(1))
                    .max(1),
            )
            .min(self.rows.len() - self.cursor);
        let mut columns: Vec<(Vec<Value>, Vec<bool>)> = Vec::with_capacity(width);
        for _ in 0..width {
            let mut values = buffer.take_values();
            let mut nulls = buffer.take_nulls();
            values.clear();
            nulls.clear();
            columns.push((values, nulls));
        }
        for row in &self.rows[self.cursor..self.cursor + rows] {
            for (slot, (column, nulls)) in columns.iter_mut().enumerate() {
                let value = row.get(slot).cloned().unwrap_or(Value::Null);
                nulls.push(value == Value::Null);
                column.push(value);
            }
        }
        self.cursor += rows;
        self.batches_produced += 1;
        ctx.finish_batch(rows);
        let batch_columns = columns
            .into_iter()
            .map(|(values, nulls)| {
                BatchColumn::from_parts(values, nulls).map_err(|_| {
                    ExecutorError::ImplementationDefined {
                        detail: "batch expand built a malformed batch",
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch =
            BindingBatch::from_batch_columns(self.schema.clone(), batch_columns).map_err(|_| {
                ExecutorError::ImplementationDefined {
                    detail: "batch expand built a malformed batch",
                }
            })?;
        ctx.budget_mut()
            .reserve(batch.estimated_bytes())
            .map_err(|err| err.into_executor_error(span))?;
        Ok(Some(batch))
    }
}

impl PhysicalOperator for BatchExpand<'_, '_, '_, '_, '_> {
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
                detail: "batch expand pull is legal only while Open",
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
        self.cursor = 0;
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.schema
    }
}

/// Resolved output slots for one expansion.
struct ExpandSlots {
    edge: ColumnSlot,
    edge_hidden: ColumnSlot,
    right: ColumnSlot,
    right_hidden: ColumnSlot,
}

impl ExpandSlots {
    fn resolve(
        edge: &EdgeMatch,
        pattern: &PatternPlan,
        schema: &BindingTableSchema,
    ) -> Result<Self, ExecutorError> {
        // The source slot resolves separately at each use site through
        // `source_index`, matching the row expansion's error detail per site.
        Ok(Self {
            edge: ColumnSlot::binding(
                pattern,
                schema,
                edge.binding,
                "expand edge binding column missing",
            )?,
            edge_hidden: ColumnSlot::hidden(
                schema,
                edge.hidden_binding,
                "expand edge hidden binding column missing",
            )?,
            right: ColumnSlot::binding(
                pattern,
                schema,
                edge.right_binding,
                "expand right binding column missing",
            )?,
            right_hidden: ColumnSlot::hidden(
                schema,
                edge.right_hidden_binding,
                "expand right hidden binding column missing",
            )?,
        })
    }
}

/// Per-source adjacency iteration (the row expansion's default branch).
#[allow(clippy::too_many_arguments)]
fn expand_adjacent(
    child_rows: &[Binding],
    snapshot: &selene_graph::SeleneGraph,
    direction: EdgeDirection,
    filter: &Option<CandidateSet<selene_graph::Edge>>,
    slots: &ExpandSlots,
    edge: &EdgeMatch,
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    source_index: usize,
    eval: &EvalCtx<'_, '_, '_, '_>,
    output: &mut Vec<Vec<Value>>,
) -> Result<(), ExecutorError> {
    for row in child_rows {
        let Some(source) = node_at_index(row, source_index, "expand source binding is not a node")?
        else {
            continue;
        };
        for adjacent in adjacent_edges(snapshot, source, direction) {
            if let Some(emitted) = maybe_emit(
                adjacent.edge_id,
                adjacent.neighbor,
                row,
                filter,
                slots,
                edge,
                pattern,
                schema,
                eval,
            )? {
                output.push(emitted);
            }
        }
    }
    Ok(())
}

/// Indexed-edge iteration (the row expansion's small-filter branch).
#[allow(clippy::too_many_arguments)]
fn expand_indexed(
    child_rows: &[Binding],
    filter: &Option<CandidateSet<selene_graph::Edge>>,
    snapshot: &selene_graph::SeleneGraph,
    direction: EdgeDirection,
    slots: &ExpandSlots,
    edge: &EdgeMatch,
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    source_index: usize,
    eval: &EvalCtx<'_, '_, '_, '_>,
    output: &mut Vec<Vec<Value>>,
) -> Result<(), ExecutorError> {
    let edge_ids: Vec<EdgeId> = match filter.as_ref() {
        Some(candidates) => candidates.iter().collect(),
        None => return Ok(()),
    };
    let mut rows_by_source: BTreeMap<NodeId, Vec<&Binding>> = BTreeMap::new();
    for row in child_rows {
        let Some(source) = node_at_index(row, source_index, "expand source binding is not a node")?
        else {
            continue;
        };
        rows_by_source.entry(source).or_default().push(row);
    }
    for edge_id in edge_ids {
        let Some((source, target)) = snapshot.edge_endpoints(edge_id) else {
            continue;
        };
        for current in [Some(source), (target != source).then_some(target)]
            .into_iter()
            .flatten()
        {
            if let Some(next) = next_node(snapshot, edge_id, current, direction) {
                let Some(rows) = rows_by_source.get(&current) else {
                    continue;
                };
                for row in rows {
                    if let Some(emitted) = maybe_emit(
                        edge_id, next, row, filter, slots, edge, pattern, schema, eval,
                    )? {
                        output.push(emitted);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Emit one expanded row, or `None` when the edge does not survive the
/// label, binding-unification, and property-predicate checks.
///
/// Mirrors the row expansion's `maybe_emit` exactly, including the slot-set
/// unification order and the edge-then-right predicate order.
#[allow(clippy::too_many_arguments)]
fn maybe_emit(
    edge_id: EdgeId,
    right_node: NodeId,
    row: &Binding,
    filter: &Option<CandidateSet<selene_graph::Edge>>,
    slots: &ExpandSlots,
    edge: &EdgeMatch,
    pattern: &PatternPlan,
    schema: &BindingTableSchema,
    eval: &EvalCtx<'_, '_, '_, '_>,
) -> Result<Option<Vec<Value>>, ExecutorError> {
    if !edge_filter_matches(filter.as_ref(), edge_id) {
        return Ok(None);
    }
    if !edge_label_matches(edge, edge_id, eval) || !right_node_matches(edge, right_node, eval) {
        return Ok(None);
    }
    let mut values = row.values().to_vec();
    values.resize(schema.columns.len(), Value::Null);
    if !slots.edge.set(&mut values, Value::EdgeRef(edge_id)) {
        return Ok(None);
    }
    if !slots.edge_hidden.set(&mut values, Value::EdgeRef(edge_id)) {
        return Ok(None);
    }
    if !slots.right.set(&mut values, Value::NodeRef(right_node)) {
        return Ok(None);
    }
    if !slots
        .right_hidden
        .set(&mut values, Value::NodeRef(right_node))
    {
        return Ok(None);
    }
    // Pattern-phase rows carry no insert sites (scans and expansions build
    // site-free rows), so plain value storage preserves the row content.
    let candidate = Binding::new(values);
    if !predicates_pass(
        &edge.property_predicates,
        pattern,
        &candidate,
        schema,
        &Value::EdgeRef(edge_id),
        eval,
    )? {
        return Ok(None);
    }
    if !predicates_pass(
        &edge.right_property_predicates,
        pattern,
        &candidate,
        schema,
        &Value::NodeRef(right_node),
        eval,
    )? {
        return Ok(None);
    }
    Ok(Some(candidate.values().to_vec()))
}

/// Evaluate edge/right-node property predicates: every predicate must pass.
///
/// Mirrors the row expansion's predicate loop over the shared singular
/// predicate helper, so residual semantics (including index-consumed skips)
/// agree exactly.
fn predicates_pass(
    predicates: &[crate::FilterPredicate],
    pattern: &PatternPlan,
    row: &Binding,
    schema: &BindingTableSchema,
    entity: &Value,
    eval: &EvalCtx<'_, '_, '_, '_>,
) -> Result<bool, ExecutorError> {
    for predicate in predicates {
        if !predicate_passes(predicate, pattern, row, schema, entity, eval)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn edge_label_matches(edge: &EdgeMatch, edge_id: EdgeId, ctx: &EvalCtx<'_, '_, '_, '_>) -> bool {
    let Some(label_expr) = &edge.label_predicate else {
        return true;
    };
    ctx.tx
        .snapshot()
        .edge_label(edge_id)
        .is_some_and(|label| label_matches_edge(label_expr, label))
}

fn right_node_matches(edge: &EdgeMatch, node: NodeId, ctx: &EvalCtx<'_, '_, '_, '_>) -> bool {
    let Some(label_expr) = &edge.right_label_predicate else {
        return true;
    };
    ctx.tx
        .snapshot()
        .node_labels(node)
        .is_some_and(|labels| label_matches_node(label_expr, labels))
}
