//! M5c plan-snapshot corpus harness.

use std::collections::{BTreeMap, BTreeSet};

use selene_gql::{
    AggregateArg, AnalyzedStatement, AnalyzedType, BindingId, CatalogOp, EmptyProcedureRegistry,
    ExecutionPlan, ExprId, FilterPredicate, GqlType, ImplDefinedCaps, JoinTree, MutationOp,
    OptimizeContext, OrderKey, PipelineOp, PlannedTypePropertyConstraint, ProcedureRegistry,
    ProjectExpr, analyze, optimize, parse, plan,
    plan::optimize::{RULE_NAMES, optimize_summary},
};
use selene_testing::{MockIndexCatalog, MockProcedureRegistry};
use selene_testing::{PlanCorpus, PlanCorpusEntry, PlanCorpusRegistry};

#[test]
fn corpus_snapshots_match() {
    let corpus = PlanCorpus::m5c();
    let mock_catalog = PlanCorpus::standard_mock_catalog();
    let empty_registry = EmptyProcedureRegistry;
    let mock_registry = PlanCorpus::standard_mock_registry();

    for entry in corpus.entries() {
        let (_, plan) = plan_entry(entry, &empty_registry, &mock_registry);
        let caps = ImplDefinedCaps::default();
        let ctx = context_for(entry, &caps, &mock_catalog);
        let snapshot = optimize_summary(plan, &ctx);

        insta::with_settings!({ snapshot_suffix => entry.slug }, {
            insta::assert_snapshot!(snapshot.to_string());
        });
    }
}

#[test]
fn corpus_expected_rule_names_are_known() {
    let known = RULE_NAMES.iter().copied().collect::<BTreeSet<_>>();
    for entry in PlanCorpus::m5c().entries() {
        for rule in entry.expected_rules {
            assert!(
                known.contains(rule),
                "{} references unknown optimizer rule `{rule}`",
                entry.slug
            );
        }
    }
}

#[test]
fn corpus_covers_every_rule() {
    let covered = PlanCorpus::m5c()
        .entries()
        .flat_map(|entry| entry.expected_rules.iter().copied())
        .collect::<BTreeSet<_>>();
    let expected = RULE_NAMES.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(covered, expected);
}

#[test]
fn corpus_expected_rules_match_actual() {
    let corpus = PlanCorpus::m5c();
    let mock_catalog = PlanCorpus::standard_mock_catalog();
    let empty_registry = EmptyProcedureRegistry;
    let mock_registry = PlanCorpus::standard_mock_registry();

    for entry in corpus.entries() {
        let (_, plan) = plan_entry(entry, &empty_registry, &mock_registry);
        let caps = ImplDefinedCaps::default();
        let ctx = context_for(entry, &caps, &mock_catalog);
        let snapshot = optimize_summary(plan, &ctx);
        let expected = entry
            .expected_rules
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let actual = snapshot
            .fired_rules
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected, "{}", entry.slug);
    }
}

#[test]
fn recording_driver_matches_production_pipeline_op_high_water() {
    // PLAN-20: the snapshot corpus drives `optimize_summary` -> `optimize_recording`,
    // a test-only optimizer. It previously skipped the post-loop
    // `refresh_pipeline_op_high_water()` that the production `optimize` /
    // `optimize_with_rules` runs, so the corpus + idempotence snapshots verified
    // the test driver, not production. Now that the recording driver mirrors the
    // production post-loop step, pin parity: for every corpus entry the recording
    // driver's `next_pipeline_op_id` must equal production's. A future divergence
    // (e.g. dropping the refresh from one driver) fails here.
    let corpus = PlanCorpus::m5c();
    let mock_catalog = PlanCorpus::standard_mock_catalog();
    let empty_registry = EmptyProcedureRegistry;
    let mock_registry = PlanCorpus::standard_mock_registry();

    for entry in corpus.entries() {
        let caps = ImplDefinedCaps::default();

        // Recording driver (test harness) — re-plan because optimize consumes.
        let (_, recording_plan) = plan_entry(entry, &empty_registry, &mock_registry);
        let recording_ctx = context_for(entry, &caps, &mock_catalog);
        let recording = optimize_summary(recording_plan, &recording_ctx);

        // Production driver.
        let (_, production_plan) = plan_entry(entry, &empty_registry, &mock_registry);
        let production_ctx = context_for(entry, &caps, &mock_catalog);
        let production = optimize(production_plan, &production_ctx);

        assert_eq!(
            recording.next_pipeline_op_id,
            production.next_pipeline_op_id.get(),
            "{}: recording driver next_pipeline_op_id drifted from production",
            entry.slug
        );
    }
}

