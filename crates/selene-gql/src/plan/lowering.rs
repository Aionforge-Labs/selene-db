//! Physical planning for the single batch executor.
//!
//! Semantic → logical lowering is the only analysis route: [`plan_with_caps`]
//! first lowers through [`crate::plan::logical::lower_logical`], so every
//! supported family reaches logical planning and unsupported features fail
//! through the same profile authority with useful spans. Physical lowering
//! transports those fixed decisions into the optimizer's [`ExecutionPlan`].
//!
//! Source is borrowed solely for unchanged syntax payloads required by physical
//! operators (label predicates, property values, ordering of arms/blocks).
//! Resolved identities, types, effects, scopes, procedure metadata, path
//! automata, and grouping/ordering keys come from the frozen semantic tree
//! and the logical plan. Physical lowering never analyzes, never mutates syntax,
//! and never rederives a semantic decision. The optimizer IR is retained, not
//! translated into a second row plan: runtime assembly consumes it directly.

mod aggregate;
mod binding_refs;
mod bindings;
mod call;
mod catalog;
mod expr;
mod match_clause;
mod mutation;
mod optional_filters;
pub(crate) mod path_program;
mod query;
#[cfg(test)]
mod rejections;
mod sequential_match;
mod set_op;

use query::{lower_query_pipeline, lower_return, nullable_call_yield_type, visible_after_pattern};
use set_op::assert_arms_column_name_equal;

/// Immutable logical path authority shared by every nested physical lowerer.
#[derive(Clone, Copy)]
pub(crate) struct PathLowering<'a> {
    paths: &'a crate::LoweredPathSet,
    max_quantifier: u32,
}

use crate::{
    GqlType, ProcedureRegistry, QueryPipeline, SourceSpan, Statement,
    analyze::{AnalyzedStatement, AnalyzedType, ExprId, StatementCategory},
    plan::{
        BindingTableColumn, BindingTableSchema, ExecutionPlan, ImplDefinedCaps, PipelineOp,
        PlannerError, SessionOp, TxOp,
    },
};

/// Lower an analyzed statement into a literal, unoptimized execution plan
/// using the default implementation-defined caps ([`ImplDefinedCaps::DEFAULT`]).
///
/// Thin wrapper over [`plan_with_caps`]; call that to inject caller-configured
/// caps (see [`crate::runtime::Session::with_impl_defined_caps`]).
pub fn plan(
    analyzed: &AnalyzedStatement,
    registry: &dyn ProcedureRegistry,
) -> Result<ExecutionPlan, PlannerError> {
    plan_with_caps(analyzed, registry, &ImplDefinedCaps::DEFAULT)
}

