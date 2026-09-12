//! F03-PR03 logical effect and operator regressions.
//!
//! Proves that effects resolve from registration metadata, that a parser-only
//! classifier cannot pass the indirect-write fixture, that filter/project/page
//! preserve schemas and duplicates, that GP18 mixing never silently splits,
//! and that EXPLAIN carries stable semantic descriptors without runtime
//! addresses.

use selene_core::DbString;
use selene_gql::{
    BindingTableSchema, EmptyProcedureRegistry, ExecutionPlan, GqlType, LimitAmount, PipelineOp,
    PlannedCall, PlannedYieldItem, ProcedureHandle, ProcedureMetadata, ProcedureMutability,
    ProcedureOutputColumn, ProcedureOutputSchema, ProcedureParameter, ProcedureRegistry,
    ProcedureSignature, ProcedureTier, SourceSpan, StatementCategory, YieldKind, analyze,
    check_gp18, classify_analyzed, classify_plan, explain_logical, lower_logical,
    measure_lowering_cost, parse, plan, verify_plan_effects,
};
use selene_testing::MockProcedureRegistry;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test strings fit DB string cap")
}

fn registry_with_mutability(mutability: ProcedureMutability) -> MockProcedureRegistry {
    MockProcedureRegistry::new().with_procedure_mutability(
        vec![db_string("pkg"), db_string("proc")],
        Vec::new(),
        vec![ProcedureOutputColumn::new(
            db_string("result"),
            GqlType::String,
        )],
        mutability,
    )
}

fn analyze_with(source: &str, registry: &dyn ProcedureRegistry) -> selene_gql::AnalyzedStatement {
    let statement = parse(source).expect("test input parses");
    analyze(statement, registry, None).expect("test input analyzes")
}

#[test]
fn top_level_call_effects_resolve_from_registration_metadata() {
    for (mutability, expected) in [
        (ProcedureMutability::Read, selene_gql::LogicalEffect::Query),
        (
            ProcedureMutability::SchemaWrite,
            selene_gql::LogicalEffect::Catalog,
        ),
        (
            ProcedureMutability::MaintenanceWrite,
            selene_gql::LogicalEffect::Maintenance,
        ),
    ] {
        let registry = registry_with_mutability(mutability);
        let analyzed = analyze_with("CALL pkg.proc() YIELD result", &registry);
        let summary = classify_analyzed(&analyzed);
        assert_eq!(summary.effect, expected, "{mutability:?}");
        assert_eq!(summary.call_count, 1);
        // Write-set computation stays conservative and cheap: one linear pass
        // over calls and write entries, no graph inspection.
        assert_eq!(summary.write_entry_count, 0);
    }
}

#[test]
fn parser_only_category_misses_hidden_catalog_effect() {
    // A parser-only classifier inspects only the top-level statement shape:
    // `Statement::Query` implies read-only. The plan-level classifier visits
    // nested CALL metadata, so it upgrades a read-only plan carrying a hidden
    // catalog write. This is the indirect-write fixture a parser-only
    // classifier would fail.
    let read_registry = EmptyProcedureRegistry;
    let statement = parse("RETURN 1 AS n").expect("parses");
    let analyzed = analyze(statement, &read_registry, None).expect("analyzes");
    let mut reference_plan = plan(&analyzed, &read_registry).expect("plans");
    assert_eq!(reference_plan.category, StatementCategory::ReadOnly);

    // Inject a catalog-effect call under the read-only category, simulating a
    // registry drift between analysis and planning that a top-level check
    // would miss.
    let catalog_registry = registry_with_mutability(ProcedureMutability::SchemaWrite);
    let catalog_analyzed = analyze_with("CALL pkg.proc() YIELD result", &catalog_registry);
    let catalog_plan = plan(&catalog_analyzed, &catalog_registry).expect("catalog plans");
    let [PipelineOp::Call(hidden)] = catalog_plan.pipeline.as_slice() else {
        panic!("expected one call op");
    };
    reference_plan
        .pipeline
        .push(PipelineOp::Call(hidden.clone()));

    let summary = classify_plan(&reference_plan);
    assert!(
        summary.has_catalog_write,
        "plan classifier must see the hidden catalog write"
    );
    assert_eq!(summary.effect, selene_gql::LogicalEffect::Catalog);
    // The naive category check still says read-only; that is the parser-only
    // failure mode.
    assert_eq!(reference_plan.category, StatementCategory::ReadOnly);
    let semantic = classify_analyzed(&analyzed);
    assert_eq!(semantic.effect, selene_gql::LogicalEffect::Query);
    let error = verify_plan_effects(&semantic, &reference_plan)
        .expect_err("hidden write under read-only must fail verification");
    assert_eq!(error.gqlstatus().as_str(), "25G02");
}