#[test]
fn corpus_idempotent_under_double_optimize() {
    let corpus = PlanCorpus::m5c();
    let mock_catalog = PlanCorpus::standard_mock_catalog();
    let empty_registry = EmptyProcedureRegistry;
    let mock_registry = PlanCorpus::standard_mock_registry();

    for entry in corpus.entries() {
        let (_, plan) = plan_entry(entry, &empty_registry, &mock_registry);
        let caps = ImplDefinedCaps::default();
        let ctx = context_for(entry, &caps, &mock_catalog);
        let once = optimize(plan, &ctx);
        let twice = optimize(once.clone(), &ctx);
        assert_eq!(format!("{once:?}"), format!("{twice:?}"), "{}", entry.slug);
    }
}

#[test]
fn corpus_preserves_ty_for_surviving_expression_rows() {
    let corpus = PlanCorpus::m5c();
    let mock_catalog = PlanCorpus::standard_mock_catalog();
    let empty_registry = EmptyProcedureRegistry;
    let mock_registry = PlanCorpus::standard_mock_registry();

    for entry in corpus.entries() {
        let (_, plan) = plan_entry(entry, &empty_registry, &mock_registry);
        let mut before = BTreeMap::new();
        collect_plan_types(&plan, &mut before);

        let caps = ImplDefinedCaps::default();
        let ctx = context_for(entry, &caps, &mock_catalog);
        let optimized = optimize(plan, &ctx);
        let mut after = BTreeMap::new();
        collect_plan_types(&optimized, &mut after);

        for (expr_id, before_ty) in before {
            if let Some(after_ty) = after.get(&expr_id) {
                assert_eq!(
                    after_ty,
                    &before_ty,
                    "{} changed ty for expr_id {}",
                    entry.slug,
                    expr_id.get()
                );
            }
        }
    }
}

#[test]
fn corpus_synthesized_split_predicates_are_boolean() {
    let corpus = PlanCorpus::m5c();
    let mock_catalog = PlanCorpus::standard_mock_catalog();
    let empty_registry = EmptyProcedureRegistry;
    let mock_registry = PlanCorpus::standard_mock_registry();

    for entry in corpus.entries() {
        let (analyzed, plan) = plan_entry(entry, &empty_registry, &mock_registry);
        let high_water = analyzed.expr_types.len() as u32;
        let caps = ImplDefinedCaps::default();
        let ctx = context_for(entry, &caps, &mock_catalog);
        let optimized = optimize(plan, &ctx);

        let mut predicates = Vec::new();
        collect_filter_predicates(&optimized, &mut predicates);
        for predicate in predicates {
            if predicate.expr_id.get() >= high_water {
                assert_eq!(
                    predicate.ty,
                    AnalyzedType::Resolved(GqlType::Boolean),
                    "{} synthesized non-boolean predicate at expr_id {}",
                    entry.slug,
                    predicate.expr_id.get()
                );
            }
        }
    }
}

