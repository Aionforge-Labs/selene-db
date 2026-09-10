//! Derive result metadata from the actual final ordering operator, even when
//! execution produces no rows. This is not inferred from observed row order.

use crate::{
    BindingTableSchema, ExecutionPlan, NullsPolicy, OrderDirection, PipelineOp, ValueExpr,
};
use selene_core::{NullPlacement, ResultOrderKey, SortDirection};

pub(super) fn for_plan(plan: &ExecutionPlan, schema: &BindingTableSchema) -> Vec<ResultOrderKey> {
    for op in plan.pipeline.iter().rev() {
        let keys = match op {
            PipelineOp::OrderBy(keys) | PipelineOp::TopK { keys, .. } => keys,
            PipelineOp::Limit { .. }
            | PipelineOp::TrimOrderCarriers { .. }
            | PipelineOp::Filter(_) => continue,
            PipelineOp::Chain(next) => return for_plan(next, schema),
            // Other operators do not promise to preserve one global ordering.
            _ => return Vec::new(),
        };
        return keys
            .iter()
            .map(|key| {
                let column = match &key.expr {
                    ValueExpr::Variable { name, .. } => schema
                        .columns
                        .iter()
                        .position(|column| column.name.as_ref() == Some(name)),
                    _ => None,
                };
                let direction = match key.direction {
                    OrderDirection::Asc => SortDirection::Ascending,
                    OrderDirection::Desc => SortDirection::Descending,
                };
                let nulls = match (key.nulls, key.direction) {
                    (Some(NullsPolicy::NullsFirst), _) | (None, OrderDirection::Desc) => {
                        NullPlacement::First
                    }
                    (Some(NullsPolicy::NullsLast), _) | (None, OrderDirection::Asc) => {
                        NullPlacement::Last
                    }
                };
                ResultOrderKey::new(
                    column,
                    crate::ast::format::format_value_expr(&key.expr),
                    direction,
                    nulls,
                )
            })
            .collect();
    }
    Vec::new()
}
