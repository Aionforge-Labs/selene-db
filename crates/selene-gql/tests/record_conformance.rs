//! RECORD field-name-equality conformance tests (GQLRT-14).
//!
//! ISO/IEC 39075:2024 §4.15 defines record equality via a field-name bijection,
//! not a positional zip: `{a:1, b:2}` equals `{b:2, a:1}`. These end-to-end
//! tests pin that `=` over record literals and DISTINCT / GROUP BY over permuted
//! records honour the bijection, reaching the runtime through record literals.

mod exec_common;

use selene_core::Value;
use selene_gql::{GqlStatus, parse};

use exec_common::{column_values, execute_read};

#[test]
fn record_equality_ignores_field_order() {
    let table = execute_read("RETURN {a: 1, b: 2} = {b: 2, a: 1} AS eq");

    assert_eq!(column_values(&table, "eq"), vec![Value::Bool(true)]);
}

#[test]
fn differing_record_field_sets_are_not_comparable_without_ga04() {
    let error = selene_gql::analyze(
        parse("RETURN {a: 1} = {a: 1, b: 2} AS eq").unwrap(),
        &selene_gql::EmptyProcedureRegistry,
        None,
    )
    .unwrap_err();
    assert_eq!(error.gqlstatus().as_str(), "22G03");
}

#[test]
fn record_equality_with_null_field_is_unknown() {
    let table = execute_read("RETURN ({a: 1, b: null} = {b: null, a: 1}) IS NULL AS \"unknown\"");

    assert_eq!(column_values(&table, "unknown"), vec![Value::Bool(true)]);
}

#[test]
fn distinct_over_permuted_records_collapses() {
    let table = execute_read("FOR r IN [{a: 1, b: 2}, {b: 2, a: 1}] RETURN DISTINCT r");

    assert_eq!(
        table.row_count(),
        1,
        "permuted records are one DISTINCT value"
    );
}

#[test]
fn distinct_keeps_records_with_different_field_values_apart() {
    let table = execute_read("FOR r IN [{a: 1, b: 2}, {a: 1, b: 3}] RETURN DISTINCT r");

    assert_eq!(table.row_count(), 2);
}

#[test]
fn group_by_collapses_permuted_record_keys() {
    let table = execute_read(
        "FOR r IN [{a: 1, b: 2}, {b: 2, a: 1}, {a: 9, b: 9}] \
         RETURN r AS key, count(*) AS n GROUP BY r",
    );

    // The two permuted records share a group; the distinct record is its own.
    assert_eq!(table.row_count(), 2);
    let counts = column_values(&table, "n");
    assert!(counts.contains(&Value::Int(2)));
    assert!(counts.contains(&Value::Int(1)));
}

#[test]
fn closed_record_type_rejects_duplicate_field_names() {
    for source in [
        "RETURN CAST({a: 1} AS RECORD{a :: INT, a :: STRING}) AS r",
        "RETURN {a: 1} IS TYPED RECORD{a :: INT, a :: STRING} AS ok",
        "RETURN $r :: RECORD{a :: INT, a :: STRING} AS r",
        "CREATE NODE TYPE :Host (config :: RECORD{a :: INT, a :: STRING})",
    ] {
        let err = parse(source).unwrap_err();
        assert_eq!(err.gqlstatus(), GqlStatus::SYNTAX_ERROR, "{source}");
        assert!(
            err.to_string()
                .contains("duplicate record field type name: a"),
            "{source}: {err:?}"
        );
    }
}

#[test]
fn nested_closed_record_type_rejects_duplicate_field_names() {
    let err = parse(
        "RETURN CAST({outer: {a: 1}} AS RECORD{outer :: RECORD{a :: INT, a :: STRING}}) AS r",
    )
    .unwrap_err();

    assert_eq!(err.gqlstatus(), GqlStatus::SYNTAX_ERROR);
    assert!(
        err.to_string()
            .contains("duplicate record field type name: a"),
        "{err:?}"
    );
}
