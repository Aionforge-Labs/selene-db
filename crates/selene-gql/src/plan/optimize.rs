//! Planner optimizer entry points.
//!
//! The optimizer is intentionally infallible: lowering owns planner errors,
//! while rules either rewrite a plan or leave it unchanged.

mod binding_refs;
mod context;
mod cost;
mod index_catalog;
mod live_index_catalog;
mod registry;
mod rule;
pub mod rules;
mod selectivity;
#[cfg(any(test, feature = "test-harness"))]
mod summary;
mod walk;

pub use context::OptimizeContext;
pub use index_catalog::{
    CompositeIndexHandle, EmptyIndexCatalog, IndexCatalog, IndexHandle, IndexKind, IndexTarget,
    TypedIndexLookup,
};
pub use live_index_catalog::LiveIndexCatalog;
pub use registry::{DEFAULT_RULES, RULE_NAMES};
pub use rule::{Rule, Transformed};
#[cfg(any(test, feature = "test-harness"))]
pub use summary::{
    PatternSnapshot, PipelineOpSummary, PlanSnapshot, ScanSnapshot, optimize_summary,
};
// Test-harness-gated re-export of the cost estimators so integration tests can
// exercise them in isolation against a synthetic catalog. Production code
// reaches these via the `cost` module path; this is not a stable public API
// (gated exactly like `summary`).
#[cfg(any(test, feature = "test-harness"))]
pub use cost::{
    composite_cost, disjunctive_cost, in_list_cost, linear_baseline, should_decline_index,
    typed_index_cost,
};

use crate::plan::ExecutionPlan;

/// Optimize an execution plan using the default structural rule set.
#[must_use]
#[tracing::instrument(
    name = "selene.gql.optimize",
    skip(plan, ctx),
    fields(category = ?plan.category)
)]
pub fn optimize(plan: ExecutionPlan, ctx: &OptimizeContext<'_>) -> ExecutionPlan {
    optimize_with_rules(plan, DEFAULT_RULES, ctx)
}

