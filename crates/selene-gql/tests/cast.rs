//! BRIEF-135a commit 1 + commit 2 acceptance bars — CAST(<expr> AS <type>)
//! parser, analyzer, walker, format, GQLSTATUS, and runtime ISO §22 dispatch
//! matrix coverage. The CONFORMANCE-00 inventory-honesty bars (CAST records
//! GA05 "Cast specification"; GA05 is runtime-supported, GE08 is not; corpus + CHANGELOG pins)
//! live in the sibling `cast_conformance.rs` so both files stay under the
//! 700-LOC cap.

// Execution-test subdomains live in sibling files to keep this test root
// under the 700-LOC cap; they reuse this binary's execute helpers.
#[path = "cast/exec_param_list_record.rs"]
mod exec_param_list_record;
#[path = "cast/exec_scalar.rs"]
mod exec_scalar;

use selene_core::{GraphId, Value, db_string};
use selene_gql::{
    AnalysisError, AnalyzedStatement, AnalyzedType, EmptyProcedureRegistry, GqlStatus, GqlType,
    PipelineStatement, ReturnItem, Session, Statement, StatementOutput, ValueExpr, analyze,
    ast::format::format_read_statement, parse,
};
use selene_graph::SharedGraph;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_or_panic(source: &str) -> Statement {
    parse(source).unwrap_or_else(|err| panic!("parse failed for `{source}`: {err:?}"))
}

fn analyze_one(source: &str) -> Result<AnalyzedStatement, AnalysisError> {
    analyze(parse_or_panic(source), &EmptyProcedureRegistry, None)
}

fn analyze_or_panic(source: &str) -> AnalyzedStatement {
    analyze_one(source).unwrap_or_else(|err| panic!("analyze failed for `{source}`: {err:?}"))
}

fn return_items(analyzed: &AnalyzedStatement) -> &[ReturnItem] {
    let Statement::Query(query) = analyzed.source() else {
        panic!("expected query statement");
    };
    query
        .statements
        .iter()
        .find_map(|statement| match statement {
            PipelineStatement::Return(clause) => Some(clause.items.as_slice()),
            _ => None,
        })
        .expect("RETURN clause exists")
}

fn first_return_item_type(analyzed: &AnalyzedStatement) -> AnalyzedType {
    let item = return_items(analyzed)
        .first()
        .expect("at least one RETURN item");
    let id = analyzed
        .expr_ids
        .get(&item.expr)
        .expect("RETURN item has an ExprId");
    analyzed.expr_types.get(id).clone()
}

fn first_return_item_expr(analyzed: &AnalyzedStatement) -> &ValueExpr {
    &return_items(analyzed)
        .first()
        .expect("at least one RETURN item")
        .expr
}

// ---------------------------------------------------------------------------
// §F commit 1 bars
// ---------------------------------------------------------------------------

#[test]
fn parser_accepts_cast_integer_to_string() {
    let stmt = parse_or_panic("RETURN CAST(1 AS STRING) AS s");
    // Confirm the parsed AST contains a Cast node, not a function-call lookalike.
    let analyzed = analyze(stmt, &EmptyProcedureRegistry, None).expect("analyzes");
    assert!(
        matches!(first_return_item_expr(&analyzed), ValueExpr::Cast { .. }),
        "expected ValueExpr::Cast, got {:?}",
        first_return_item_expr(&analyzed)
    );
}

#[test]
fn parser_accepts_cast_string_to_integer() {
    let analyzed = analyze_or_panic("RETURN CAST('42' AS INTEGER) AS n");
    assert!(matches!(
        first_return_item_expr(&analyzed),
        ValueExpr::Cast { .. }
    ));
}

#[test]
fn parser_accepts_cast_with_nested_expression() {
    // Outer CAST receives a sub-expression that already contains a CAST;
    // exercises the recursive `bind_value_expr` path through the variant.
    let analyzed = analyze_or_panic("RETURN CAST(CAST(1 AS STRING) AS INTEGER) AS n");
    let ValueExpr::Cast { value, .. } = first_return_item_expr(&analyzed) else {
        panic!("expected outer CAST");
    };
    assert!(
        matches!(value.as_ref(), ValueExpr::Cast { .. }),
        "expected nested CAST as inner value, got {value:?}",
    );
}

#[test]
fn parser_accepts_cast_with_list_target_type() {
    let analyzed = analyze_or_panic("RETURN CAST([1, 2, 3] AS LIST<INTEGER>) AS l");
    let ValueExpr::Cast { target_type, .. } = first_return_item_expr(&analyzed) else {
        panic!("expected CAST node");
    };
    assert!(
        matches!(target_type.as_ref(), GqlType::List(_)),
        "expected LIST target type, got {target_type:?}",
    );
}