#[test]
fn registry_drift_between_analyze_and_plan_is_rejected() {
    // Analyze with a read registry so the nested CALL passes analysis, then
    // lower/plan against a catalog-write registry for the same procedure name.
    // Effects must resolve from current registration metadata, never from the
    // analysis-time copy or the procedure name alone.
    let read_registry = MockProcedureRegistry::new().with_procedure_mutability(
        vec![db_string("pkg"), db_string("proc")],
        Vec::new(),
        vec![ProcedureOutputColumn::new(
            db_string("result"),
            GqlType::String,
        )],
        ProcedureMutability::Read,
    );
    let analyzed = analyze_with(
        "MATCH (n) CALL pkg.proc() YIELD result RETURN result",
        &read_registry,
    );
    let write_registry = MockProcedureRegistry::new().with_procedure_mutability(
        vec![db_string("pkg"), db_string("proc")],
        Vec::new(),
        vec![ProcedureOutputColumn::new(
            db_string("result"),
            GqlType::String,
        )],
        ProcedureMutability::SchemaWrite,
    );
    let plan_error =
        plan(&analyzed, &write_registry).expect_err("mutability drift must fail planning");
    // Registry drift surfaces as a metadata mismatch (implementation-defined),
    // not as a silent authority upgrade. The key property is that planning
    // fails rather than executing with stale query authority.
    assert_eq!(plan_error.gqlstatus().as_str(), "5GQL0");
    let logical_error = lower_logical(&analyzed, &write_registry)
        .expect_err("mutability drift must fail logical lowering");
    assert_eq!(logical_error.gqlstatus().as_str(), "5GQL0");
}

#[test]
fn logical_filter_project_page_preserve_schema_and_duplicates() {
    let registry = EmptyProcedureRegistry;
    let analyzed = analyze_with(
        "MATCH (n) WHERE n.age > 1 RETURN n.age AS age LIMIT 2",
        &registry,
    );
    let logical = lower_logical(&analyzed, &registry).expect("lowers");
    let reference = plan(&analyzed, &registry).expect("reference plans");

    // Schemas agree on the projected output column.
    assert_eq!(logical.output_schema.columns.len(), 1);
    assert_eq!(reference.output_schema.columns.len(), 1);
    assert_eq!(
        logical.output_schema.columns[0].name, reference.output_schema.columns[0].name,
        "logical and reference schemas must agree on the projected alias"
    );
    assert_eq!(
        logical.output_schema.columns[0]
            .name
            .as_ref()
            .map(|name| name.as_str()),
        Some("age")
    );
    // Filter, project, and page preserve duplicates and ordering by contract.
    assert!(logical.preserves_duplicates());
    for op in &logical.operators {
        assert_eq!(
            op.multiplicity(),
            selene_gql::LogicalMultiplicity::PreservesDuplicates,
            "{op:?}"
        );
    }
    // Page never reorders.
    for op in &logical.operators {
        if matches!(op, selene_gql::LogicalOp::Page { .. }) {
            assert_eq!(op.ordering(), selene_gql::LogicalOrdering::Preserved);
        }
    }

    // Reference execution preserves duplicates: two nodes with the same age
    // return two rows through filter/project/page.
    {
        use selene_core::{GraphId, LabelSet, PropertyMap, Value};
        use selene_graph::SharedGraph;
        let graph = SharedGraph::new(GraphId::new(31_001));
        {
            let mut txn = graph.begin_write();
            let mut mutator = txn.mutator();
            for _ in 0..2 {
                let age = db_string("age");
                mutator
                    .create_node(
                        LabelSet::new(),
                        PropertyMap::from_pairs([(age, Value::Int(5))]).expect("props fit"),
                    )
                    .expect("inserts");
            }
            txn.commit().expect("commits");
        }
        let mut session = selene_gql::Session::new(&graph);
        let output = session
            .execute_source(
                "MATCH (n) WHERE n.age > 1 RETURN n.age AS age LIMIT 10",
                &EmptyProcedureRegistry,
            )
            .expect("executes");
        let selene_gql::StatementOutput::Rows(table) = output else {
            panic!("expected rows");
        };
        assert_eq!(table.row_count(), 2, "duplicates are preserved");
        assert_eq!(table.schema().columns.len(), 1);
    }
}