#[test]
fn corpus_binding_refs_are_sorted_deduped_and_pattern_resolvable() {
    let corpus = PlanCorpus::m5c();
    let mock_catalog = PlanCorpus::standard_mock_catalog();
    let empty_registry = EmptyProcedureRegistry;
    let mock_registry = PlanCorpus::standard_mock_registry();

    for entry in corpus.entries() {
        let (_, plan) = plan_entry(entry, &empty_registry, &mock_registry);
        let caps = ImplDefinedCaps::default();
        let ctx = context_for(entry, &caps, &mock_catalog);
        let optimized = optimize(plan, &ctx);
        assert_binding_refs(&optimized, entry.slug);
    }
}

#[test]
#[ignore = "local-only spec mirror; run manually with --ignored"]
fn spec_13_section_7_invariants_present() {
    let spec = std::fs::read_to_string("../../_spec/13-iso-gql-planner.md")
        .expect("local spec mirror exists");
    for heading in [
        "## 7. Optimizer Implementation Invariants",
        "### 7.1 OPTIONAL Preservation",
        "### 7.2 Predicate Removal Contract",
        "### 7.3 ExprId Allocator Semantics",
        "### 7.4 Binding Ref Recompute Fallback",
        "### 7.5 Composite Index Subset Semantics",
        "### 7.6 Rule Order Rationale",
        "### 7.7 Standard Snapshot Corpus Fixtures",
        "### 7.8 Node-Only Typed Indexes",
        "### 7.9 Marker-Only WCO Contract",
        "### 7.10 Standard Mock Registry",
    ] {
        assert!(spec.contains(heading), "missing heading: {heading}");
    }
}

fn context_for<'a>(
    entry: &PlanCorpusEntry,
    caps: &'a ImplDefinedCaps,
    catalog: &'a MockIndexCatalog,
) -> OptimizeContext<'a> {
    let ctx = OptimizeContext::new(caps);
    if entry.uses_index_catalog {
        ctx.with_index_catalog(catalog)
    } else {
        ctx
    }
}

fn plan_entry(
    entry: &PlanCorpusEntry,
    empty_registry: &EmptyProcedureRegistry,
    mock_registry: &MockProcedureRegistry,
) -> (AnalyzedStatement, ExecutionPlan) {
    let registry = registry_for(entry, empty_registry, mock_registry);
    let statement = parse(entry.source).unwrap_or_else(|error| {
        panic!("{} failed to parse: {error:?}", entry.slug);
    });
    let analyzed = analyze(statement, registry, None).unwrap_or_else(|error| {
        panic!("{} failed to analyze: {error:?}", entry.slug);
    });
    let planned = plan(&analyzed, registry).unwrap_or_else(|error| {
        panic!("{} failed to plan: {error:?}", entry.slug);
    });
    (analyzed, planned)
}

fn registry_for<'a>(
    entry: &PlanCorpusEntry,
    empty_registry: &'a EmptyProcedureRegistry,
    mock_registry: &'a MockProcedureRegistry,
) -> &'a dyn ProcedureRegistry {
    match entry.registry {
        PlanCorpusRegistry::Empty => empty_registry,
        PlanCorpusRegistry::StandardMock => mock_registry,
        _ => empty_registry,
    }
}

fn collect_plan_types(plan: &ExecutionPlan, types: &mut BTreeMap<ExprId, AnalyzedType>) {
    if let Some(pattern) = &plan.pattern_plan {
        collect_filter_types(&pattern.filters, types);
        collect_join_tree_types(&pattern.join_tree, types);
    }
    for op in &plan.pipeline {
        collect_pipeline_types(op, types);
    }
}

