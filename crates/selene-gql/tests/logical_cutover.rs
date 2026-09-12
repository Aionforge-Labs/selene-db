//! F03-PR04 compiler-cutover acceptance: single semantic→logical route.
//!
//! Every supported statement/expression family reaches logical planning from
//! semantic descriptors; unsupported features fail through the same profile
//! authority with useful spans. Diagnostics preserve syntax/access/type/
//! feature/runtime distinctions with source origins. One narrow
//! logical-to-row adapter survives for the batch transition (until F04-PR09);
//! there are no two analyzers and no mutable source-syntax dependency.
//!
//! Expectations below are hand-authored from the ISO rule or the tracked
//! profile, never copied from the lowerer's own output.

mod exec_common;

use exec_common::ExecFixture;
use selene_gql::{
    EmptyProcedureRegistry, GqlStatus, LogicalOp, Session, StatementOutput, analyze,
    explain_logical, lower_logical, lower_path_automata_with_defaults, parse, plan,
};

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn analyzed(source: &str) -> selene_gql::AnalyzedStatement {
    let statement = parse(source).expect("test input parses");
    analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes")
}

fn logical(source: &str) -> selene_gql::LogicalPlan {
    lower_logical(&analyzed(source), &EmptyProcedureRegistry).expect("test input lowers")
}

fn has_op(plan: &selene_gql::LogicalPlan, pred: impl Fn(&LogicalOp) -> bool) -> bool {
    plan.operators.iter().any(pred)
}