/// Optimize an execution plan using an explicit rule set.
///
/// This seam exists for defensive fixed-point tests that need custom rules
/// without exposing rule registration as public API.
pub(crate) fn optimize_with_rules(
    mut plan: ExecutionPlan,
    rules: &[&'static dyn Rule],
    ctx: &OptimizeContext<'_>,
) -> ExecutionPlan {
    for _ in 0..ctx.impl_defined_caps.max_optimizer_iterations {
        let mut changed = false;
        for rule in rules {
            let transformed = rule.rewrite(plan, ctx);
            plan = transformed.plan;
            changed |= transformed.changed;
        }
        if !changed {
            plan.refresh_pipeline_op_high_water();
            return plan;
        }
    }
    plan.refresh_pipeline_op_high_water();
    plan
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{
        EmptyProcedureRegistry, ImplDefinedCaps, PipelineOp, StatementCategory, analyze, parse,
        plan::{
            BindingTableSchema, ExecutionPlan,
            optimize::{OptimizeContext, Rule, Transformed, optimize, optimize_with_rules},
            plan,
        },
    };

    fn empty_plan(caps: ImplDefinedCaps) -> ExecutionPlan {
        ExecutionPlan {
            category: StatementCategory::ReadOnly,
            pattern_plan: None,
            pipeline: Vec::new(),
            output_schema: BindingTableSchema {
                columns: Vec::new(),
            },
            impl_defined_caps: caps,
            expr_ids: Default::default(),
            subqueries: Default::default(),
            next_expr_id: crate::ExprId::new(0),
            next_pipeline_op_id: crate::PipelineOpId::new(0),
        }
    }

    fn plan_one(source: &str) -> ExecutionPlan {
        let statement = parse(source).expect("test input parses");
        let analyzed =
            analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
        plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
    }

    #[test]
    fn no_rules_return_original_plan() {
        let caps = ImplDefinedCaps::default();
        let ctx = OptimizeContext::new(&caps);
        let plan = empty_plan(caps);
        let before = format!("{plan:?}");
        let optimized = optimize_with_rules(plan, &[], &ctx);
        assert_eq!(format!("{optimized:?}"), before);
    }

    #[test]
    fn default_optimizer_is_idempotent_after_convergence() {
        let plan = plan_one("MATCH (n) FILTER n.age > 30 RETURN n ORDER BY n.age LIMIT 10");
        let once = optimize(plan, &OptimizeContext::default());
        let twice = optimize(once.clone(), &OptimizeContext::default());
        assert_eq!(format!("{once:?}"), format!("{twice:?}"));
    }

    #[test]
    fn iteration_cap_stops_oscillating_rules() {
        struct AlwaysChanged {
            calls: AtomicUsize,
        }

        impl Rule for AlwaysChanged {
            fn name(&self) -> &'static str {
                "always_changed"
            }

            fn rewrite(
                &self,
                mut plan: ExecutionPlan,
                _ctx: &OptimizeContext<'_>,
            ) -> Transformed<ExecutionPlan> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if plan.pipeline.is_empty() {
                    plan.pipeline.push(PipelineOp::Distinct);
                } else {
                    plan.pipeline.clear();
                }
                Transformed::changed(plan)
            }
        }

        let caps = ImplDefinedCaps {
            max_optimizer_iterations: 3,
            ..ImplDefinedCaps::default()
        };
        let ctx = OptimizeContext::new(&caps);
        let rule = Box::leak(Box::new(AlwaysChanged {
            calls: AtomicUsize::new(0),
        }));
        let rules: [&'static dyn Rule; 1] = [rule];

        let optimized = optimize_with_rules(empty_plan(caps), &rules, &ctx);

        assert_eq!(rule.calls.load(Ordering::SeqCst), 3);
        assert!(
            optimized.pipeline.is_empty() || matches!(optimized.pipeline[0], PipelineOp::Distinct)
        );
    }

    #[test]
    fn optimizer_refreshes_pipeline_op_id_high_water_after_rewrite() {
        struct AppendDistinct {
            calls: AtomicUsize,
        }

        impl Rule for AppendDistinct {
            fn name(&self) -> &'static str {
                "append_distinct"
            }

            fn rewrite(
                &self,
                mut plan: ExecutionPlan,
                _ctx: &OptimizeContext<'_>,
            ) -> Transformed<ExecutionPlan> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    plan.pipeline.push(PipelineOp::Distinct);
                    Transformed::changed(plan)
                } else {
                    Transformed::unchanged(plan)
                }
            }
        }

        let caps = ImplDefinedCaps::default();
        let ctx = OptimizeContext::new(&caps);
        let rule = Box::leak(Box::new(AppendDistinct {
            calls: AtomicUsize::new(0),
        }));
        let rules: [&'static dyn Rule; 1] = [rule];

        let optimized = optimize_with_rules(empty_plan(caps), &rules, &ctx);

        assert_eq!(optimized.pipeline.len(), 1);
        assert_eq!(
            optimized.next_pipeline_op_id.get(),
            optimized.pipeline.len() as u32
        );
    }

    // --- PLAN-18: expression-subquery body coverage ---------------------------
    //
    // The optimizer never recurses into `plan.subqueries`, so an indexed
    // EXISTS/COUNT/VALUE body always runs a Linear scan even when an applicable
    // index exists for the same label that the OUTER scan does use. The corpus
    // invariant collectors never descended into subquery bodies, leaving this
    // behavior unpinned. These tests descend into the subquery bodies and pin
    // the current (Linear, unoptimized) behavior, contrasted against the
    // optimized outer scan. Whether to optimize subquery bodies is a deferred
    // decision; this only PINS current behavior.

    use std::sync::Arc;

    use selene_core::GraphId;
    use selene_graph::SeleneGraph;

    use crate::plan::{JoinTree, LiveIndexCatalog, ScanAccess, SubqueryBody, SubqueryKind};

    fn leading_scan_access(tree: &JoinTree) -> Option<&ScanAccess> {
        match tree {
            JoinTree::Scan(scan) => Some(&scan.access),
            JoinTree::Expand { child, .. } => leading_scan_access(child),
            _ => None,
        }
    }

    fn optimize_with_label_index(source: &str) -> ExecutionPlan {
        let plan = plan_one(source);
        // LiveIndexCatalog::label_index always returns Some for any label, so a
        // bare `(:Person)` scan takes the intrinsic label index — the contrast
        // baseline for the Linear subquery-body pins.
        let catalog = LiveIndexCatalog::new(Arc::new(SeleneGraph::new(GraphId::new(9_001))));
        let ctx = OptimizeContext::default().with_index_catalog(&catalog);
        optimize(plan, &ctx)
    }

    #[test]
    fn outer_scan_uses_label_index_as_a_contrast_baseline() {
        // Confirms the index IS applicable to a `(:Person)` scan, so the Linear
        // subquery-body pins below are meaningful (the body declines an index
        // the outer query takes).
        let optimized = optimize_with_label_index("MATCH (a:Person) RETURN a");
        let access = leading_scan_access(&optimized.pattern_plan.as_ref().unwrap().join_tree)
            .expect("leading scan");
        assert!(
            matches!(access, ScanAccess::LabelIndex { .. }),
            "outer (:Person) scan should take the label index, got {access:?}"
        );
    }

    #[test]
    fn exists_subquery_body_stays_linear_unoptimized() {
        // EXISTS lowers to a `SubqueryBody::Pattern`. The optimizer does not
        // descend, so the body scan stays Linear even though `(:Person)` would
        // otherwise take the label index (proven above).
        for source in
            ["MATCH (a:Person) WHERE EXISTS { MATCH (n:Person) WHERE n.age > 5 } RETURN a"]
        {
            let optimized = optimize_with_label_index(source);
            let mut subqueries = 0;
            for subquery in optimized.subqueries.iter() {
                assert!(
                    matches!(subquery.kind, SubqueryKind::Exists { .. }),
                    "unexpected subquery kind {:?} for {source}",
                    subquery.kind
                );
                let SubqueryBody::Pattern(pattern) = &subquery.body else {
                    panic!("EXISTS body must be a Pattern body");
                };
                let access = leading_scan_access(&pattern.join_tree).expect("body scan");
                assert!(
                    matches!(access, ScanAccess::Linear),
                    "{source}: subquery body scan should stay Linear (optimizer does not \
                     recurse into subqueries), got {access:?}"
                );
                subqueries += 1;
            }
            assert_eq!(subqueries, 1, "{source} should plan exactly one subquery");
        }
    }

    #[test]
    fn value_subquery_body_plan_stays_linear_unoptimized() {
        // VALUE lowers to a full `SubqueryBody::Plan`. Its inner pattern scan is
        // likewise left Linear.
        let optimized = optimize_with_label_index(
            "MATCH (a:Person) RETURN VALUE { MATCH (n:Person) WHERE n.age > 5 RETURN n.age LIMIT 1 } AS v",
        );
        let mut found = false;
        for subquery in optimized.subqueries.iter() {
            assert_eq!(subquery.kind, SubqueryKind::Value);
            let SubqueryBody::Plan(inner) = &subquery.body else {
                panic!("VALUE body must be a Plan body");
            };
            let access = leading_scan_access(&inner.pattern_plan.as_ref().unwrap().join_tree)
                .expect("inner body scan");
            assert!(
                matches!(access, ScanAccess::Linear),
                "VALUE subquery body scan should stay Linear, got {access:?}"
            );
            found = true;
        }
        assert!(found, "VALUE query should plan a subquery");
    }

    /// Walk a subquery body and record every filter-predicate `(expr_id, ty)`.
    /// This is the descent the external `plan_snapshot_corpus` collectors cannot
    /// perform (`SubqueryRegistry::iter` is `pub(crate)`), so the type-stability
    /// invariant is extended into subquery bodies here, in-crate.
    fn collect_body_predicate_types(
        body: &SubqueryBody,
        types: &mut std::collections::BTreeMap<crate::ExprId, crate::AnalyzedType>,
    ) {
        let pattern = match body {
            SubqueryBody::Pattern(pattern) => Some(pattern.as_ref()),
            SubqueryBody::Plan(plan) => plan.pattern_plan.as_ref(),
        };
        let Some(pattern) = pattern else { return };
        let mut record = |predicate: &crate::FilterPredicate| {
            if let Some(previous) = types.insert(predicate.expr_id, predicate.ty.clone()) {
                assert_eq!(
                    previous,
                    predicate.ty,
                    "subquery-body expr_id {} has an inconsistent ty",
                    predicate.expr_id.get()
                );
            }
        };
        for predicate in &pattern.filters {
            record(predicate);
        }
        // The body scans are Linear here, so their property predicates live on
        // the leading scan; record those too.
        let mut tree = &pattern.join_tree;
        loop {
            match tree {
                JoinTree::Scan(scan) => {
                    for predicate in &scan.property_predicates {
                        record(predicate);
                    }
                    break;
                }
                JoinTree::Expand { child, .. } => tree = child,
                _ => break,
            }
        }
    }

    #[test]
    fn subquery_body_predicate_types_are_stable_across_optimize() {
        // PLAN-18 (collector half): the external corpus invariants
        // (`corpus_optimize_preserves_expr_types`) never descend into subquery
        // bodies, so a rule that mutated an expr-id's analyzed type INSIDE an
        // EXISTS/VALUE body would go uncaught. Pin the type-stability
        // invariant inside the body: every `(expr_id, ty)` observed before
        // optimize must match the one observed after. (The bodies are currently
        // left untouched, so this also documents that they are stable today.)
        for source in [
            "MATCH (a:Person) WHERE EXISTS { MATCH (n:Person) WHERE n.age > 5 } RETURN a",
            "MATCH (a:Person) RETURN VALUE { MATCH (n:Person) WHERE n.age > 5 RETURN n.age LIMIT 1 } AS v",
        ] {
            let before = plan_one(source);
            let mut before_types = std::collections::BTreeMap::new();
            for subquery in before.subqueries.iter() {
                collect_body_predicate_types(&subquery.body, &mut before_types);
            }
            assert!(
                !before_types.is_empty(),
                "{source}: subquery body should contribute at least one typed predicate"
            );

            let optimized = optimize_with_label_index(source);
            let mut after_types = std::collections::BTreeMap::new();
            for subquery in optimized.subqueries.iter() {
                collect_body_predicate_types(&subquery.body, &mut after_types);
            }

            for (expr_id, before_ty) in &before_types {
                if let Some(after_ty) = after_types.get(expr_id) {
                    assert_eq!(
                        before_ty,
                        after_ty,
                        "{source}: subquery-body expr_id {} changed ty across optimize",
                        expr_id.get()
                    );
                }
            }
        }
    }
}