/// Lower an analyzed statement into a literal, unoptimized execution plan,
/// stamping the caller-supplied implementation-defined caps.
///
/// Single-path entry: semantic → logical lowering runs first, so every
/// supported family is fixed in logical IR before physical lowering runs.
/// Physical lowering transports those decisions into [`ExecutionPlan`]:
/// queries / set-composed / NEXT-chained pipelines walk the read pipeline;
/// mutations lower from the analyzer's [`MutationWriteSet`]; DDL lowers to a
/// single [`PipelineOp::Catalog`]; transaction control lowers to a single
/// [`PipelineOp::Tx`]; top-level CALL looks up procedure metadata in
/// `registry` and lowers to [`PipelineOp::Call`].
///
/// `caps` reach the plan two ways: the plan-time variable-length quantifier gate
/// consults `caps.max_quantifier` *during* lowering (threaded as `max_quantifier`),
/// and the finished top-level plan carries `*caps` in `impl_defined_caps` for the
/// runtime context and optimizer. Nested plans (set-op / NEXT / CALL-subquery
/// bodies) execute under the parent context, so only the top-level stamp is read.
///
/// [`MutationWriteSet`]: crate::analyze::MutationWriteSet
#[tracing::instrument(
    name = "selene.gql.plan",
    skip(analyzed, registry, caps),
    fields(category = ?analyzed.category)
)]
pub fn plan_with_caps(
    analyzed: &AnalyzedStatement,
    registry: &dyn ProcedureRegistry,
    caps: &ImplDefinedCaps,
) -> Result<ExecutionPlan, PlannerError> {
    // Single-path gate: every statement reaches logical planning first.
    // Unsupported features fail here through the same profile authority with
    // useful spans; supported families fix their identities, types, effects,
    // scopes, and path automata in logical IR before physical lowering runs.
    let logical = crate::plan::logical::lower_logical(analyzed, registry)?;
    let paths = PathLowering {
        paths: &logical.paths,
        max_quantifier: caps.max_quantifier,
    };
    let mut plan = lower_statement_kind(analyzed.source(), registry, analyzed, caps, paths)?;
    plan.category = analyzed.category;
    plan.expr_ids = analyzed.expr_ids.clone();
    expr::populate_plan_subqueries(&mut plan, analyzed, registry, paths)?;
    // Why: only the top-level plan's caps are consumed — statement.rs builds the
    // runtime TxContext and the optimizer's OptimizeContext from
    // `plan.impl_defined_caps`, and nested plans execute under that same context.
    // The one cap needed mid-lowering (the quantifier gate) is threaded above as
    // `max_quantifier`, so a post-lowering stamp here covers every statement kind.
    plan.impl_defined_caps = *caps;
    plan.refresh_pipeline_op_high_water();
    // Verify that the physical plan transports the logical decisions: the
    // metadata-resolved effects must agree with both the semantic summary
    // and the logical plan. A parser-only check would miss an effectful
    // nested CALL; this check resolves every planned call from registration
    // metadata and enforces the GP18 no-mix policy before the facade can
    // route the plan to execution.
    let semantic = crate::plan::logical::classify_analyzed(analyzed);
    let physical_summary = crate::plan::logical::verify_plan_effects(&semantic, &plan)?;
    if physical_summary.effect != logical.effects.effect
        || physical_summary.has_data_write != logical.effects.has_data_write
        || physical_summary.has_catalog_write != logical.effects.has_catalog_write
        || physical_summary.has_maintenance_write != logical.effects.has_maintenance_write
    {
        return Err(PlannerError::EffectMismatch {
            detail: "physical plan effects disagree with the logical plan",
            span: physical_summary.origin,
        });
    }
    Ok(plan)
}

fn lower_statement_kind(
    statement: &Statement,
    registry: &dyn ProcedureRegistry,
    analyzed: &AnalyzedStatement,
    caps: &ImplDefinedCaps,
    paths: PathLowering<'_>,
) -> Result<ExecutionPlan, PlannerError> {
    // The quantifier gate is the only cap threaded recursively mid-lowering; the
    // DDL key-label-set IL003 gate needs the full caps, so `lower_ddl` receives
    // `caps` directly.
    let max_quantifier = paths;
    match statement {
        Statement::Query(pipeline) => {
            lower_query_pipeline(pipeline, registry, analyzed, max_quantifier)
        }
        Statement::Composite { first, rest, .. } => {
            let mut plan = lower_query_pipeline(first, registry, analyzed, max_quantifier)?;
            for (op, rhs) in rest {
                let rhs_plan = lower_query_pipeline(rhs, registry, analyzed, max_quantifier)?;
                // ISO §14.2 SR v: set-composition arms must be column
                // name-equal. The binder binds each arm independently, so the
                // names are first available here on the lowered output schemas.
                assert_arms_column_name_equal(
                    *op,
                    &plan.output_schema,
                    &rhs_plan.output_schema,
                    rhs.span,
                )?;
                plan.pipeline.push(PipelineOp::Union {
                    op: *op,
                    rhs: Box::new(rhs_plan),
                });
            }
            Ok(plan)
        }
        Statement::Chained { blocks, .. } => {
            lower_chained(blocks, registry, analyzed, max_quantifier)
        }
        Statement::Mutate(pipeline) => mutation::lower_mutation(pipeline, analyzed, max_quantifier),
        Statement::Ddl(statement) => catalog::lower_ddl(statement, analyzed, caps),
        Statement::Call(call) => call::lower_top_level_call(call, registry, analyzed),
        Statement::Explain { inner, span } => {
            lower_explain(inner, *span, registry, analyzed, caps, paths)
        }
        Statement::StartTransaction { span } => Ok(tx_plan(TxOp::Start { span: *span })),
        Statement::Commit { span } => Ok(tx_plan(TxOp::Commit { span: *span })),
        Statement::Rollback { span } => Ok(tx_plan(TxOp::Rollback { span: *span })),
        Statement::SessionSetValue {
            param,
            declared_type,
            value,
            if_not_exists,
            span,
        } => Ok(session_plan(SessionOp::SetValue {
            param: param.clone(),
            declared_type: declared_type.clone(),
            value: value.clone(),
            if_not_exists: *if_not_exists,
            span: *span,
        })),
        Statement::SessionSetTimeZone { zone, span, .. } => {
            Ok(session_plan(SessionOp::SetTimeZone {
                zone: zone.clone(),
                span: *span,
            }))
        }
        Statement::SessionSetGraph { target, span } => {
            if let crate::SessionSetGraphTarget::SchemaReference(reference) = target {
                return Ok(session_plan(SessionOp::SetSchema {
                    reference: reference.clone(),
                    span: *span,
                }));
            }
            Ok(session_plan(SessionOp::SetGraph {
                target: target.clone(),
                span: *span,
            }))
        }
        Statement::SessionReset { target, span } => {
            Ok(session_plan(session_reset_op(target, *span)))
        }
        Statement::SessionClose { span } => Ok(session_plan(SessionOp::Close { span: *span })),
    }
}

