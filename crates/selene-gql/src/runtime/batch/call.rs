//! Typed native calls over physical input batches.
//!
//! Calls are an eager barrier: every input invokes exactly once before output
//! is pulled, including when a later LIMIT discards rows. This preserves errors
//! and ephemeral projection operations from the existing CALL contract. Native
//! code receives only graph authority; writes retain the transaction dispatcher.

use crate::{
    PlannedCall, ProcedureMutability, ProcedureTier,
    plan::BindingTableSchema,
    runtime::{Binding, EvalCtx, ExecutorError, pipeline::call},
};

use super::{
    binding_batch::{BatchBuffer, BindingBatch},
    join::reserve_rows,
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
    sort::slice_rows,
};

pub(crate) struct BatchCall<'x, 'a, 'ctx, 'g, 'plan> {
    child: Box<dyn PhysicalOperator + 'x>,
    call: &'plan PlannedCall,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    schema: BindingTableSchema,
    policy: BatchPolicy,
    rows: Vec<Binding>,
    reserved: usize,
    cursor: usize,
    state: OperatorState,
}

impl<'x, 'a, 'ctx, 'g, 'plan> BatchCall<'x, 'a, 'ctx, 'g, 'plan> {
    pub(crate) fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        call: &'plan PlannedCall,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        policy: BatchPolicy,
    ) -> Self {
        let schema = call::output_schema(child.output_schema(), call);
        Self {
            child,
            call,
            eval,
            schema,
            policy,
            rows: Vec::new(),
            reserved: 0,
            cursor: 0,
            state: OperatorState::Created,
        }
    }

    fn init_inner(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch call init is legal only once",
            });
        }
        call::validate_registration(self.call, self.eval.tx)?;
        if self.call.tier != ProcedureTier::Graph
            || self.call.mutability != ProcedureMutability::Read
        {
            return Err(ExecutorError::InvalidTransactionState {
                detail: "batch query call requires graph-read authority",
                span: self.call.span,
            });
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(self.call.span)?;
        self.child.init(ctx)?;
        let schema = self.child.output_schema().clone();
        let mut buffer = BatchBuffer::new();
        while let Some(batch) = self.child.next_batch(ctx, &mut buffer)? {
            let outcome = self.consume(&batch, &schema, ctx);
            ctx.budget_mut().release(batch.estimated_bytes());
            batch.recycle(&mut buffer);
            outcome?;
        }
        self.state = OperatorState::Open;
        Ok(())
    }

    fn consume(
        &mut self,
        batch: &BindingBatch,
        schema: &BindingTableSchema,
        ctx: &mut BatchExecutionContext<'_>,
    ) -> Result<(), ExecutorError> {
        for index in 0..batch.logical_rows() {
            ctx.check_cancel(self.call.span)?;
            let row = Binding::new(batch.logical_row(index));
            let args = call::evaluate_args(&self.call.args, &row, schema, &self.eval)?;
            call::validate_arguments(self.call, &args)?;
            let mut authority = call::context::build_read_only(self.call, self.eval.tx)?;
            let result = self
                .eval
                .tx
                .registry()
                .execute(self.call.handle, &args, &mut authority)
                .map_err(|source| {
                    call::context::procedure_error(source, self.call.span, self.eval.tx.deadline())
                })?;
            let count = if self.call.optional && result.rows.is_empty() {
                1
            } else {
                result.rows.len()
            };
            self.reserved += reserve_rows(
                ctx,
                count,
                self.schema.columns.len(),
                "batch call result exceeds supported range",
            )?;
            if self.call.optional && result.rows.is_empty() {
                self.rows.push(call::optional_output_row(self.call, &row));
            } else {
                for output in result.rows {
                    ctx.check_cancel(self.call.span)?;
                    let values = call::project::project_yield_row(self.call, output)?;
                    self.rows.push(row.with_appended_values(values));
                }
            }
        }
        Ok(())
    }
}

impl PhysicalOperator for BatchCall<'_, '_, '_, '_, '_> {
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
                detail: "batch call pull is legal only while Open",
            });
        }
        // A zero-column unit must not be mistaken for an empty relation by a
        // column-length-derived constructor. Emit explicit units, preserving
        // multiplicity for projection_build/drop and calls without YIELD.
        if self.schema.columns.is_empty() && self.cursor < self.rows.len() {
            ctx.ensure_generation()?;
            ctx.check_cancel(self.call.span)?;
            self.cursor += 1;
            ctx.finish_batch(1);
            return Ok(Some(BindingBatch::unit()));
        }
        let outcome = slice_rows(
            &self.schema,
            &self.rows,
            &mut self.cursor,
            self.policy,
            ctx,
            buffer,
            "batch call built a malformed batch",
        );
        match &outcome {
            Ok(None) => self.state = OperatorState::Exhausted,
            Err(_) => self.state = OperatorState::Failed,
            _ => {}
        }
        outcome
    }

    fn close(&mut self, ctx: &mut BatchExecutionContext<'_>) {
        self.rows.clear();
        ctx.budget_mut().release(self.reserved);
        self.reserved = 0;
        self.child.close(ctx);
        self.state = OperatorState::Closed;
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.schema
    }
}
