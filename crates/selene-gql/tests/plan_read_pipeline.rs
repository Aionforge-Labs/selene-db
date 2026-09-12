//! BRIEF-26 read-side planner lowering tests.

use selene_gql::{
    AnalyzedStatement, AnalyzedType, BindingElement, EdgeDirection, EmptyProcedureRegistry,
    FilterPredicateKind, GqlType, JoinTree, LabelExpr, LimitAmount, PipelineOp, PlannerError,
    ScanKind, SetOp, SubqueryBody, SubqueryKind, analyze, parse, plan,
};

fn analyze_one(source: &str) -> AnalyzedStatement {
    let statement = parse(source).expect("test input parses");
    analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes")
}

fn plan_one(source: &str) -> selene_gql::ExecutionPlan {
    let analyzed = analyze_one(source);
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

fn plan_err(source: &str) -> PlannerError {
    let analyzed = analyze_one(source);
    plan(&analyzed, &EmptyProcedureRegistry).expect_err("test input should fail planning")
}

fn variant_names(plan: &selene_gql::ExecutionPlan) -> Vec<&'static str> {
    plan.pipeline
        .iter()
        .map(|op| match op {
            PipelineOp::Filter(_) => "Filter",
            PipelineOp::Project(_) => "Project",
            PipelineOp::Let(_) => "Let",
            PipelineOp::Unwind { .. } => "Unwind",
            PipelineOp::OrderBy(_) => "OrderBy",
            PipelineOp::Limit { .. } => "Limit",
            PipelineOp::TopK { .. } => "TopK",
            PipelineOp::GroupBy { .. } => "GroupBy",
            PipelineOp::Distinct => "Distinct",
            PipelineOp::Union { .. } => "Union",
            PipelineOp::Chain(_) => "Chain",
            PipelineOp::CorrelatedChain(_) => "CorrelatedChain",
            PipelineOp::OptionalMatch(_) => "OptionalMatch",
            PipelineOp::Call(_) => "Call",
            PipelineOp::Mutation(_) => "Mutation",
            PipelineOp::Catalog(_) => "Catalog",
            PipelineOp::Tx(_) => "Tx",
            _ => "Unknown",
        })
        .collect()
}

fn expand(plan: &selene_gql::ExecutionPlan) -> (&selene_gql::EdgeMatch, EdgeDirection) {
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    match &pattern.join_tree {
        JoinTree::Expand {
            edge, direction, ..
        } => (edge, *direction),
        other => panic!("expected expand, got {other:?}"),
    }
}

fn path_program(plan: &selene_gql::ExecutionPlan) -> &selene_gql::PathProgram {
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    match &pattern.join_tree {
        JoinTree::Paths(program) => program,
        other => panic!("expected product paths, got {other:?}"),
    }
}

#[path = "plan_read_pipeline/pattern.rs"]
mod pattern;
#[path = "plan_read_pipeline/pipeline.rs"]
mod pipeline;
#[path = "plan_read_pipeline/regressions.rs"]
mod regressions;