fn session_reset_op(target: &crate::SessionResetTarget, span: SourceSpan) -> SessionOp {
    use crate::SessionResetTarget;
    match target {
        SessionResetTarget::AllCharacteristics => SessionOp::ResetAllCharacteristics { span },
        SessionResetTarget::Schema => SessionOp::ResetSchema { span },
        SessionResetTarget::Graph => SessionOp::ResetGraph { span },
        SessionResetTarget::Parameters => SessionOp::ResetParameters { span },
        SessionResetTarget::TimeZone => SessionOp::ResetTimeZone { span },
        SessionResetTarget::Parameter(param) => SessionOp::ResetParameter {
            param: param.clone(),
            span,
        },
    }
}

fn lower_explain(
    inner: &Statement,
    span: SourceSpan,
    registry: &dyn ProcedureRegistry,
    analyzed: &AnalyzedStatement,
    caps: &ImplDefinedCaps,
    paths: PathLowering<'_>,
) -> Result<ExecutionPlan, PlannerError> {
    let inner = lower_statement_kind(inner, registry, analyzed, caps, paths)?;
    Ok(ExecutionPlan {
        category: StatementCategory::ReadOnly,
        pattern_plan: None,
        pipeline: vec![PipelineOp::ExplainPlan {
            inner: Box::new(inner),
            span,
        }],
        output_schema: explain_output_schema(span)?,
        impl_defined_caps: ImplDefinedCaps::default(),
        expr_ids: analyzed.expr_ids.clone(),
        subqueries: Default::default(),
        next_expr_id: next_expr_id(analyzed),
        next_pipeline_op_id: crate::PipelineOpId::new(1),
    })
}

fn lower_chained(
    blocks: &[QueryPipeline],
    registry: &dyn ProcedureRegistry,
    analyzed: &AnalyzedStatement,
    max_quantifier: PathLowering<'_>,
) -> Result<ExecutionPlan, PlannerError> {
    let Some((first, rest)) = blocks.split_first() else {
        return Ok(empty_plan());
    };
    let mut plan = lower_query_pipeline(first, registry, analyzed, max_quantifier)?;
    // NEXT's output_schema must reflect the final block's projection because
    // each NEXT discards the prior block's columns.
    for block in rest {
        let correlated = block_references_prior_bindings(block.span, analyzed);
        let inner = lower_query_pipeline(block, registry, analyzed, max_quantifier)?;
        plan.output_schema = inner.output_schema.clone();
        let inner = Box::new(inner);
        if correlated {
            plan.pipeline.push(PipelineOp::CorrelatedChain(inner));
        } else {
            plan.pipeline.push(PipelineOp::Chain(inner));
        }
    }
    Ok(plan)
}

