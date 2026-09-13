//! Assemble physical read segments; only explicit effect/control barriers split them.

use super::{operator::PhysicalOperator, policy::BatchPolicy};
use crate::{
    PipelineOp,
    runtime::{EvalCtx, ExecutorError, pipeline},
};

pub(super) fn build<'e, 'a: 'e, 'ctx: 'e, 'g: 'e, 'p: 'e>(
    mut root: Box<dyn PhysicalOperator + 'e>,
    ops: &'p [PipelineOp],
    eval: EvalCtx<'a, 'ctx, 'g, 'p>,
    policy: BatchPolicy,
) -> Result<(Box<dyn PhysicalOperator + 'e>, usize), ExecutorError> {
    for (index, op) in ops.iter().enumerate() {
        if let PipelineOp::Union { rhs, .. }
        | PipelineOp::Chain(rhs)
        | PipelineOp::CorrelatedChain(rhs) = op
            && crate::plan::classify_plan(rhs).effect != crate::plan::LogicalEffect::Query
        {
            return Ok((root, index));
        }
        root = match op {
            PipelineOp::Filter(predicate) => {
                Box::new(super::filter::BatchFilter::new(root, predicate, eval))
            }
            PipelineOp::Project(items) => Box::new(super::project::BatchProject::new(
                root,
                items,
                pipeline::schema_for_items(items),
                eval,
            )),
            PipelineOp::Let(_) | PipelineOp::Unwind { .. } | PipelineOp::CallSubquery(_) => {
                Box::new(super::extend::BatchExtend::new(root, op, eval, policy)?)
            }
            PipelineOp::Limit { offset, count } => Box::new(super::page::BatchPage::for_pipeline(
                root,
                pipeline::resolve_amount(offset, eval.tx)?,
                pipeline::resolve_amount(count, eval.tx)?,
            )),
            PipelineOp::GroupBy { keys, aggregates } => Box::new(
                super::aggregate::BatchGroupBy::new(root, keys, aggregates, eval, policy),
            ),
            PipelineOp::OrderBy(keys) => {
                Box::new(super::sort::BatchSort::new(root, keys, eval, policy))
            }
            PipelineOp::TopK {
                keys,
                offset,
                count,
            } => {
                let (offset, count) = super::sort::resolve_top_k_window(offset, count, eval.tx)?;
                Box::new(super::sort::BatchTopK::new(
                    root, keys, offset, count, eval, policy,
                ))
            }
            PipelineOp::Distinct => Box::new(super::distinct::BatchDistinct::new(root, policy)),
            PipelineOp::TrimOrderCarriers { projected_width } => {
                Box::new(super::sort::BatchTrimCarriers::new(root, *projected_width))
            }
            PipelineOp::Match(pattern) | PipelineOp::OptionalMatch(pattern) => {
                let input = root.output_schema().clone();
                let target = pipeline::target_schema(&input, pattern);
                Box::new(super::chain::BatchMatch::new(
                    root,
                    pattern,
                    target,
                    input,
                    matches!(op, PipelineOp::OptionalMatch(_)),
                    eval,
                    policy,
                ))
            }
            PipelineOp::Union { op, rhs } => {
                let schema = root.output_schema().clone();
                Box::new(super::set::BatchSet::new(
                    root, *op, rhs, schema, eval, policy,
                ))
            }
            PipelineOp::Chain(rhs) => {
                Box::new(super::chain::BatchChain::new(root, rhs, eval, policy))
            }
            PipelineOp::CorrelatedChain(rhs) => Box::new(super::chain::BatchCorrelatedChain::new(
                root, rhs, eval, policy,
            )),
            PipelineOp::Call(call)
                if call.tier == crate::ProcedureTier::Graph
                    && call.mutability == crate::ProcedureMutability::Read =>
            {
                pipeline::call::validate_registration(call, eval.tx)?;
                Box::new(super::call::BatchCall::new(root, call, eval, policy))
            }
            PipelineOp::Call(_)
            | PipelineOp::Mutation(_)
            | PipelineOp::Catalog(_)
            | PipelineOp::Tx(_)
            | PipelineOp::Session(_)
            | PipelineOp::ExplainPlan { .. } => return Ok((root, index)),
        };
    }
    Ok((root, ops.len()))
}