#[test]
fn logical_mutation_describes_intent_from_write_set() {
    let registry = EmptyProcedureRegistry;
    let analyzed = analyze_with("INSERT (:Person { name: 'a' })", &registry);
    let summary = classify_analyzed(&analyzed);
    assert_eq!(summary.effect, selene_gql::LogicalEffect::Data);
    assert_eq!(summary.write_entry_count, 1);
    let logical = lower_logical(&analyzed, &registry).expect("lowers");
    let mutate = logical
        .operators
        .iter()
        .find_map(|op| match op {
            selene_gql::LogicalOp::Mutate { descriptor, .. } => Some(descriptor),
            _ => None,
        })
        .expect("mutation operator");
    assert_eq!(mutate.write_entry_count, 1);
    assert!(mutate.inserts_node);
}

#[test]
fn gp18_mixing_never_silently_splits() {
    // A synthetic mixed plan (data mutation plus catalog call) must fail
    // verification rather than route to one funnel and silently split.
    let read_registry = EmptyProcedureRegistry;
    let analyzed = analyze_with("RETURN 1 AS n", &read_registry);
    let mut mixed_plan = plan(&analyzed, &read_registry).expect("plans");
    // Inject both a data mutation and a catalog call.
    let data_analyzed = analyze_with("INSERT (:Mixed)", &read_registry);
    let data_plan = plan(&data_analyzed, &read_registry).expect("data plans");
    let mutation = data_plan
        .pipeline
        .iter()
        .find_map(|op| match op {
            PipelineOp::Mutation(mutation) => Some(mutation.clone()),
            _ => None,
        })
        .expect("mutation op");
    let catalog_registry = registry_with_mutability(ProcedureMutability::SchemaWrite);
    let catalog_analyzed = analyze_with("CALL pkg.proc() YIELD result", &catalog_registry);
    let catalog_plan = plan(&catalog_analyzed, &catalog_registry).expect("catalog plans");
    let [PipelineOp::Call(catalog_call)] = catalog_plan.pipeline.as_slice() else {
        panic!("expected call");
    };
    mixed_plan.pipeline.push(PipelineOp::Mutation(mutation));
    mixed_plan
        .pipeline
        .push(PipelineOp::Call(catalog_call.clone()));
    let summary = classify_plan(&mixed_plan);
    assert!(summary.is_mixed_data_catalog());
    assert!(check_gp18(&summary).is_err());
    let semantic = classify_analyzed(&analyzed);
    assert!(verify_plan_effects(&semantic, &mixed_plan).is_err());
}

#[test]
fn logical_explain_shows_stable_descriptors_without_addresses() {
    let registry = EmptyProcedureRegistry;
    let analyzed = analyze_with(
        "MATCH (n) WHERE n.age > 1 RETURN n.age AS age LIMIT 2",
        &registry,
    );
    let logical = lower_logical(&analyzed, &registry).expect("lowers");
    let text = explain_logical(&logical);
    assert!(text.contains("effect=query"), "{text}");
    assert!(text.contains("origin="), "{text}");
    // Stable semantic descriptors appear; runtime addresses never do.
    assert!(
        text.contains("scope=s") || text.contains("binding=b") || text.contains("predicate=e"),
        "{text}"
    );
    assert!(!text.contains("0x"), "{text}");

    let mutation_analyzed = analyze_with("INSERT (:Person)", &registry);
    let mutation_logical = lower_logical(&mutation_analyzed, &registry).expect("lowers");
    let mutation_text = explain_logical(&mutation_logical);
    assert!(mutation_text.contains("effect=data"), "{mutation_text}");
    assert!(mutation_text.contains("mutate"), "{mutation_text}");
    assert!(!mutation_text.contains("0x"), "{mutation_text}");
}

