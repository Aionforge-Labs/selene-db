//! Bounded product-graph execution of the F05 automata contract.
//!
//! This native integration seam is not a new GQL surface. The statement driver
//! retains legacy path routing until F05-PR04. Execution uses the existing batch
//! pin, cancellation, budget, lifecycle and materialization contracts. Only one
//! MATCH clause's flat automata are accepted. Path-local predicates precede
//! endpoint-partitioned selection; selected paths use the native typed value.
//! No partial result survives a failure. Open bounds are resource-limited,
//! never silently truncated. See `docs/gql/product-path-selection.md`.
//!
//! TEMPORARY observations are opt-in debugging data, not result columns or a
//! traversal-order promise. Presentation must use explicit deterministic keys.
//!
//! ```
//! use selene_gql::{analyze, parse, lower_path_automata_with_defaults,
//!     EmptyProcedureRegistry, ImplDefinedCaps, TxContext};
//! use selene_gql::runtime::product_path::{BoundedPathProgram, PathExecutionLimits};
//! use selene_core::GraphId;
//! use selene_graph::SharedGraph;
//!
//! let analyzed = analyze(parse("MATCH WALK (a)-[r{0,2}]->(b) RETURN a").unwrap(),
//!     &EmptyProcedureRegistry, None).unwrap();
//! let paths = lower_path_automata_with_defaults(&analyzed).unwrap();
//! let program = BoundedPathProgram::compile(&paths.automata, &analyzed).unwrap();
//! let graph = SharedGraph::new(GraphId::new(1));
//! let caps = ImplDefinedCaps::default();
//! let tx = TxContext::read_only(graph.read(), &caps, &EmptyProcedureRegistry,
//!     graph.index_providers());
//! let result = program.execute(&tx, PathExecutionLimits::default()).unwrap();
//! assert_eq!(result.table.row_count(), 0);
//! ```

mod compile;
mod conditions;
mod materialize;
mod search;
mod selection;
mod state;
mod telemetry;
mod termination;

pub use compile::BoundedPathProgram;
pub use telemetry::{CheapestCostProjection, PathExecutionStats, PathObservation};

use super::batch::{
    binding_batch::{BatchBuffer, BindingBatch},
    budget::MemoryBudget,
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
    tracer::trace_operator_to_table,
    unit::BatchRowSource,
};
use super::{BindingTable, ExecutorError, TxContext};
use crate::BindingTableSchema;

/// Explicit resource policy for a bounded traversal, never a truncation policy.
#[derive(Clone, Copy, Debug)]
pub struct PathExecutionLimits {
    /// Largest sum of declared upper bounds in one path pattern. Native execution
    /// also respects the statement's `max_quantifier` cap, whichever is smaller.
    pub max_hops: u32,
    /// Maximum examined product states plus candidate incidences.
    pub max_work: u64,
    /// Maximum complete binding rows before temporary reduction.
    pub max_rows: usize,
    /// Estimated retained search/output/debug bytes, not process heap usage.
    pub max_bytes: usize,
    /// Maximum hop observations when debugging is enabled (excess fails).
    pub max_observations: usize,
    /// Retain hop-by-hop locals and choice points outside the result table.
    pub observe: bool,
}

impl Default for PathExecutionLimits {
    fn default() -> Self {
        Self {
            max_hops: 1024,
            max_work: 1_000_000,
            max_rows: 100_000,
            max_bytes: 64 * 1024 * 1024,
            max_observations: 100_000,
            observe: false,
        }
    }
}

/// A complete bounded execution; graph identities, never storage row offsets.
#[derive(Debug)]
pub struct PathExecution {
    /// Named binding columns only, preserving binding multiplicity.
    pub table: BindingTable,
    /// Observed work and output-sensitive costs.
    pub stats: PathExecutionStats,
    /// Optional TEMPORARY debug exposure, never part of GQL results.
    pub observations: Vec<PathObservation>,
}