fn collect_join_tree_types(tree: &JoinTree, types: &mut BTreeMap<ExprId, AnalyzedType>) {
    match tree {
        JoinTree::Scan(scan) => collect_filter_types(&scan.property_predicates, types),
        JoinTree::Expand { child, edge, .. } => {
            collect_join_tree_types(child, types);
            collect_filter_types(&edge.property_predicates, types);
            collect_filter_types(&edge.right_property_predicates, types);
        }
        // Path predicate IDs are retained in the immutable automata; their
        // transport is checked separately against the analyzed expressions.
        JoinTree::Paths(_) => {}
        JoinTree::HashJoin { left, right, .. } => {
            collect_join_tree_types(left, types);
            collect_join_tree_types(right, types);
        }
        JoinTree::Outer {
            left,
            right,
            right_filters,
            ..
        } => {
            collect_join_tree_types(left, types);
            collect_join_tree_types(right, types);
            collect_filter_types(right_filters, types);
        }
        JoinTree::WorstCaseOptimal { intersection, .. } => {
            for child in intersection {
                collect_join_tree_types(child, types);
            }
        }
        JoinTree::Subplan(plan) => collect_plan_types(plan, types),
        _ => {}
    }
}

fn collect_pipeline_types(op: &PipelineOp, types: &mut BTreeMap<ExprId, AnalyzedType>) {
    match op {
        PipelineOp::Filter(predicate) => record_filter_type(predicate, types),
        PipelineOp::Project(items) | PipelineOp::Let(items) => {
            for item in items {
                record_project_type(item, types);
            }
        }
        PipelineOp::Unwind { source, .. } => record_project_type(source, types),
        PipelineOp::OrderBy(keys) => {
            for key in keys {
                record_order_type(key, types);
            }
        }
        PipelineOp::TopK { keys, .. } => {
            for key in keys {
                record_order_type(key, types);
            }
        }
        PipelineOp::GroupBy { keys, aggregates } => {
            for key in keys {
                record_project_type(key, types);
            }
            for aggregate in aggregates {
                types.insert(aggregate.aggregate_id, aggregate.ty.clone());
                for arg in &aggregate.args {
                    record_aggregate_arg_type(arg, types);
                }
            }
        }
        PipelineOp::Union { rhs, .. }
        | PipelineOp::Chain(rhs)
        | PipelineOp::CorrelatedChain(rhs) => collect_plan_types(rhs, types),
        PipelineOp::Call(call) => {
            for arg in &call.args {
                record_project_type(arg, types);
            }
        }
        PipelineOp::Mutation(mutation) => collect_mutation_types(mutation, types),
        PipelineOp::Catalog(catalog) => collect_catalog_types(catalog, types),
        PipelineOp::Limit { .. } | PipelineOp::Distinct | PipelineOp::Tx(_) => {}
        _ => {}
    }
}

fn collect_mutation_types(mutation: &MutationOp, types: &mut BTreeMap<ExprId, AnalyzedType>) {
    match mutation {
        MutationOp::InsertNode { property_inits, .. }
        | MutationOp::InsertEdge { property_inits, .. } => {
            for init in property_inits {
                record_project_type(&init.value, types);
            }
        }
        MutationOp::SetProperty { value, .. } => record_project_type(value, types),
        _ => {}
    }
}

fn collect_catalog_types(catalog: &CatalogOp, types: &mut BTreeMap<ExprId, AnalyzedType>) {
    match catalog {
        CatalogOp::CreateNodeType { properties, .. }
        | CatalogOp::CreateEdgeType { properties, .. } => {
            for property in properties {
                for constraint in &property.constraints {
                    if let PlannedTypePropertyConstraint::Default(project, _) = constraint {
                        record_project_type(project, types);
                    }
                }
            }
        }
        _ => {}
    }
}

fn collect_filter_types(
    predicates: &[FilterPredicate],
    types: &mut BTreeMap<ExprId, AnalyzedType>,
) {
    for predicate in predicates {
        record_filter_type(predicate, types);
    }
}

fn record_filter_type(predicate: &FilterPredicate, types: &mut BTreeMap<ExprId, AnalyzedType>) {
    record_type(types, predicate.expr_id, predicate.ty.clone());
}

fn record_project_type(project: &ProjectExpr, types: &mut BTreeMap<ExprId, AnalyzedType>) {
    record_type(types, project.expr_id, project.ty.clone());
}