#[test]
#[allow(
    clippy::print_stderr,
    reason = "measurement output for the handoff, not production logging"
)]
fn lowering_cost_is_conservative_and_cheap() {
    let registry = EmptyProcedureRegistry;
    let analyzed = analyze_with(
        "MATCH (n:Person WHERE n.age > 1) RETURN n.name AS name LIMIT 10",
        &registry,
    );
    let (micros, sizes) = measure_lowering_cost(&analyzed, &registry);
    // Numbers are observed, not SLOs: report them for the handoff and keep
    // the computation to one linear pass over calls/write entries.
    eprintln!("lowering cost: {micros}us sizes={sizes:?}");
    assert_eq!(sizes.get("calls"), Some(&analyzed.calls.len()));
    assert_eq!(
        sizes.get("write_entries"),
        Some(
            &analyzed
                .write_set
                .as_ref()
                .map_or(0, |set| set.entries.len())
        )
    );
    // Conservative means bounded by the semantic input sizes, not by graph
    // content. A single-digit operator count for this slice proves no
    // per-row fanout happened during lowering.
    let operators = sizes.get("operators").copied().unwrap_or(usize::MAX);
    assert!(operators <= 8, "operators={operators}");
}

#[test]
fn planned_call_effects_resolve_from_metadata_not_names() {
    // Two procedures share no name relationship; only their registered
    // mutability distinguishes their effects. A name-based inference would
    // conflate them.
    let mut registry = MockProcedureRegistry::new();
    registry.insert_procedure_with_mutability(
        vec![db_string("pkg"), db_string("read_like_write")],
        Vec::new(),
        vec![ProcedureOutputColumn::new(
            db_string("out"),
            GqlType::String,
        )],
        ProcedureMutability::Read,
    );
    registry.insert_procedure_with_mutability(
        vec![db_string("pkg"), db_string("plain")],
        Vec::new(),
        vec![ProcedureOutputColumn::new(
            db_string("out"),
            GqlType::String,
        )],
        ProcedureMutability::SchemaWrite,
    );
    let read_analyzed = analyze_with("CALL pkg.read_like_write() YIELD out", &registry);
    let write_analyzed = analyze_with("CALL pkg.plain() YIELD out", &registry);
    assert_eq!(
        classify_analyzed(&read_analyzed).effect,
        selene_gql::LogicalEffect::Query
    );
    assert_eq!(
        classify_analyzed(&write_analyzed).effect,
        selene_gql::LogicalEffect::Catalog
    );
}

#[allow(dead_code, reason = "documents the planned-call shape used above")]
fn example_planned_call_shape() -> PlannedCall {
    PlannedCall {
        registry_version: 0,
        metadata: ProcedureMetadata::new(
            ProcedureHandle::new(1),
            Default::default(),
            Default::default(),
            ProcedureTier::Graph,
            ProcedureMutability::Read,
        ),
        optional: false,
        procedure: Box::new([db_string("pkg")]),
        handle: ProcedureHandle::new(1),
        args: Vec::new(),
        yield_cols: vec![PlannedYieldItem {
            column: YieldKind::Named(db_string("out")),
            alias: None,
            span: SourceSpan::default(),
        }],
        output_schema: ProcedureOutputSchema {
            columns: Vec::new(),
        },
        yield_schema: Vec::new(),
        tier: ProcedureTier::Graph,
        mutability: ProcedureMutability::Read,
        span: SourceSpan::default(),
    }
}

#[allow(
    dead_code,
    reason = "documents unused imports kept for contract clarity"
)]
fn contract_imports(
    _schema: BindingTableSchema,
    _plan: ExecutionPlan,
    _amount: LimitAmount,
    _metadata: ProcedureMetadata,
    _signature: ProcedureSignature,
    _parameter: ProcedureParameter,
    _category: StatementCategory,
) {
}
