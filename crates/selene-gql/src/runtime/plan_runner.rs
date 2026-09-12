//! Standalone execution-plan runner used by recursive runtime operators.

use crate::{
    BindingElement, BindingTableSchema, ExecutionPlan, LimitAmount, PatternPlan, PipelineOp,
    ProjectExpr, ValueExpr,
    runtime::{Binding, BindingTable, EvalCtx, ExecutorError, TxContext, pattern, pipeline},
};

pub(crate) fn execute_plan(
    plan: &ExecutionPlan,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    execute_plan_with_seed(plan, None, ctx)
}

pub(crate) fn execute_plan_with_seed(
    plan: &ExecutionPlan,
    seed: Option<BindingTable>,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    // F04-PR02 production seam: unseeded plans whose pattern and leading
    // pipeline prefix fall in the batch families run through the batch
    // driver. The driver declines (`Ok(None)`) anything outside those
    // families — including every seeded (correlated) execution — and the row
    // path below runs unchanged. Errors never fall back: a batch failure
    // aborts with the row path's diagnostics.
    let batch_prefix = if seed.is_none() {
        super::batch::query::try_execute_prefix(plan, ctx)?
    } else {
        None
    };
    if let Some(prefix) = batch_prefix {
        let remaining = &plan.pipeline[prefix.suffix_from..];
        if remaining.is_empty() {
            return Ok(prefix.table);
        }
        return pipeline::execute_pipeline_with_plan(
            remaining,
            prefix.table,
            ctx,
            &plan.expr_ids,
            &plan.subqueries,
        );
    }
    let row_limit = pattern_row_limit(plan);
    let table = {
        let eval_ctx = EvalCtx {
            tx: ctx,
            expr_ids: &plan.expr_ids,
            subqueries: &plan.subqueries,
        };
        match (&plan.pattern_plan, seed) {
            (Some(pattern_plan), Some(seed)) => {
                let (schema, rows) = seed.into_parts();
                let Some(row) = rows.first() else {
                    return Ok(BindingTable::new(schema, Vec::new()));
                };
                let target_schema = target_schema(&schema, pattern_plan);
                pattern::execute_pattern_with_seed_schema_and_limit(
                    pattern_plan,
                    Some(row),
                    target_schema,
                    &eval_ctx,
                    row_limit,
                )?
            }
            (Some(pattern_plan), None) => {
                let schema = pattern::schema_for_pattern(pattern_plan);
                pattern::execute_pattern_with_seed_schema_and_limit(
                    pattern_plan,
                    None,
                    schema,
                    &eval_ctx,
                    row_limit,
                )?
            }
            (None, Some(seed)) => seed,
            (None, None) => seed_table(),
        }
    };
    pipeline::execute_pipeline_with_plan(
        plan.pipeline.as_slice(),
        table,
        ctx,
        &plan.expr_ids,
        &plan.subqueries,
    )
}

pub(crate) fn execute_plan_read_only(
    plan: &ExecutionPlan,
    ctx: &TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    execute_plan_read_only_with_seed(plan, None, ctx)
}