fn record_order_type(key: &OrderKey, types: &mut BTreeMap<ExprId, AnalyzedType>) {
    record_type(types, key.expr_id, key.ty.clone());
}

fn record_aggregate_arg_type(arg: &AggregateArg, types: &mut BTreeMap<ExprId, AnalyzedType>) {
    record_type(types, arg.expr_id, arg.ty.clone());
}

fn record_type(types: &mut BTreeMap<ExprId, AnalyzedType>, expr_id: ExprId, ty: AnalyzedType) {
    if let Some(previous) = types.insert(expr_id, ty.clone()) {
        assert_eq!(
            previous,
            ty,
            "expr_id {} has inconsistent ty",
            expr_id.get()
        );
    }
}

fn collect_filter_predicates<'a>(
    plan: &'a ExecutionPlan,
    predicates: &mut Vec<&'a FilterPredicate>,
) {
    if let Some(pattern) = &plan.pattern_plan {
        predicates.extend(&pattern.filters);
        collect_join_tree_predicates(&pattern.join_tree, predicates);
    }
    for op in &plan.pipeline {
        collect_pipeline_predicates(op, predicates);
    }
}

fn collect_join_tree_predicates<'a>(tree: &'a JoinTree, predicates: &mut Vec<&'a FilterPredicate>) {
    match tree {
        JoinTree::Scan(scan) => predicates.extend(&scan.property_predicates),
        JoinTree::Expand { child, edge, .. } => {
            collect_join_tree_predicates(child, predicates);
            predicates.extend(&edge.property_predicates);
            predicates.extend(&edge.right_property_predicates);
        }
        JoinTree::Paths(_) => {}
        JoinTree::HashJoin { left, right, .. } => {
            collect_join_tree_predicates(left, predicates);
            collect_join_tree_predicates(right, predicates);
        }
        JoinTree::Outer {
            left,
            right,
            right_filters,
            ..
        } => {
            collect_join_tree_predicates(left, predicates);
            collect_join_tree_predicates(right, predicates);
            predicates.extend(right_filters);
        }
        JoinTree::WorstCaseOptimal { intersection, .. } => {
            for child in intersection {
                collect_join_tree_predicates(child, predicates);
            }
        }
        JoinTree::Subplan(plan) => collect_filter_predicates(plan, predicates),
        _ => {}
    }
}

fn collect_pipeline_predicates<'a>(op: &'a PipelineOp, predicates: &mut Vec<&'a FilterPredicate>) {
    match op {
        PipelineOp::Filter(predicate) => predicates.push(predicate),
        PipelineOp::Union { rhs, .. }
        | PipelineOp::Chain(rhs)
        | PipelineOp::CorrelatedChain(rhs) => {
            collect_filter_predicates(rhs, predicates);
        }
        _ => {}
    }
}

fn assert_binding_refs(plan: &ExecutionPlan, slug: &str) {
    if let Some(pattern) = &plan.pattern_plan {
        let allowed = pattern
            .bindings
            .iter()
            .map(|binding| binding.binding)
            .collect::<BTreeSet<_>>();
        assert_pattern_filter_refs(&pattern.filters, &allowed, slug);
        assert_join_tree_refs(&pattern.join_tree, &allowed, slug);
    }
    for op in &plan.pipeline {
        assert_pipeline_refs(op, slug);
    }
}