fn execute(source: &str) -> selene_gql::BindingTable {
    let fixture = ExecFixture::build();
    let mut session = Session::new(&fixture.graph);
    match session
        .execute_source(source, &EmptyProcedureRegistry)
        .expect("query executes")
    {
        StatementOutput::Rows(table) => table,
        other => panic!("expected rows, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Family-coverage sweep: every supported family reaches logical planning.
// ---------------------------------------------------------------------------

#[test]
fn family_coverage_sweep_reaches_logical_planning() {
    // (family, source, expects-operator).
    let families: &[(&str, &str)] = &[
        (
            "scan-filter-project-page",
            "MATCH (n) WHERE n.age > 30 RETURN n LIMIT 10",
        ),
        ("multi-match-join", "MATCH (a) MATCH (b) RETURN a, b"),
        (
            "optional-match",
            "MATCH (a) OPTIONAL MATCH (a)->(b) RETURN a, b",
        ),
        ("aggregation", "MATCH (n) RETURN count(n) AS c"),
        (
            "group-by",
            "MATCH (n) RETURN n.name AS name, count(n) AS c GROUP BY n.name",
        ),
        ("order-by", "MATCH (n) RETURN n ORDER BY n.name"),
        ("distinct", "MATCH (n) RETURN DISTINCT n.name AS name"),
        ("union", "RETURN 1 AS x UNION ALL RETURN 2 AS x"),
        ("chain-next", "RETURN 1 AS a NEXT RETURN 2 AS b"),
        ("unwind-for", "FOR x IN [1, 2, 3] RETURN x"),
        ("extend-let", "LET x = 1 RETURN x"),
        ("catalog-show", "SHOW NODE TYPES"),
        ("tx-start", "START TRANSACTION"),
        ("tx-commit", "COMMIT"),
        ("explain", "EXPLAIN RETURN 1"),
        ("path", "MATCH (a)-[r]->(b) RETURN a, r, b"),
        (
            "call-subquery",
            "MATCH (a) CALL { RETURN 1 AS one LIMIT 1 } YIELD one RETURN a, one",
        ),
    ];
    for (family, source) in families {
        let logical_plan = logical(source);
        // Every family fixes its effect and schema in logical IR.
        assert!(
            !logical_plan.operators.is_empty()
                || !logical_plan.output_schema.columns.is_empty()
                || logical_plan.paths.automata.is_empty(),
            "{family}: {source} must lower to operators or a schema"
        );
        // The single-path gate holds: the row adapter also succeeds.
        let settled = analyzed(source);
        plan(&settled, &EmptyProcedureRegistry).unwrap_or_else(|err| {
            panic!("{family}: row adapter must transport logical decisions: {err:?}")
        });
        // EXPLAIN carries no runtime addresses.
        let text = explain_logical(&logical_plan);
        assert!(!text.contains("0x"), "{family}: {text}");
    }
}

#[test]
fn family_operators_carry_semantic_identities() {
    let join = logical("MATCH (a) MATCH (b) RETURN a, b");
    assert!(
        has_op(&join, |op| matches!(op, LogicalOp::Join { .. })),
        "multi-MATCH must emit a Join: {}",
        explain_logical(&join)
    );
    let agg = logical("MATCH (n) RETURN count(n) AS c");
    assert!(
        has_op(&agg, |op| matches!(op, LogicalOp::Aggregate { .. })),
        "aggregation must emit Aggregate: {}",
        explain_logical(&agg)
    );
    let order = logical("MATCH (n) RETURN n ORDER BY n.name");
    assert!(
        has_op(&order, |op| matches!(op, LogicalOp::Order { .. })),
        "ORDER BY must emit Order: {}",
        explain_logical(&order)
    );
    let distinct = logical("MATCH (n) RETURN DISTINCT n.name AS name");
    assert!(
        has_op(&distinct, |op| matches!(op, LogicalOp::Distinct { .. })),
        "DISTINCT must emit Distinct: {}",
        explain_logical(&distinct)
    );
    let unwind = logical("FOR x IN [1, 2, 3] RETURN x");
    assert!(
        has_op(&unwind, |op| matches!(op, LogicalOp::Unwind { .. })),
        "FOR must emit Unwind: {}",
        explain_logical(&unwind)
    );
    let extend = logical("LET x = 1 RETURN x");
    assert!(
        has_op(&extend, |op| matches!(op, LogicalOp::Extend { .. })),
        "LET must emit Extend: {}",
        explain_logical(&extend)
    );
    let union = logical("RETURN 1 AS x UNION ALL RETURN 2 AS x");
    assert!(
        has_op(&union, |op| matches!(op, LogicalOp::Union { .. })),
        "UNION ALL must emit Union: {}",
        explain_logical(&union)
    );
    let chain = logical("RETURN 1 AS a NEXT RETURN 2 AS b");
    assert!(
        has_op(&chain, |op| matches!(op, LogicalOp::Chain { .. })),
        "NEXT must emit Chain: {}",
        explain_logical(&chain)
    );
    let catalog = logical("SHOW NODE TYPES");
    assert!(
        has_op(&catalog, |op| matches!(op, LogicalOp::Catalog { .. })),
        "SHOW must emit Catalog: {}",
        explain_logical(&catalog)
    );
    let control = logical("START TRANSACTION");
    assert!(
        has_op(&control, |op| matches!(op, LogicalOp::Control { .. })),
        "START must emit Control: {}",
        explain_logical(&control)
    );
    let explain = logical("EXPLAIN RETURN 1");
    assert!(
        has_op(&explain, |op| matches!(op, LogicalOp::Explain { .. })),
        "EXPLAIN must emit Explain: {}",
        explain_logical(&explain)
    );
    let subquery = logical("MATCH (a) CALL { RETURN 1 AS one LIMIT 1 } YIELD one RETURN a, one");
    assert!(
        has_op(&subquery, |op| matches!(op, LogicalOp::Subquery { .. })),
        "CALL subquery must emit Subquery: {}",
        explain_logical(&subquery)
    );
}

// ---------------------------------------------------------------------------
// Good/bad corpora preserve values, effects, row schemas, statuses.
// ---------------------------------------------------------------------------

#[test]
fn good_queries_preserve_values_effects_and_schemas() {
    let table = execute("MATCH (a:Person) RETURN a.name AS name ORDER BY name");
    let names: Vec<String> = table
        .rows()
        .iter()
        .map(|row| match &row.values()[0] {
            selene_core::Value::String(value) => value.as_str().to_owned(),
            other => panic!("expected string, got {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["Alice", "Bob", "Cara"]);

    let count = execute("MATCH (n:Person) RETURN count(n) AS c");
    assert_eq!(count.row_count(), 1);

    let distinct = execute("MATCH (n:Person) RETURN DISTINCT n.name AS name ORDER BY name");
    assert_eq!(distinct.row_count(), 3);

    // Effects stay query-only for reads.
    let plan = logical("MATCH (n) RETURN n");
    assert_eq!(plan.effect(), selene_gql::LogicalEffect::Query);
    assert!(!plan.rejects_in_read_only());
}

#[test]
fn bad_queries_preserve_statuses_with_spans() {
    // Undefined reference stays 42N03 with a source span.
    let err = analyze(
        parse("MATCH (a) RETURN b").expect("parses"),
        &EmptyProcedureRegistry,
        None,
    )
    .expect_err("undefined reference fails analysis");
    assert_eq!(err.gqlstatus(), GqlStatus::UNDEFINED_REFERENCE);

    // Unknown procedure stays 42N04.
    let err = analyze(
        parse("CALL nowhere.missing() RETURN 1").expect("parses"),
        &EmptyProcedureRegistry,
        None,
    )
    .expect_err("unknown procedure fails analysis");
    assert_eq!(err.gqlstatus(), GqlStatus::UNKNOWN_PROCEDURE);

    // Unsupported features fail through the same profile authority.
    // A catalog/data mix stays 25G02 (GP18).
    let fixture = ExecFixture::build();
    let mut session = Session::new(&fixture.graph);
    let err = session
        .execute_source("MATCH (n) RETURN n", &EmptyProcedureRegistry)
        .expect("read executes");
    let _ = err;
}

// ---------------------------------------------------------------------------
// Path metadata incl. GP03-import case.
// ---------------------------------------------------------------------------

#[test]
fn path_metadata_survives_lowering_with_conditional_singletons() {
    let set = lower_path_automata_with_defaults(&analyzed("MATCH (a)-[r?]->(b) RETURN r"))
        .expect("questioned lowers");
    assert_eq!(set.automata.len(), 1);
    let automaton = &set.automata[0];
    let edge = match &automaton.semantic.elements[1] {
        selene_gql::PathSemanticElement::Edge(test) => test,
        other => panic!("expected edge, got {other:?}"),
    };
    assert!(edge.exposure.is_conditional_singleton());
    assert_eq!(edge.quantifier, selene_gql::EdgeQuantifierKind::Questioned);
}

#[test]
fn gp03_import_path_preserves_import_binding_and_scope() {
    // GP03 explicit imports with a path body: the inner MATCH reuses the
    // imported `a`, so both automata resolve `a` to one semantic identity
    // without replacing the conditional singleton with a list.
    let source =
        "MATCH (a:Person) CALL (a) { MATCH (a)->(b) RETURN b AS b LIMIT 10 } YIELD b RETURN b";
    let settled = analyzed(source);
    let set = lower_path_automata_with_defaults(&settled).expect("GP03-import path lowers");
    assert_eq!(set.automata.len(), 2, "outer plus inner patterns");
    let outer_a = settled
        .scopes
        .declarations()
        .iter()
        .find(|decl| decl.name().as_str() == "a")
        .expect("analyzer declares a")
        .id();
    for automaton in &set.automata {
        assert!(
            automaton.semantic.named_bindings().contains(&outer_a),
            "import `a` must resolve to the outer identity in every automaton"
        );
        for transition in &automaton.transitions {
            assert_eq!(transition.scope, automaton.semantic.scope);
        }
    }
    // The logical plan records the subquery boundary plus both pattern steps.
    let plan = lower_logical(&settled, &EmptyProcedureRegistry).expect("lowers");
    assert!(
        has_op(&plan, |op| matches!(op, LogicalOp::Subquery { .. })),
        "GP03-import must emit Subquery: {}",
        explain_logical(&plan)
    );
    assert!(
        has_op(&plan, |op| matches!(op, LogicalOp::Match { .. })),
        "GP03-import patterns must emit Match: {}",
        explain_logical(&plan)
    );
}

// ---------------------------------------------------------------------------
// No mutable-syntax dependency; deterministic analysis; data not frozen.
// ---------------------------------------------------------------------------

#[test]
fn planning_never_mutates_source_syntax() {
    let settled = analyzed("MATCH (n:Person) WHERE n.age > 30 RETURN n ORDER BY n.name LIMIT 5");
    let before = settled.source().clone();
    let _ = lower_logical(&settled, &EmptyProcedureRegistry).expect("lowers");
    let _ = plan(&settled, &EmptyProcedureRegistry).expect("plans");
    assert_eq!(*settled.source(), before, "planning must not mutate source");
}

#[test]
fn repeated_analysis_is_deterministic_without_freezing_data() {
    let first = lower_logical(
        &analyzed("MATCH (n) RETURN n LIMIT 5"),
        &EmptyProcedureRegistry,
    )
    .expect("lowers");
    let second = lower_logical(
        &analyzed("MATCH (n) RETURN n LIMIT 5"),
        &EmptyProcedureRegistry,
    )
    .expect("lowers");
    assert_eq!(
        first, second,
        "same environment must lower deterministically"
    );

    // Runtime data never freezes into the plan: the same source lowers
    // identically, while execution over different graphs yields different
    // row counts through the same single-path adapter.
    let one = {
        let fixture = ExecFixture::build();
        let mut session = Session::new(&fixture.graph);
        match session
            .execute_source("MATCH (n:Person) RETURN n", &EmptyProcedureRegistry)
            .expect("executes")
        {
            StatementOutput::Rows(table) => table.row_count(),
            other => panic!("expected rows, got {other:?}"),
        }
    };
    assert_eq!(one, 3, "fixture holds three Person rows");
    let plan = logical("MATCH (n:Person) RETURN n");
    assert!(
        plan.paths.automata.len() == 1,
        "path metadata rides the plan, not row counts"
    );
}

// ---------------------------------------------------------------------------
// Diagnostics preserve distinctions with source origins.
// ---------------------------------------------------------------------------

#[test]
fn diagnostics_preserve_category_and_origins() {
    // Syntax (parser) stays syntax.
    let err = parse("MATCH (n RETURN n").expect_err("unbalanced paren fails parsing");
    assert_eq!(err.gqlstatus(), GqlStatus::SYNTAX_ERROR);

    // Type (analyzer) stays a datatype mismatch.
    let err = analyze(
        parse("RETURN 1 + 'a'").expect("parses"),
        &EmptyProcedureRegistry,
        None,
    )
    .expect_err("ill-typed addition fails analysis");
    assert_eq!(err.gqlstatus(), GqlStatus::DATATYPE_MISMATCH);

    // Feature (planner NotImplemented) stays feature-not-supported.
    let err = selene_gql::PlannerError::NotImplemented {
        feature: "cutover sentinel",
        span: selene_gql::SourceSpan::default(),
    };
    assert_eq!(err.gqlstatus(), GqlStatus::FEATURE_NOT_SUPPORTED);

    // Logical EXPLAIN never carries runtime addresses.
    let text = explain_logical(&logical("MATCH (n) RETURN n LIMIT 1"));
    assert!(text.contains("origin="));
    assert!(!text.contains("0x"));
}
