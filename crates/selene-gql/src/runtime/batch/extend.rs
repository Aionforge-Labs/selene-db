//! Eager batch extensions: LET, list expansion, and correlated table calls.
//!
//! Drain input before exposing output so LIMIT cannot suppress a later input's
//! expression error or a table call's observable native operation.

use super::{
    binding_batch::{BatchBuffer, BindingBatch},
    join::reserve_rows,
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};
use crate::{
    BindingTableSchema, PipelineOp,
    runtime::{Binding, EvalCtx, ExecutorError},
};

pub(crate) struct BatchExtend<'x, 'a, 'ctx, 'g, 'p> {
    child: Box<dyn PhysicalOperator + 'x>,
    op: &'p PipelineOp,
    eval: EvalCtx<'a, 'ctx, 'g, 'p>,
    schema: BindingTableSchema,
    policy: BatchPolicy,
    rows: Vec<Binding>,
    reserved: usize,
    cursor: usize,
    state: OperatorState,
}

impl<'x, 'a, 'ctx, 'g, 'p> BatchExtend<'x, 'a, 'ctx, 'g, 'p> {
    pub(crate) fn new(
        child: Box<dyn PhysicalOperator + 'x>,
        op: &'p PipelineOp,
        eval: EvalCtx<'a, 'ctx, 'g, 'p>,
        policy: BatchPolicy,
    ) -> Result<Self, ExecutorError> {
        let schema = super::extend_kernel::output_schema(op, child.output_schema())?;
        Ok(Self {
            child,
            op,
            eval,
            schema,
            policy,
            rows: Vec::new(),
            reserved: 0,
            cursor: 0,
            state: OperatorState::Created,
        })
    }

    fn init_inner(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(ExecutorError::ImplementationDefined {
                detail: "batch extension init is legal only once",
            });
        }
        if let PipelineOp::CallSubquery(call) = self.op
            && crate::plan::classify_plan(&call.body).rejects_in_read_only()
        {
            return Err(crate::runtime::pipeline::read_only_write_op_error());
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        self.child.init(ctx)?;
        let input = self.child.output_schema().clone();
        let mut buffer = BatchBuffer::new();
        while let Some(batch) = self.child.next_batch(ctx, &mut buffer)? {
            let result: Result<(), ExecutorError> = (|| {
                for index in 0..batch.logical_rows() {
                    ctx.check_cancel(crate::SourceSpan::default())?;
                    let row = batch.logical_binding(index);
                    super::extend_kernel::extend(
                        self.op,
                        &row,
                        &input,
                        self.eval,
                        self.policy,
                        |row| {
                            self.reserved += reserve_rows(
                                ctx,
                                1,
                                self.schema.columns.len(),
                                "batch extension fanout exceeds supported range",
                            )?;
                            self.rows.push(row);
                            Ok(())
                        },
                    )?;
                }
                Ok(())
            })();
            ctx.budget_mut().release(batch.estimated_bytes());
            batch.recycle(&mut buffer);
            result?;
        }
        self.state = OperatorState::Open;
        Ok(())
    }
}

impl PhysicalOperator for BatchExtend<'_, '_, '_, '_, '_> {
    fn init(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        let result = self.init_inner(ctx);
        if result.is_err() {
            self.state = OperatorState::Failed;
        }
        result
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
                detail: "batch extension pull is legal only while Open",
            });
        }
        if self.schema.columns.is_empty() && self.cursor < self.rows.len() {
            ctx.ensure_generation()?;
            ctx.check_cancel(crate::SourceSpan::default())?;
            self.cursor += 1;
            ctx.finish_batch(1);
            return Ok(Some(BindingBatch::unit()));
        }
        let result = super::sort::slice_rows(
            &self.schema,
            &self.rows,
            &mut self.cursor,
            self.policy,
            ctx,
            buffer,
            "batch extension built a malformed batch",
        );
        match &result {
            Ok(None) => self.state = OperatorState::Exhausted,
            Err(_) => self.state = OperatorState::Failed,
            _ => {}
        }
        result
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