fn assert_join_tree_refs(tree: &JoinTree, allowed: &BTreeSet<BindingId>, slug: &str) {
    match tree {
        JoinTree::Scan(scan) => {
            assert_pattern_filter_refs(&scan.property_predicates, allowed, slug)
        }
        JoinTree::Expand { child, edge, .. } => {
            assert_join_tree_refs(child, allowed, slug);
            assert_pattern_filter_refs(&edge.property_predicates, allowed, slug);
            assert_pattern_filter_refs(&edge.right_property_predicates, allowed, slug);
        }
        JoinTree::Paths(program) => {
            assert!(
                program.bindings.iter().all(|id| allowed.contains(id)),
                "{slug}: path binding escaped pattern"
            );
        }
        JoinTree::HashJoin { left, right, .. } => {
            assert_join_tree_refs(left, allowed, slug);
            assert_join_tree_refs(right, allowed, slug);
        }
        JoinTree::Outer {
            left,
            right,
            right_filters,
            ..
        } => {
            assert_join_tree_refs(left, allowed, slug);
            assert_join_tree_refs(right, allowed, slug);
            assert_pattern_filter_refs(right_filters, allowed, slug);
        }
        JoinTree::WorstCaseOptimal { intersection, .. } => {
            for child in intersection {
                assert_join_tree_refs(child, allowed, slug);
            }
        }
        JoinTree::Subplan(plan) => assert_binding_refs(plan, slug),
        _ => {}
    }
}

fn assert_pattern_filter_refs(
    predicates: &[FilterPredicate],
    allowed: &BTreeSet<BindingId>,
    slug: &str,
) {
    for predicate in predicates {
        assert_ref_list(&predicate.binding_refs, slug);
        for binding in &predicate.binding_refs {
            assert!(
                allowed.contains(binding),
                "{slug} has unresolved pattern binding ref {}",
                binding.get()
            );
        }
    }
}

fn assert_pipeline_refs(op: &PipelineOp, slug: &str) {
    match op {
        PipelineOp::Filter(predicate) => assert_ref_list(&predicate.binding_refs, slug),
        PipelineOp::Project(items) | PipelineOp::Let(items) => {
            for item in items {
                assert_ref_list(&item.binding_refs, slug);
            }
        }
        PipelineOp::Unwind { source, .. } => assert_ref_list(&source.binding_refs, slug),
        PipelineOp::OrderBy(keys) => {
            for key in keys {
                assert_ref_list(&key.binding_refs, slug);
            }
        }
        PipelineOp::TopK { keys, .. } => {
            for key in keys {
                assert_ref_list(&key.binding_refs, slug);
            }
        }
        PipelineOp::GroupBy { keys, .. } => {
            for key in keys {
                assert_ref_list(&key.binding_refs, slug);
            }
        }
        PipelineOp::Union { rhs, .. }
        | PipelineOp::Chain(rhs)
        | PipelineOp::CorrelatedChain(rhs) => assert_binding_refs(rhs, slug),
        PipelineOp::Call(call) => {
            for arg in &call.args {
                assert_ref_list(&arg.binding_refs, slug);
            }
        }
        PipelineOp::Mutation(mutation) => assert_mutation_refs(mutation, slug),
        PipelineOp::Catalog(catalog) => assert_catalog_refs(catalog, slug),
        _ => {}
    }
}

fn assert_mutation_refs(mutation: &MutationOp, slug: &str) {
    match mutation {
        MutationOp::InsertNode { property_inits, .. }
        | MutationOp::InsertEdge { property_inits, .. } => {
            for init in property_inits {
                assert_ref_list(&init.value.binding_refs, slug);
            }
        }
        MutationOp::SetProperty { value, .. } => assert_ref_list(&value.binding_refs, slug),
        _ => {}
    }
}

fn assert_catalog_refs(catalog: &CatalogOp, slug: &str) {
    match catalog {
        CatalogOp::CreateNodeType { properties, .. }
        | CatalogOp::CreateEdgeType { properties, .. } => {
            for property in properties {
                for constraint in &property.constraints {
                    if let PlannedTypePropertyConstraint::Default(project, _) = constraint {
                        assert_ref_list(&project.binding_refs, slug);
                    }
                }
            }
        }
        _ => {}
    }
}

fn assert_ref_list(refs: &[BindingId], slug: &str) {
    assert!(
        refs.windows(2).all(|pair| pair[0] < pair[1]),
        "{slug} has unsorted or duplicate binding_refs: {refs:?}"
    );
}
