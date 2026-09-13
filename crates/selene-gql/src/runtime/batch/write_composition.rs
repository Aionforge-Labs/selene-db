//! Effectful compound-query barriers. Every arm re-enters the same batch driver.

use super::{budget::MemoryBudget, operator::BatchExecutionContext};
use crate::{
    PipelineOp, SetOp,
    runtime::{BindingTable, ExecutorError, TxContext, pipeline},
};

pub(super) fn execute(
    op: &PipelineOp,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    match op {
        PipelineOp::Chain(rhs) => super::query::execute(rhs, None, ctx),
        PipelineOp::CorrelatedChain(rhs) => {
            let (schema, rows) = table.into_parts();
            let mut output = Vec::new();
            for row in rows {
                ctx.check_cancellation()?;
                let result = super::query::execute(
                    rhs,
                    Some(BindingTable::new(schema.clone(), vec![row])),
                    ctx,
                )?;
                output.extend(result.into_parts().1);
            }
            Ok(BindingTable::new(rhs.output_schema.clone(), output))
        }
        PipelineOp::Union { op, rhs } => {
            let name = match op {
                SetOp::Union | SetOp::UnionAll => "UNION",
                SetOp::Intersect | SetOp::IntersectAll => "INTERSECT",
                SetOp::Except | SetOp::ExceptAll => "EXCEPT",
                SetOp::Otherwise => "OTHERWISE",
            };
            if *op == SetOp::Otherwise {
                pipeline::assert_compatible_schemas(name, table.schema(), &rhs.output_schema)?;
                if !table.is_empty() {
                    return Ok(table);
                }
            }
            let right = super::query::execute(rhs, None, ctx)?;
            pipeline::assert_compatible_schemas(name, table.schema(), right.schema())?;
            let (schema, left) = table.into_parts();
            let (_, right) = right.into_parts();
            if *op == SetOp::Otherwise {
                return Ok(BindingTable::new(schema, right));
            }
            let mut exec = BatchExecutionContext::borrowed(
                ctx.snapshot(),
                ctx.batch_cancel(),
                MemoryBudget::unlimited(),
            );
            let result =
                super::set::combine_set_rows(*op, left, &right, ctx.impl_defined_caps(), &mut exec);
            let rows = result.map(|(rows, reserved)| {
                exec.budget_mut().release(reserved);
                rows
            });
            exec.close();
            Ok(BindingTable::new(schema, rows?))
        }
        _ => Err(ExecutorError::ImplementationDefined {
            detail: "invalid effectful compound-query barrier",
        }),
    }
}