impl BoundedPathProgram<'_> {
    /// Execute against the statement's pinned graph through the batch driver substrate.
    ///
    /// # Errors
    /// Returns typed cancellation, deadline, work, memory, row or hop-limit
    /// errors. Even a late error discards the entire result. There is no fallback.
    pub fn execute(
        &self,
        tx: &TxContext<'_, '_>,
        mut limits: PathExecutionLimits,
    ) -> Result<PathExecution, ExecutorError> {
        limits.max_hops = limits.max_hops.min(tx.impl_defined_caps().max_quantifier);
        let mut ctx = BatchExecutionContext::borrowed(
            tx.snapshot(),
            tx.batch_cancel(),
            MemoryBudget::unlimited(),
        );
        let subqueries = crate::SubqueryRegistry::default();
        let eval = super::EvalCtx {
            tx,
            expr_ids: &self.expr_ids,
            subqueries: &subqueries,
        };
        let qualify = |state: &state::SearchState, entity: &selene_core::Value, phase| {
            conditions::evaluate(self, state, entity, phase, &eval)
        };
        let mut operator = ProductPathOperator::new(self, limits, BatchPolicy::default_policy());
        operator.qualify = Some(&qualify);
        let table = trace_operator_to_table(&mut operator, &mut ctx)?;
        tx.note_result_rows(table.row_count())?;
        Ok(PathExecution {
            table,
            stats: operator.stats,
            observations: operator.observations,
        })
    }
}

/// Eager failure barrier followed by ordinary bounded batch pulls. F05-PR04
/// can compose this source with the existing pipeline operators without rows
/// or search frames becoming an intermediate public result.
pub(crate) struct ProductPathOperator<'a, 'p> {
    program: &'a BoundedPathProgram<'p>,
    limits: PathExecutionLimits,
    policy: BatchPolicy,
    source: Option<BatchRowSource>,
    lifecycle: OperatorState,
    stats: PathExecutionStats,
    observations: Vec<PathObservation>,
    reserved: usize,
    qualify: Option<&'a conditions::Qualifier<'a>>,
}

impl<'a, 'p> ProductPathOperator<'a, 'p> {
    pub(crate) fn new(
        program: &'a BoundedPathProgram<'p>,
        limits: PathExecutionLimits,
        policy: BatchPolicy,
    ) -> Self {
        Self {
            program,
            limits,
            policy,
            source: None,
            lifecycle: OperatorState::Created,
            stats: PathExecutionStats::default(),
            observations: Vec::new(),
            reserved: 0,
            qualify: None,
        }
    }
}

impl PhysicalOperator for ProductPathOperator<'_, '_> {
    fn init(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.lifecycle != OperatorState::Created {
            return Err(compile::invalid(
                "product path init is legal only once from Created",
            ));
        }
        self.lifecycle = OperatorState::Failed;
        ctx.ensure_generation()?;
        let result = search::execute(self.program, self.limits, ctx, self.qualify)?;
        self.stats = result.stats;
        self.observations = result.observations;
        self.reserved = result.reserved;
        self.source = Some(BatchRowSource::new(result.table, self.policy));
        self.source
            .as_mut()
            .expect("source just assigned")
            .init(ctx)?;
        self.lifecycle = OperatorState::Open;
        Ok(())
    }

    fn next_batch(
        &mut self,
        ctx: &mut BatchExecutionContext<'_>,
        buffer: &mut BatchBuffer,
    ) -> Result<Option<BindingBatch>, ExecutorError> {
        if self.lifecycle == OperatorState::Exhausted {
            return Ok(None);
        }
        if self.lifecycle != OperatorState::Open {
            return Err(compile::invalid(
                "product path pull is legal only while Open",
            ));
        }
        let result = self
            .source
            .as_mut()
            .expect("Open owns source")
            .next_batch(ctx, buffer);
        match &result {
            Ok(None) => self.lifecycle = OperatorState::Exhausted,
            Err(_) => self.lifecycle = OperatorState::Failed,
            Ok(Some(_)) => {}
        }
        result
    }

    fn close(&mut self, ctx: &mut BatchExecutionContext<'_>) {
        if let Some(source) = &mut self.source {
            source.close(ctx);
        }
        ctx.budget_mut().release(self.reserved);
        self.reserved = 0;
        ctx.close();
        self.lifecycle = OperatorState::Closed;
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.program.schema
    }
}

#[cfg(test)]
mod boundaries;
#[cfg(test)]
mod differentials;
#[cfg(test)]
mod oracle;
#[cfg(test)]
mod selection_boundaries;
#[cfg(test)]
mod selector_tests;
#[cfg(test)]
mod tests;
