//! Duration unit-group semantics across predicates, grouping, and indexes.

mod exec_common;

use exec_common::{column_values, execute_read};
use selene_core::{GraphId, Value, db_string};
use selene_gql::{EmptyProcedureRegistry, Session, StatementOutput};
use selene_graph::{SharedGraph, TypedIndexKind};

#[test]
fn equivalent_units_share_predicate_grouping_and_sort_identity() {
    for (left, right) in [("P1Y", "P12M"), ("P1W", "P7D"), ("PT1H", "PT60M")] {
        let table = execute_read(&format!(
            "RETURN DURATION '{left}' = DURATION '{right}' AS eq"
        ));
        assert_eq!(column_values(&table, "eq"), vec![Value::Bool(true)]);
        let table = execute_read(&format!(
            "FOR d IN [DURATION '{left}', DURATION '{right}'] RETURN DISTINCT d"
        ));
        assert_eq!(table.row_count(), 1);
        let table = execute_read(&format!(
            "FOR d IN [DURATION '{left}', DURATION '{right}'] RETURN d, count(*) AS n GROUP BY d"
        ));
        assert_eq!(column_values(&table, "n"), vec![Value::Int(2)]);
    }
    let table = execute_read(
        "RETURN DURATION 'P11M' < DURATION 'P1Y' AS lt, DURATION 'PT59M' < DURATION 'PT1H' AS dt",
    );
    assert_eq!(column_values(&table, "lt"), vec![Value::Bool(true)]);
    assert_eq!(column_values(&table, "dt"), vec![Value::Bool(true)]);
    let table = execute_read(
        "RETURN (DURATION 'PT1H' - DURATION 'PT60M') = DURATION 'P0M' AS z, DURATION 'P0M' < DURATION 'PT1H' AS dt",
    );
    assert_eq!(column_values(&table, "z"), vec![Value::Bool(true)]);
    assert_eq!(column_values(&table, "dt"), vec![Value::Bool(true)]);
}

fn graph(indexed: bool, composite: bool, mixed: bool) -> SharedGraph {
    let graph = SharedGraph::new(GraphId::new(99_871));
    let mut session = Session::new(&graph);
    session
        .execute_source(
            "INSERT (:Reading {duration: DURATION 'PT1H', tag: 1})",
            &EmptyProcedureRegistry,
        )
        .unwrap();
    if mixed {
        session
            .execute_source(
                "INSERT (:Reading {duration: DURATION 'P1M', tag: 1})",
                &EmptyProcedureRegistry,
            )
            .unwrap();
    }
    drop(session);
    if indexed {
        graph
            .create_property_index(
                db_string("Reading").unwrap(),
                db_string("duration").unwrap(),
                TypedIndexKind::Duration,
            )
            .unwrap();
    }
    if composite {
        let mut tx = graph.begin_write();
        tx.mutator()
            .create_composite_property_index_named(
                db_string("Reading").unwrap(),
                smallvec::smallvec![db_string("duration").unwrap(), db_string("tag").unwrap()],
                smallvec::smallvec![TypedIndexKind::Duration, TypedIndexKind::I64],
                None,
            )
            .unwrap();
        tx.commit().unwrap();
    }
    graph
}

#[test]
fn indexes_preserve_equivalent_unit_matches() {
    for (indexed, composite) in [(false, false), (true, false), (false, true)] {
        let graph = graph(indexed, composite, false);
        for predicate in [
            "duration = DURATION 'PT60M'",
            "duration >= DURATION 'PT60M'",
            "duration = DURATION 'PT60M' AND n.tag = 1",
        ] {
            let source = format!("MATCH (n:Reading) WHERE n.{predicate} RETURN n");
            let output = Session::new(&graph)
                .execute_source(&source, &EmptyProcedureRegistry)
                .unwrap();
            let StatementOutput::Rows(table) = output else {
                panic!("expected rows")
            };
            assert_eq!(table.row_count(), 1, "{indexed}/{composite}: {source}");
        }
    }
}

#[test]
fn indexes_preserve_incomparable_duration_errors() {
    for mixed in [false, true] {
        for (indexed, composite) in [(false, false), (true, false), (false, true)] {
            let graph = graph(indexed, composite, mixed);
            for predicate in [
                "duration = DURATION 'P1Y'",
                "duration > DURATION 'P1Y'",
                "duration = DURATION 'P1Y' AND n.tag = 1",
                "duration >= DURATION 'PT1H' AND n.duration <= DURATION 'P1Y'",
            ] {
                let source = format!("MATCH (n:Reading) WHERE n.{predicate} RETURN n");
                let error = Session::new(&graph)
                    .execute_source(&source, &EmptyProcedureRegistry)
                    .expect_err(&source);
                assert_eq!(
                    error.gqlstatus().as_str(),
                    "22G04",
                    "{mixed}/{indexed}/{composite}: {source}"
                );
            }
        }
    }
}

#[test]
fn zero_duration_is_comparable_in_both_index_groups() {
    for composite in [false, true] {
        let graph = graph(!composite, composite, false);
        for source in [
            "MATCH (n:Reading) WHERE n.duration > DURATION 'P0M' RETURN n",
            "MATCH (n:Reading) WHERE n.duration > DURATION 'PT0S' RETURN n",
        ] {
            let StatementOutput::Rows(table) = Session::new(&graph)
                .execute_source(source, &EmptyProcedureRegistry)
                .unwrap()
            else {
                panic!("rows")
            };
            assert_eq!(table.row_count(), 1);
        }
    }
}

#[test]
fn low_level_generic_duration_unique_reports_comparability_status_through_gql() {
    use selene_core::{LabelSet, PropertyValueType};
    use selene_graph::{GraphTypeDef, NodeTypeDef, PropertyTypeDef, ValidationMode};
    let name = |value| db_string(value).unwrap();
    let definition = GraphTypeDef {
        name: name("generic.unique"),
        node_types: vec![NodeTypeDef {
            name: name("Item"),
            key_labels: LabelSet::single(name("Item")),
            properties: vec![PropertyTypeDef {
                name: name("key"),
                value_type: PropertyValueType::Duration,
                list_element_type: None,
                required: false,
                default: None,
                immutable: false,
                unique: true,
                decimal_type: None,
                character_string_type: None,
                byte_string_type: None,
                record_field_types: None,
            }],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: vec![],
    };
    let graph = SharedGraph::builder(GraphId::new(99_872))
        .bound_to(definition)
        .unwrap()
        .build()
        .unwrap();
    let mut session = Session::new(&graph);
    session
        .execute_source(
            "INSERT (:Item {key: DURATION 'P1M'})",
            &EmptyProcedureRegistry,
        )
        .unwrap();
    let error = session
        .execute_source(
            "INSERT (:Item {key: DURATION 'PT1H'})",
            &EmptyProcedureRegistry,
        )
        .unwrap_err();
    assert_eq!(error.gqlstatus().as_str(), "22G04");
    assert_eq!(graph.read().node_count(), 1);
}