#[test]
fn parser_rejects_cast_with_invalid_grammar() {
    // No `AS` keyword - the cast_expr rule fails to match and pest reports a
    // syntax error.
    let err = parse("RETURN CAST(1 STRING) AS x").expect_err("invalid CAST is rejected");
    let rendered = format!("{err:?}");
    assert!(
        !rendered.is_empty(),
        "expected non-empty parser error for invalid CAST grammar"
    );
}

#[test]
fn parser_accepts_cast_to_vector_target_type() {
    let analyzed = analyze_or_panic("RETURN CAST([1.0] AS VECTOR) AS v");
    let ValueExpr::Cast { target_type, .. } = first_return_item_expr(&analyzed) else {
        panic!("expected CAST node");
    };
    assert_eq!(target_type.as_ref(), &GqlType::Vector);
}

#[test]
fn vector_is_a_usable_bare_identifier() {
    // With the reserved `VECTOR` keyword removed, `vector` is an ordinary
    // identifier again and binds as a variable name.
    let statement = parse_or_panic("MATCH (vector) RETURN vector");
    assert!(matches!(statement, Statement::Query(_)));
}

#[test]
fn analyze_bind_cast_returns_target_type_integer() {
    let analyzed = analyze_or_panic("RETURN CAST('42' AS INTEGER) AS n");
    let ty = first_return_item_type(&analyzed);
    assert_eq!(ty, AnalyzedType::Resolved(GqlType::Integer));
}

#[test]
fn analyze_bind_cast_returns_target_type_string() {
    let analyzed = analyze_or_panic("RETURN CAST(1 AS STRING) AS s");
    let ty = first_return_item_type(&analyzed);
    assert_eq!(ty, AnalyzedType::Resolved(GqlType::String));
}

#[test]
fn analyze_bind_cast_preserves_nullability() {
    // ISO §22 explicit casts propagate NULL → NULL; the analyzer reports the
    // declared target type regardless of source-expression nullability (NULL
    // handling is a runtime concern). This test pins the analyzer contract:
    // CAST(NULL AS INTEGER) statically types as INTEGER, not Dynamic.
    let analyzed = analyze_or_panic("RETURN CAST(NULL AS INTEGER) AS n");
    let ty = first_return_item_type(&analyzed);
    assert_eq!(ty, AnalyzedType::Resolved(GqlType::Integer));
}

#[test]
fn analyze_hash_distinguishes_cast_integer_vs_float_target() {
    // F6 fold: the structural hash MUST hash `target_type` so that two CAST
    // nodes differing only in declared target type don't dedup against each
    // other in the ExprIdLookup. The contract is observable through
    // `expr_ids.len()`: parse two distinct CASTs in the same statement; both
    // get distinct ExprIds.
    let analyzed = analyze_or_panic(
        "RETURN CAST(1 AS INTEGER) AS i, CAST(1 AS FLOAT) AS f, CAST(1 AS STRING) AS s",
    );
    let items = return_items(&analyzed);
    assert_eq!(items.len(), 3, "three RETURN projections");
    let mut ids = std::collections::HashSet::new();
    for item in items {
        let id = analyzed
            .expr_ids
            .get(&item.expr)
            .expect("each CAST projection has an ExprId");
        assert!(
            ids.insert(id),
            "CAST projections with different target_type must hash to distinct ExprIds"
        );
    }
}

#[test]
fn walker_audit_16_sites_extended() {
    // F4 fold: 16 walker sites across the analyzer / planner / optimizer /
    // renderer / parser must include the new Cast variant. Each compile-error
    // failure caught by the `cargo build` gate proves a missing arm. This
    // smoke-level integration test confirms the load-bearing end-to-end
    // pipeline (parse → analyze → success) handles Cast everywhere it
    // matters; the exhaustive non-`_` matches enforce site coverage at
    // compile time. The 16th site is `parser/many.rs::rebase_value` (Stage 0
    // F4 missed it; the canonical count is 16, not 15).
    //
    // Nested + LIST + parameter + binary-op-over-cast exercise all the
    // recursive walker paths flagged in §A.4.
    let sources = [
        "RETURN CAST(1 AS STRING) AS a",
        "RETURN CAST(CAST(1 AS STRING) AS INTEGER) AS a",
        "RETURN CAST($p AS INTEGER) AS a",
        "RETURN CAST(1 AS INTEGER) + CAST(2 AS INTEGER) AS a",
        "RETURN CAST([1, 2, 3] AS LIST<INTEGER>) AS a",
    ];
    for source in sources {
        analyze_one(source)
            .unwrap_or_else(|err| panic!("walker audit failed for `{source}`: {err:?}"));
    }
}

