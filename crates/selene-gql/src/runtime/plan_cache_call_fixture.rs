//! Synthetic CALL fixture, split to keep the cache module below the source cap.

use crate::{
    BindingTableSchema, ExecutionPlan, ExprId, ImplDefinedCaps, PipelineOp, PipelineOpId,
    PlannedCall, ProcedureHandle, ProcedureMetadata, ProcedureMutability, ProcedureOutputSchema,
    ProcedureTier, SourceSpan, StatementCategory,
};
use std::sync::Arc;

pub(super) fn call_plan() -> Arc<ExecutionPlan> {
    Arc::new(ExecutionPlan {
        category: StatementCategory::ReadOnly,
        pattern_plan: None,
        pipeline: vec![PipelineOp::Call(PlannedCall {
            registry_version: 0,
            metadata: ProcedureMetadata::new(
                ProcedureHandle::new(1),
                Default::default(),
                Default::default(),
                ProcedureTier::Graph,
                ProcedureMutability::Read,
            ),
            optional: false,
            procedure: Box::from([
                selene_core::db_string("cache").unwrap(),
                selene_core::db_string("call").unwrap(),
            ]),
            handle: ProcedureHandle::new(1),
            args: Vec::new(),
            yield_cols: Vec::new(),
            output_schema: ProcedureOutputSchema::default(),
            yield_schema: Vec::new(),
            tier: ProcedureTier::Graph,
            mutability: ProcedureMutability::Read,
            span: SourceSpan::default(),
        })],
        output_schema: BindingTableSchema {
            columns: Vec::new(),
        },
        impl_defined_caps: ImplDefinedCaps::default(),
        expr_ids: Default::default(),
        subqueries: Default::default(),
        next_expr_id: ExprId::new(0),
        next_pipeline_op_id: PipelineOpId::new(1),
    })
}
