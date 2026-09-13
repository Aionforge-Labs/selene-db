//! Batch sources for optimizer disjunctions and projected pattern subplans.

use super::{
    binding_batch::{BatchBuffer, BindingBatch},
    join::reserve_rows,
    operator::{BatchExecutionContext, OperatorState, PhysicalOperator},
    policy::BatchPolicy,
};
use crate::{
    BindingTableSchema, JoinTree, PatternPlan,
    runtime::{Binding, EvalCtx, ExecutorError, pattern},
};
use rustc_hash::FxHashSet;
use selene_core::Value;

pub(super) struct BatchTreeSource<'a, 'ctx, 'g, 'p> {
    tree: &'p JoinTree,
    pattern: &'p PatternPlan,
    schema: BindingTableSchema,
    eval: EvalCtx<'a, 'ctx, 'g, 'p>,
    policy: BatchPolicy,
    seed: Option<Binding>,
    rows: Vec<Binding>,
    reserved: usize,
    cursor: usize,
    state: OperatorState,
}

impl<'a, 'ctx, 'g, 'p> BatchTreeSource<'a, 'ctx, 'g, 'p> {
    pub(super) fn new(
        tree: &'p JoinTree,
        pattern: &'p PatternPlan,
        schema: BindingTableSchema,
        eval: EvalCtx<'a, 'ctx, 'g, 'p>,
        policy: BatchPolicy,
        seed: Option<Binding>,
    ) -> Self {
        Self {
            tree,
            pattern,
            schema,
            eval,
            policy,
            seed,
            rows: Vec::new(),
            reserved: 0,
            cursor: 0,
            state: OperatorState::Created,
        }
    }

    fn push(
        &mut self,
        row: Binding,
        ctx: &mut BatchExecutionContext<'_>,
    ) -> Result<(), ExecutorError> {
        self.reserved += reserve_rows(
            ctx,
            1,
            self.schema.columns.len(),
            "batch tree source exceeds supported range",
        )?;
        self.rows.push(row);
        Ok(())
    }

    fn init_inner(&mut self, ctx: &mut BatchExecutionContext<'_>) -> Result<(), ExecutorError> {
        if self.state != OperatorState::Created {
            return Err(invalid_shape());
        }
        ctx.ensure_generation()?;
        ctx.check_cancel(crate::SourceSpan::default())?;
        match self.tree {
            JoinTree::DisjunctiveScan {
                branches,
                scan_anchor,
            } => {
                let anchor = scan_anchor
                    .binding
                    .and_then(|id| pattern::binding_index(self.pattern, &self.schema, id))
                    .or_else(|| {
                        scan_anchor
                            .hidden_binding
                            .and_then(|id| pattern::hidden_index(&self.schema, id))
                    })
                    .ok_or(ExecutorError::ImplementationDefined {
                        detail: "DisjunctiveScan anchor binding missing from pattern schema",
                    })?;
                let mut seen = FxHashSet::default();
                for scan in branches {
                    let tree = JoinTree::Scan(scan.clone());
                    let rows = super::tree::trace_subtree(
                        &tree,
                        self.pattern,
                        &self.schema,
                        self.seed.clone(),
                        self.eval,
                        self.policy,
                        ctx,
                    )?;
                    for row in rows {
                        ctx.check_cancel(crate::SourceSpan::default())?;
                        if let Some(Value::NodeRef(id)) = row.get(anchor)
                            && !seen.insert(*id)
                        {
                            continue;
                        }
                        self.push(row, ctx)?;
                    }
                }
            }
            JoinTree::Subplan(plan) => {
                if !plan.pipeline.is_empty() {
                    return Err(ExecutorError::ImplementationDefined {
                        detail: "Subplan pipeline ops not yet supported",
                    });
                }
                let pattern =
                    plan.pattern_plan
                        .as_ref()
                        .ok_or(ExecutorError::ImplementationDefined {
                            detail: "Subplan without pattern plan",
                        })?;
                let schema = pattern::schema_for_pattern(pattern);
                let eval = self.eval.with_plan(&plan.expr_ids, &plan.subqueries);
                let rows = super::tree::trace_subtree(
                    &pattern.join_tree,
                    pattern,
                    &schema,
                    self.seed.clone(),
                    eval,
                    self.policy,
                    ctx,
                )?;
                let projection = pattern::resolve_projection(&schema, &self.schema);
                for row in rows {
                    if pattern::filter_predicates_pass(
                        &pattern.filters,
                        pattern,
                        &row,
                        &schema,
                        &eval,
                    )? {
                        self.push(
                            pattern::project_row_with_projection(
                                &row,
                                &self.schema,
                                &projection,
                                self.seed.as_ref(),
                            ),
                            ctx,
                        )?;
                    }
                }
            }
            _ => return Err(invalid_shape()),
        }
        self.state = OperatorState::Open;
        Ok(())
    }
}

impl PhysicalOperator for BatchTreeSource<'_, '_, '_, '_> {
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
            return Err(invalid_shape());
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
            "batch tree source built a malformed batch",
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
        self.state = OperatorState::Closed;
    }
    fn output_schema(&self) -> &BindingTableSchema {
        &self.schema
    }
}

fn invalid_shape() -> ExecutorError {
    ExecutorError::ImplementationDefined {
        detail: "invalid physical tree source state or shape",
    }
}