pub(crate) fn execute_plan_read_only_with_seed(
    plan: &ExecutionPlan,
    seed: Option<BindingTable>,
    ctx: &TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    // Same batch seam as the read-write path, resuming through the read-only
    // dispatcher so write-bearing suffix operators keep their read-only
    // rejection diagnostics.
    let batch_prefix = if seed.is_none() {
        super::batch::query::try_execute_prefix(plan, ctx)?
    } else {
        None
    };
    if let Some(prefix) = batch_prefix {
        let remaining = &plan.pipeline[prefix.suffix_from..];
        if remaining.is_empty() {
            return Ok(prefix.table);
        }
        return pipeline::execute_pipeline_read_only_with_plan(
            remaining,
            prefix.table,
            ctx,
            &plan.expr_ids,
            &plan.subqueries,
        );
    }
    let row_limit = pattern_row_limit(plan);
    let table = {
        let eval_ctx = EvalCtx {
            tx: ctx,
            expr_ids: &plan.expr_ids,
            subqueries: &plan.subqueries,
        };
        match (&plan.pattern_plan, seed) {
            (Some(pattern_plan), Some(seed)) => {
                let (schema, rows) = seed.into_parts();
                let Some(row) = rows.first() else {
                    return Ok(BindingTable::new(schema, Vec::new()));
                };
                let target_schema = target_schema(&schema, pattern_plan);
                pattern::execute_pattern_with_seed_schema_and_limit(
                    pattern_plan,
                    Some(row),
                    target_schema,
                    &eval_ctx,
                    row_limit,
                )?
            }
            (Some(pattern_plan), None) => {
                let schema = pattern::schema_for_pattern(pattern_plan);
                pattern::execute_pattern_with_seed_schema_and_limit(
                    pattern_plan,
                    None,
                    schema,
                    &eval_ctx,
                    row_limit,
                )?
            }
            (None, Some(seed)) => seed,
            (None, None) => seed_table(),
        }
    };
    pipeline::execute_pipeline_read_only_with_plan(
        plan.pipeline.as_slice(),
        table,
        ctx,
        &plan.expr_ids,
        &plan.subqueries,
    )
}

pub(crate) fn seed_table() -> BindingTable {
    BindingTable::new(
        BindingTableSchema {
            columns: Vec::new(),
        },
        vec![Binding::empty()],
    )
}

/// Extend an input schema with a pattern's new columns for seeded execution.
///
/// Shared with the batch seeded-subplan helper so both paths seed into
/// identical target schemas.
pub(crate) fn target_schema(
    input: &BindingTableSchema,
    pattern_plan: &crate::PatternPlan,
) -> BindingTableSchema {
    let mut schema = input.clone();
    for column in pattern::schema_for_pattern(pattern_plan).columns {
        if column_exists(&schema, &column) {
            continue;
        }
        schema.columns.push(column);
    }
    schema
}

fn column_exists(schema: &BindingTableSchema, column: &crate::BindingTableColumn) -> bool {
    match (column.name.clone(), column.hidden) {
        (Some(name), _) => schema
            .columns
            .iter()
            .any(|candidate| candidate.name == Some(name.clone())),
        (None, Some(hidden)) => schema
            .columns
            .iter()
            .any(|candidate| candidate.hidden == Some(hidden)),
        (None, None) => false,
    }
}

/// Proven-safe pattern row limit shared with the batch driver.
///
/// The batch query driver truncates its pattern phase with exactly this
/// bound (and only this bound): replicating the row path's pushdown cases
/// exactly keeps `LIMIT` queries observably identical, while never pushing a
/// limit below an operator the row path left untruncated. See the row-limit
/// helpers below for the safety cases.
pub(crate) fn pattern_row_limit(plan: &ExecutionPlan) -> Option<usize> {
    leading_pattern_row_limit(plan.pipeline.as_slice()).or_else(|| {
        plan.pattern_plan
            .as_ref()
            .and_then(|pattern| post_return_project_row_limit(pattern, plan.pipeline.as_slice()))
    })
}

fn leading_pattern_row_limit(pipeline: &[PipelineOp]) -> Option<usize> {
    let Some(PipelineOp::Limit { offset, count }) = pipeline.first() else {
        return None;
    };
    literal_row_limit(offset, count)
}

fn post_return_project_row_limit(pattern: &PatternPlan, pipeline: &[PipelineOp]) -> Option<usize> {
    let [
        PipelineOp::Project(items),
        PipelineOp::Limit { offset, count },
        ..,
    ] = pipeline
    else {
        return None;
    };
    if !items
        .iter()
        .all(|item| project_preserves_row_limit_safety(pattern, item))
    {
        return None;
    }
    literal_row_limit(offset, count)
}

