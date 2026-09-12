//! Execution-plan container and optimizer bookkeeping.

use crate::analyze::{ExprId, ExprIdLookup, StatementCategory};

use super::{
    BindingTableSchema, ImplDefinedCaps, JoinTree, PatternPlan, PipelineOp, SubqueryRegistry,
};

/// Identifier for a pipeline op within one execution plan.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct PipelineOpId(u32);

impl PipelineOpId {
    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Return the zero-based op identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Literal execution plan produced by planner lowering.
#[derive(Clone, Debug)]
pub struct ExecutionPlan {
    /// Analyzer-derived statement category used by top-level execution.
    pub category: StatementCategory,
    /// Optional leading pattern plan for query pipelines beginning with MATCH.
    pub pattern_plan: Option<PatternPlan>,
    /// Binding-table operations after the leading pattern phase.
    pub pipeline: Vec<PipelineOp>,
    /// Columns exposed by the final pipeline boundary.
    pub output_schema: BindingTableSchema,
    /// Planner implementation-defined limits.
    pub impl_defined_caps: ImplDefinedCaps,
    /// Analyzer expression lookup cloned into the executable plan.
    pub expr_ids: ExprIdLookup,
    /// Planned expression subqueries indexed by their analyzer expression ID.
    pub subqueries: SubqueryRegistry,
    /// Next optimizer-owned expression ID for this plan.
    pub next_expr_id: ExprId,
    /// Next executor-observable pipeline op ID for this plan.
    pub next_pipeline_op_id: PipelineOpId,
}

impl ExecutionPlan {
    /// Allocate a fresh expression ID for optimizer-synthesized expressions.
    pub(crate) fn alloc_expr_id(&mut self) -> ExprId {
        let id = self.next_expr_id;
        self.next_expr_id = ExprId::new(id.get().saturating_add(1));
        id
    }

    /// Recompute the pipeline-op ID high-water mark after lowering or rewrite.
    pub(crate) fn refresh_pipeline_op_high_water(&mut self) {
        if let Some(pattern) = &mut self.pattern_plan {
            refresh_join_tree_pipeline_op_high_water(&mut pattern.join_tree);
        }
        for op in &mut self.pipeline {
            match op {
                PipelineOp::Union { rhs, .. }
                | PipelineOp::Chain(rhs)
                | PipelineOp::CorrelatedChain(rhs) => {
                    rhs.refresh_pipeline_op_high_water();
                }
                PipelineOp::CallSubquery(subquery) => {
                    subquery.body.refresh_pipeline_op_high_water();
                }
                PipelineOp::ExplainPlan { inner, .. } => inner.refresh_pipeline_op_high_water(),
                _ => {}
            }
        }
        // Today pipeline ops do not carry stable IDs, so the next ID is len().
        // When ops gain stored IDs, this must scan for max(existing_id) + 1.
        self.next_pipeline_op_id = PipelineOpId::new(self.pipeline.len() as u32);
    }
}

fn refresh_join_tree_pipeline_op_high_water(tree: &mut JoinTree) {
    match tree {
        JoinTree::Unit | JoinTree::Scan(_) | JoinTree::Paths(_) => {}
        JoinTree::Expand { child, .. } => refresh_join_tree_pipeline_op_high_water(child),
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            refresh_join_tree_pipeline_op_high_water(left);
            refresh_join_tree_pipeline_op_high_water(right);
        }
        JoinTree::WorstCaseOptimal { intersection, .. } => {
            for branch in intersection {
                refresh_join_tree_pipeline_op_high_water(branch);
            }
        }
        JoinTree::Subplan(plan) => plan.refresh_pipeline_op_high_water(),
        // DisjunctiveScan branches are NodeOrEdgeScans, which never carry
        // nested pipeline ops; same shape as the `Scan(_)` arm.
        JoinTree::DisjunctiveScan { .. } => {}
    }
}
