//! The one working batch scan: node access lowered into batches.
//!
//! [`BatchNodeScan`] covers the two label shapes the tracer executes in this
//! slice: unlabeled node access and single-label node access. Candidates are
//! resolved exactly once in [`PhysicalOperator::init`] through the F01-PR02
//! typed candidate APIs
//! ([`CandidateSet`](selene_graph::CandidateSet) over
//! [`Node`](selene_graph::Node)) against the context's pinned snapshot; pulls
//! then slice the captured stable [`NodeId`](selene_core::NodeId)s into
//! batches. Property predicates, edge access, and wider index paths stay with
//! the row executor until their owning F04 slices.
//!
//! This operator never calls the row evaluator: there is no per-row
//! expression evaluation disguised as batches. Each output value is a stable
//! graph identity (`Value::NodeRef`); batch positions never appear in output.

use std::mem::size_of;

use selene_core::{DbString, NodeId, Value};
use selene_graph::{CandidateSet, GraphError, Node};

use crate::{SourceSpan, plan::BindingTableSchema, runtime::ExecutorError};

use super::{
    binding_batch::{BatchBuffer, BatchColumn, BindingBatch},
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};

fn scan_error(_err: GraphError) -> ExecutorError {
    ExecutorError::ImplementationDefined {
        detail: "graph scan candidate error",
    }
}

/// What the batch node scan reads: all live nodes or one label's nodes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BatchScanSpec {
    /// Single label to restrict to, or `None` for all live nodes.
    pub(crate) label: Option<DbString>,
    /// Declared single-column output schema.
    pub(crate) schema: BindingTableSchema,
}

impl BatchScanSpec {
    /// Build a spec, requiring a single-column output schema.
    ///
    /// # Errors
    ///
    /// Returns `ImplementationDefined` when the schema width is not one.
    pub(crate) fn new(
        label: Option<DbString>,
        schema: BindingTableSchema,
    ) -> Result<Self, ExecutorError> {
        if schema.columns.len() != 1 {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch node scan requires a single-column schema",
            });
        }
        Ok(Self { label, schema })
    }
}

/// Pull-based node scan producing one-column batches of node references.
///
/// The operator captures stable node identities at `init` and serves them in
/// policy-sized slices. It holds no snapshot of its own: every pull goes
/// through the execution context, which enforces the pinned generation and
/// cancellation checkpoints.
#[derive(Debug)]
pub(crate) struct BatchNodeScan {
    spec: BatchScanSpec,
    policy: BatchPolicy,
    candidates: Vec<NodeId>,
    cursor: usize,
    state: OperatorState,
    batches_produced: u64,
}

impl BatchNodeScan {
    /// Construct a scan over `spec` with `policy` sizing.
    #[must_use]
    pub(crate) const fn new(spec: BatchScanSpec, policy: BatchPolicy) -> Self {
        Self {
            spec,
            policy,
            candidates: Vec::new(),
            cursor: 0,
            state: OperatorState::Created,
            batches_produced: 0,
        }
    }

    /// Return the number of batches produced so far.
    #[must_use]
    pub(crate) const fn batches_produced(&self) -> u64 {
        self.batches_produced
    }

    /// Return the total captured candidate count.
    #[must_use]
    pub(crate) fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    fn resolve_candidates(
        label: Option<&DbString>,
        snapshot: &selene_graph::SeleneGraph,
    ) -> Result<CandidateSet<Node>, ExecutorError> {
        match label {
            Some(label) => snapshot
                .node_candidates_with_label(label)
                .map_err(scan_error),
            None => snapshot.live_node_candidates().map_err(scan_error),
        }
    }
}

impl PhysicalOperator for BatchNodeScan {
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
        self.candidates.clear();
        self.candidates.shrink_to_fit();
        self.cursor = 0;
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn state(&self) -> OperatorState {
        self.state
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.spec.schema
    }
}

impl BatchNodeScan {
    fn init_inner(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch scan init is legal only once from Created",
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(SourceSpan::default())?;
        let span = SourceSpan::default();
        let candidates = Self::resolve_candidates(self.spec.label.as_ref(), ctx.snapshot()?)?;
        // The typed candidate set is the F01-PR02 contract: graph, generation,
        // and layout are validated by construction against this snapshot.
        self.candidates = candidates.iter().collect();
        // Deterministic scan order for the pinned snapshot: candidate order is
        // the snapshot's canonical order, identical for every pull.
        ctx.note_nodes_scanned(self.candidates.len(), span)?;
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
        if self.cursor >= self.candidates.len() {
            self.state = OperatorState::Exhausted;
            return Ok(None);
        }
        let span = SourceSpan::default();
        ctx.check_cancel(span)?;
        let rows = self
            .policy
            .rows_per_batch(size_of::<Value>().saturating_add(1))
            .min(self.candidates.len() - self.cursor);

        let mut values = buffer.take_values();
        let mut nulls = buffer.take_nulls();
        values.clear();
        nulls.clear();
        values.reserve(rows);
        nulls.reserve(rows);
        for id in &self.candidates[self.cursor..self.cursor + rows] {
            values.push(Value::NodeRef(*id));
            nulls.push(false);
        }
        self.cursor += rows;
        self.batches_produced += 1;
        ctx.finish_batch(rows);
        // The taken vectors become the batch storage directly: no allocation
        // on the reuse path. Scan values are never null; the bitmap round
        // trip keeps `from_parts` as the single alignment authority.
        let column = BatchColumn::from_parts(values, nulls).map_err(|_| {
            ExecutorError::ImplementationDefined {
                detail: "batch scan built a malformed batch",
            }
        })?;
        debug_assert!(column.nulls().iter().all(|n| !n));
        let batch = BindingBatch::from_batch_columns(self.spec.schema.clone(), vec![column])
            .map_err(|_| ExecutorError::ImplementationDefined {
                detail: "batch scan built a malformed batch",
            })?;
        // Account for the batch this pull keeps alive. The tracer releases
        // the reservation when it recycles the batch, so the budget bounds
        // live plus retained storage instead of accumulating closed history.
        // A failed reservation drops the batch (freeing its fresh growth)
        // and reports without producing rows.
        ctx.budget_mut()
            .reserve(batch.estimated_bytes())
            .map_err(|err| err.into_executor_error(span))?;
        Ok(Some(batch))
    }
}