fn literal_row_limit(offset: &LimitAmount, count: &LimitAmount) -> Option<usize> {
    let (LimitAmount::Literal(offset), LimitAmount::Literal(count)) = (offset, count) else {
        return None;
    };
    usize::try_from(offset.saturating_add(*count)).ok()
}

fn project_preserves_row_limit_safety(pattern: &PatternPlan, item: &ProjectExpr) -> bool {
    match &item.expr {
        ValueExpr::Literal(_) => true,
        ValueExpr::Variable { name, .. } => pattern_binding(pattern, name).is_some(),
        ValueExpr::PropertyAccess { target, .. } => {
            let ValueExpr::Variable { name, .. } = target.as_ref() else {
                return false;
            };
            matches!(
                pattern_binding(pattern, name),
                Some(BindingElement::Node | BindingElement::Edge)
            )
        }
        _ => false,
    }
}

fn pattern_binding(pattern: &PatternPlan, name: &selene_core::DbString) -> Option<BindingElement> {
    pattern
        .bindings
        .iter()
        .find(|binding| binding.name == *name)
        .map(|binding| binding.element)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use selene_core::{GraphId, LabelSet, PropertyMap, Value, db_string};
    use selene_graph::SharedGraph;

    use crate::{
        EmptyProcedureRegistry, ExecutionPlan, analyze, parse, plan,
        runtime::{EvalCtx, TxContext, pattern, plan_runner::execute_plan},
    };

    fn planned(source: &str) -> ExecutionPlan {
        let statement = parse(source).expect("test input parses");
        let analyzed =
            analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
        plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
    }

    fn context(plan: &ExecutionPlan) -> TxContext<'_, '_> {
        TxContext::read_only(
            Arc::new(selene_graph::SeleneGraph::new(GraphId::new(991))),
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            &[],
        )
    }

    #[test]
    fn execute_plan_with_no_pattern_plan_seeds_single_empty_row() {
        let plan = planned("RETURN 1 AS n");
        let mut ctx = context(&plan);

        let table = execute_plan(&plan, &mut ctx).expect("plan executes");

        assert_eq!(table.row_count(), 1);
        assert_eq!(table.rows()[0].values(), &[Value::Int(1)]);
    }

    #[test]
    fn execute_plan_with_pattern_plan_runs_pattern_then_pipeline() {
        let graph = SharedGraph::new(GraphId::new(992));
        {
            let mut txn = graph.begin_write();
            let mut mutator = txn.mutator();
            mutator
                .create_node(LabelSet::new(), Default::default())
                .expect("node inserts");
            txn.commit().expect("fixture commits");
        }
        let plan = planned("MATCH (n) RETURN n");
        let mut ctx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            graph.index_providers(),
        );

        let table = execute_plan(&plan, &mut ctx).expect("plan executes");

        assert_eq!(table.row_count(), 1);
        assert!(matches!(table.rows()[0].values(), [Value::NodeRef(_)]));
    }

    #[test]
    fn execute_plan_returns_pattern_table_when_pipeline_empty() {
        let graph = SharedGraph::new(GraphId::new(993));
        {
            let mut txn = graph.begin_write();
            let mut mutator = txn.mutator();
            mutator
                .create_node(LabelSet::new(), Default::default())
                .expect("node inserts");
            txn.commit().expect("fixture commits");
        }
        let mut plan = planned("MATCH (n) RETURN n");
        plan.pipeline.clear();
        let mut ctx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            graph.index_providers(),
        );

        let table = execute_plan(&plan, &mut ctx).expect("plan executes");

        assert_eq!(table.row_count(), 1);
        assert_eq!(table.schema().columns.len(), 1);
        assert!(matches!(table.rows()[0].values(), [Value::NodeRef(_)]));
    }

    #[test]
    fn pattern_row_limit_caps_accepted_rows_before_pipeline() {
        let graph = SharedGraph::new(GraphId::new(994));
        {
            let mut txn = graph.begin_write();
            let mut mutator = txn.mutator();
            for _ in 0..3 {
                mutator
                    .create_node(LabelSet::new(), Default::default())
                    .expect("node inserts");
            }
            txn.commit().expect("fixture commits");
        }
        let plan = planned("MATCH (n) RETURN n");
        let pattern_plan = plan.pattern_plan.as_ref().expect("pattern plan");
        let ctx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            graph.index_providers(),
        );
        let eval_ctx = EvalCtx {
            tx: &ctx,
            expr_ids: &plan.expr_ids,
            subqueries: &plan.subqueries,
        };

        let table = pattern::execute_pattern_with_seed_schema_and_limit(
            pattern_plan,
            None,
            pattern::schema_for_pattern(pattern_plan),
            &eval_ctx,
            Some(2),
        )
        .expect("pattern executes");

        assert_eq!(table.row_count(), 2);
    }

    #[test]
    fn leading_pattern_row_limit_accepts_pre_return_literal_limit() {
        let plan = planned("MATCH (n) LIMIT 10 OFFSET 5 RETURN n");

        assert_eq!(super::pattern_row_limit(&plan), Some(15));
    }

    #[test]
    fn leading_pattern_row_limit_includes_offset_for_zero_count() {
        let plan = planned("MATCH (n) LIMIT 0 OFFSET 5 RETURN n");

        assert_eq!(super::pattern_row_limit(&plan), Some(5));
    }

    #[test]
    fn post_return_project_row_limit_accepts_pattern_variable() {
        let plan = planned("MATCH (n) RETURN n LIMIT 10");

        assert_eq!(super::pattern_row_limit(&plan), Some(10));
    }

    #[test]
    fn post_return_project_row_limit_accepts_direct_node_property() {
        let plan = planned("MATCH (n) RETURN n.name AS name LIMIT 10 OFFSET 5");

        assert_eq!(super::pattern_row_limit(&plan), Some(15));
    }

    #[test]
    fn post_return_project_row_limit_rejects_computed_projection() {
        let plan = planned("MATCH (n) RETURN n.age + 1 AS next_age LIMIT 10");

        assert_eq!(super::pattern_row_limit(&plan), None);
    }

    #[test]
    fn post_return_project_row_limit_rejects_nested_property_projection() {
        let plan = planned("MATCH (n) RETURN n.payload.kind AS kind LIMIT 10");

        assert_eq!(super::pattern_row_limit(&plan), None);
    }

    #[test]
    fn post_return_project_row_limit_rejects_parameterized_limit() {
        let plan = planned("MATCH (n) RETURN n.name AS name LIMIT $count :: INT");

        assert_eq!(super::pattern_row_limit(&plan), None);
    }

    #[test]
    fn post_return_project_row_limit_does_not_skip_computed_projection_errors() {
        let graph = SharedGraph::new(GraphId::new(995));
        {
            let mut txn = graph.begin_write();
            let mut mutator = txn.mutator();
            let age = db_string("age").expect("test string fits");
            mutator
                .create_node(
                    LabelSet::new(),
                    PropertyMap::from_pairs([(age.clone(), Value::Int(41))])
                        .expect("test properties fit"),
                )
                .expect("first node inserts");
            mutator
                .create_node(
                    LabelSet::new(),
                    PropertyMap::from_pairs([(
                        age,
                        Value::String(db_string("old").expect("test string fits")),
                    )])
                    .expect("test properties fit"),
                )
                .expect("second node inserts");
            txn.commit().expect("fixture commits");
        }
        let plan = planned("MATCH (n) RETURN n.age + 1 AS next_age LIMIT 1");
        let mut ctx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            graph.index_providers(),
        );

        execute_plan(&plan, &mut ctx).expect_err("computed projection remains uncapped");
    }
}
