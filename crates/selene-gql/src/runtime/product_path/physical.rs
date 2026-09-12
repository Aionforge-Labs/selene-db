//! Physical statement operator: seeded inputs use the same native search barrier.

use super::*;
use crate::{
    PathProgram,
    runtime::{Binding, EvalCtx, pattern},
};
use selene_core::Value;

pub(crate) struct BatchPath<'a, 'ctx, 'g, 'plan> {
    plan: &'plan PathProgram,
    eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
    schema: BindingTableSchema,
    seed: Option<Binding>,
    policy: BatchPolicy,
    source: Option<BatchRowSource>,
    state: OperatorState,
    reserved: usize,
}

impl<'a, 'ctx, 'g, 'plan> BatchPath<'a, 'ctx, 'g, 'plan> {
    pub(crate) fn new(
        plan: &'plan PathProgram,
        eval: EvalCtx<'a, 'ctx, 'g, 'plan>,
        schema: BindingTableSchema,
        seed: Option<Binding>,
        policy: BatchPolicy,
    ) -> Self {
        Self {
            plan,
            eval,
            schema,
            seed,
            policy,
            source: None,
            state: OperatorState::Created,
            reserved: 0,
        }
    }
}

impl PhysicalOperator for BatchPath<'_, '_, '_, '_> {
    fn init(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(compile::invalid("path init is legal only from Created"));
        }
        self.state = OperatorState::Failed;
        ctx.ensure_generation()?;
        let mut program = BoundedPathProgram::from_plan(self.plan)?;
        // Carry non-pattern input columns so correlated predicates can resolve
        // scalars too. Pattern identities retain their compiler-assigned slots.
        for column in &self.schema.columns {
            if column.name.is_some()
                && !program.schema.columns.iter().any(|c| c.name == column.name)
            {
                program.schema.columns.push(column.clone());
            }
        }
        let seed = self.seed.as_ref().map(|row| {
            program
                .schema
                .columns
                .iter()
                .enumerate()
                .map(|(i, column)| {
                    let value = column
                        .name
                        .as_ref()
                        .and_then(|name| pattern::column_index(&self.schema, name))
                        .and_then(|index| row.get(index))
                        .cloned()
                        .unwrap_or(Value::Null);
                    // NULL is bound only for an actual incoming variable; newly exposed
                    // path slots are unbound, distinct from a questioned skip's NULL.
                    let incoming = self
                        .plan
                        .bindings
                        .get(i)
                        .is_none_or(|id| self.plan.input_bindings.contains(id));
                    (incoming || !matches!(value, Value::Null)).then_some(value)
                })
                .collect()
        });
        let qualify = |state: &state::SearchState, entity: &Value, phase| {
            conditions::evaluate(&program, state, entity, phase, &self.eval)
        };
        let limits = PathExecutionLimits {
            max_hops: PathExecutionLimits::default()
                .max_hops
                .min(self.eval.tx.impl_defined_caps().max_quantifier),
            ..Default::default()
        };
        let result = search::execute(&program, limits, ctx, Some(&qualify), seed)?;
        self.reserved = result.reserved;
        let projection = pattern::resolve_projection(result.table.schema(), &self.schema);
        let rows = result
            .table
            .rows()
            .iter()
            .map(|row| {
                pattern::project_row_with_projection(
                    row,
                    &self.schema,
                    &projection,
                    self.seed.as_ref(),
                )
            })
            .collect();
        self.source = Some(BatchRowSource::new(
            BindingTable::new(self.schema.clone(), rows),
            self.policy,
        ));
        self.source.as_mut().expect("source assigned").init(ctx)?;
        self.state = OperatorState::Open;
        Ok(())
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
            return Err(compile::invalid("path pull requires Open"));
        }
        let result = self
            .source
            .as_mut()
            .expect("Open owns source")
            .next_batch(ctx, buffer);
        match &result {
            Ok(None) => self.state = OperatorState::Exhausted,
            Err(_) => self.state = OperatorState::Failed,
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
        self.source = None;
        self.seed = None;
        self.state = OperatorState::Closed;
        ctx.close();
    }

    fn output_schema(&self) -> &BindingTableSchema {
        &self.schema
    }
}