fn block_references_prior_bindings(block_span: SourceSpan, analyzed: &AnalyzedStatement) -> bool {
    for reference in &analyzed.references {
        if !span_contains(block_span, reference.span) {
            continue;
        }
        let Some(decl) = analyzed.scopes.declaration(reference.binding) else {
            continue;
        };
        if !span_contains(block_span, decl.span()) {
            return true;
        }
    }
    false
}

fn span_contains(outer: SourceSpan, inner: SourceSpan) -> bool {
    outer.byte_offset <= inner.byte_offset && inner.end() <= outer.end()
}

fn explain_output_schema(span: SourceSpan) -> Result<BindingTableSchema, PlannerError> {
    let name = selene_core::db_string("plan").map_err(|_err| {
        PlannerError::StaticStringConstructionFailed {
            detail: "static EXPLAIN column 'plan'",
            span,
        }
    })?;
    Ok(BindingTableSchema {
        columns: vec![BindingTableColumn {
            name: Some(name),
            hidden: None,
            ty: AnalyzedType::Resolved(GqlType::String),
        }],
    })
}

fn empty_plan() -> ExecutionPlan {
    ExecutionPlan {
        category: StatementCategory::ReadOnly,
        pattern_plan: None,
        pipeline: Vec::new(),
        output_schema: BindingTableSchema {
            columns: Vec::new(),
        },
        impl_defined_caps: ImplDefinedCaps::default(),
        expr_ids: Default::default(),
        subqueries: Default::default(),
        next_expr_id: ExprId::new(0),
        next_pipeline_op_id: crate::PipelineOpId::new(0),
    }
}

fn tx_plan(op: TxOp) -> ExecutionPlan {
    ExecutionPlan {
        category: StatementCategory::TransactionControl,
        pattern_plan: None,
        pipeline: vec![PipelineOp::Tx(op)],
        output_schema: BindingTableSchema {
            columns: Vec::new(),
        },
        impl_defined_caps: ImplDefinedCaps::default(),
        expr_ids: Default::default(),
        subqueries: Default::default(),
        next_expr_id: ExprId::new(0),
        next_pipeline_op_id: crate::PipelineOpId::new(1),
    }
}

fn session_plan(op: SessionOp) -> ExecutionPlan {
    ExecutionPlan {
        category: StatementCategory::SessionControl,
        pattern_plan: None,
        pipeline: vec![PipelineOp::Session(op)],
        output_schema: BindingTableSchema {
            columns: Vec::new(),
        },
        impl_defined_caps: ImplDefinedCaps::default(),
        expr_ids: Default::default(),
        subqueries: Default::default(),
        next_expr_id: ExprId::new(0),
        next_pipeline_op_id: crate::PipelineOpId::new(1),
    }
}

pub(super) fn next_expr_id(analyzed: &AnalyzedStatement) -> ExprId {
    ExprId::new(analyzed.expr_types.len() as u32)
}

#[cfg(test)]
mod defensive_tests {
    use super::*;
    use crate::{
        EmptyProcedureRegistry,
        analyze::{BindingScopeTree, ExprIdLookup},
        parse,
    };

    #[test]
    fn missing_expression_type_reports_planner_error() {
        let mut statement =
            crate::analyze(parse("RETURN 1").unwrap(), &EmptyProcedureRegistry, None).unwrap();
        statement.corrupt_for_test(|_, semantic| semantic.expr_ids = ExprIdLookup::default());
        let err = plan(&statement, &EmptyProcedureRegistry).expect_err("missing expr cell");
        assert!(matches!(err, PlannerError::ExpressionTypeMissing { .. }));
    }

    #[test]
    fn lost_binding_reference_reports_planner_error() {
        let mut statement = crate::analyze(
            parse("LET n = 1 RETURN n").unwrap(),
            &EmptyProcedureRegistry,
            None,
        )
        .unwrap();
        statement
            .corrupt_for_test(|_, semantic| semantic.scopes = BindingScopeTree::new(semantic.span));
        let err = plan(&statement, &EmptyProcedureRegistry).expect_err("lost binding");
        assert!(matches!(err, PlannerError::BindingResolutionLost { .. }));
    }
}