#[test]
fn format_cast_round_trips_via_sibling_module() {
    // F3 fold: ast/format/cast.rs lives as a sibling module to keep
    // ast/format.rs under the 700-LOC cap. The contract is that the
    // formatter emits a `CAST(... AS ...)` round-trippable form via the
    // public `format_read_statement` path; reparsing yields a Cast node.
    let source = "RETURN CAST(1 AS STRING) AS s";
    let stmt = parse_or_panic(source);
    let rendered = format_read_statement(&stmt).expect("formats");
    assert!(
        rendered.contains("CAST"),
        "rendered form must contain `CAST`, got `{rendered}`"
    );
    assert!(
        rendered.contains(" AS "),
        "rendered form must contain ` AS `, got `{rendered}`"
    );
    assert!(
        rendered.contains("STRING"),
        "rendered form must contain target type `STRING`, got `{rendered}`"
    );
    // Re-parse the rendered form and confirm the outer RETURN item is still
    // a CAST. This pins both the format direction and parser round-trip.
    let reanalyzed = analyze_or_panic(&rendered);
    assert!(matches!(
        first_return_item_expr(&reanalyzed),
        ValueExpr::Cast { .. }
    ));
}

#[test]
fn gqlstatus_22018_invalid_character_value_for_cast_registered() {
    // F1 fold: GQLSTATUS 22018 is wired through both the canonical Table 8
    // map in selene-core and the `GqlStatus::INVALID_CHARACTER_VALUE_FOR_CAST`
    // const in selene-gql.
    let status = GqlStatus::INVALID_CHARACTER_VALUE_FOR_CAST;
    assert_eq!(status.as_str(), "22018");
    assert_eq!(&status.class(), b"22");
    // Round-trip lookup through the canonical Table 8 name registry.
    let name =
        selene_core::gqlstatus::gqlstatus_name("22018").expect("22018 is registered in Table 8");
    assert_eq!(name, "invalid-character-value-for-cast");
}

// ---------------------------------------------------------------------------
// §F commit 2 bars — runtime CAST evaluator ISO §22 dispatch matrix
// ---------------------------------------------------------------------------

fn execute_first_value(source: &str) -> Value {
    let graph = SharedGraph::new(GraphId::new(13_500));
    let mut session = Session::new(&graph);
    let output = session
        .execute_source(source, &EmptyProcedureRegistry)
        .unwrap_or_else(|err| panic!("execute failed for `{source}`: {err:?}"));
    let table = match output {
        StatementOutput::Rows(table) => table,
        other => panic!("`{source}` produced no row output: {other:?}"),
    };
    table
        .rows()
        .first()
        .and_then(|row| row.values().first())
        .cloned()
        .unwrap_or_else(|| panic!("`{source}` produced an empty row"))
}

fn execute_first_status(source: &str) -> String {
    let graph = SharedGraph::new(GraphId::new(13_501));
    let mut session = Session::new(&graph);
    session
        .execute_source(source, &EmptyProcedureRegistry)
        .expect_err("statement errors")
        .gqlstatus()
        .as_str()
        .to_owned()
}

fn as_string(value: Value) -> String {
    match value {
        Value::String(db_string) => db_string.as_str().to_owned(),
        other => panic!("expected string, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 810 — DECIMAL CAST arms + numeric-family source widening (end-to-end).
// These thread the widened/decimal source variants through the full
// parse → analyze → plan → execute pipeline. Sources that have no GQL literal
// (Uint / Int128 / Uint128 / Float32 / Decimal) are bound as session
// parameters. The DECIMAL conversion helpers themselves are unit-tested in
// `runtime/evaluator/cast/decimal.rs`.
// ---------------------------------------------------------------------------

fn execute_with_param(source: &str, name: &str, value: Value) -> Value {
    let graph = SharedGraph::new(GraphId::new(13_530));
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string(name).expect("db_string param"), value);
    let output = session
        .execute_source(source, &EmptyProcedureRegistry)
        .unwrap_or_else(|err| panic!("execute failed for `{source}`: {err:?}"));
    let StatementOutput::Rows(table) = output else {
        panic!("`{source}` produced no rows");
    };
    table
        .rows()
        .first()
        .and_then(|row| row.values().first())
        .cloned()
        .unwrap_or_else(|| panic!("`{source}` produced an empty row"))
}

fn execute_with_param_status(source: &str, name: &str, value: Value) -> String {
    let graph = SharedGraph::new(GraphId::new(13_531));
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string(name).expect("db_string param"), value);
    session
        .execute_source(source, &EmptyProcedureRegistry)
        .expect_err("statement errors")
        .gqlstatus()
        .as_str()
        .to_owned()
}
