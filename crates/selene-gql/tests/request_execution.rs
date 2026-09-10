//! Explicit facade-request runtime contract coverage.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc};

use selene_core::{DbString, GraphId, Value, db_string};
use selene_gql::{
    BuiltinProcedureRegistry, CallPlanCache, CatalogSessionOutput, EmptyProcedureRegistry,
    ExecutionOutcome, GqlType, RequestExecutionInput, RequestParameter, Session, StatementOutput,
    analyze, is_parameter_name, parse,
};
use selene_graph::SharedGraph;

fn parameter_name(name: &str) -> DbString {
    db_string(name).unwrap()
}

fn input(
    parameters: impl IntoIterator<Item = (&'static str, RequestParameter)>,
    timestamp: jiff::Timestamp,
    time_zone: jiff::tz::TimeZone,
) -> RequestExecutionInput {
    RequestExecutionInput::new(
        parameters
            .into_iter()
            .map(|(name, parameter)| (parameter_name(name), parameter))
            .collect::<BTreeMap<_, _>>(),
        timestamp,
        time_zone,
    )
}

fn execute(
    graph: &SharedGraph,
    source: &str,
    request: RequestExecutionInput,
) -> Result<CatalogSessionOutput, selene_gql::ExecutorError> {
    Session::new(graph).execute_source_catalog_request(source, &EmptyProcedureRegistry, request)
}

fn values(output: CatalogSessionOutput) -> Vec<Value> {
    let CatalogSessionOutput::RequestOutcome(ExecutionOutcome::RegularResult {
        table,
        declared,
        ..
    }) = output
    else {
        panic!("expected row output");
    };
    assert_eq!(table.row_count(), 1);
    assert_eq!(declared.fields().len(), table.schema().columns.len());
    table.rows()[0].values().to_vec()
}

fn statement_values(output: StatementOutput) -> Vec<Value> {
    let StatementOutput::Rows(table) = output else {
        panic!("expected row output");
    };
    assert_eq!(table.row_count(), 1);
    table.rows()[0].values().to_vec()
}

#[test]
fn parameter_name_helper_is_equivalent_to_source_grammar() {
    for name in ["value", "_2", "RETURN", "Δείγμα", "变量2"] {
        assert!(is_parameter_name(name), "{name:?}");
        parse(&format!("RETURN ${name}")).expect("accepted helper name parses after $");
    }
    for name in ["", "$value", "2value", "has-dash", "with space"] {
        assert!(!is_parameter_name(name), "{name:?}");
        if let Ok(statement) = parse(&format!("RETURN ${name}")) {
            let analyzed = analyze(statement, &EmptyProcedureRegistry, None);
            assert!(
                analyzed.is_err()
                    || analyzed
                        .unwrap()
                        .parameters
                        .iter()
                        .all(|parameter| parameter.name.as_str() != name),
                "no successful parse can decode the full invalid spelling {name:?}"
            );
        }
    }
}

#[test]
fn analyzer_contract_records_spans_and_inherits_source_declarations() {
    let source = "RETURN $x AS before, $x::INTEGER AS declared, $other";
    let statement = parse(source).unwrap();
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).unwrap();

    assert_eq!(analyzed.parameters.len(), 3);
    assert!(
        analyzed
            .parameters
            .windows(2)
            .all(|pair| pair[0].span.byte_offset <= pair[1].span.byte_offset)
    );
    let x = analyzed
        .parameters
        .iter()
        .filter(|parameter| parameter.name.as_str() == "x")
        .collect::<Vec<_>>();
    assert_eq!(x.len(), 2);
    assert!(
        x.iter()
            .all(|parameter| parameter.declared_type == Some(GqlType::Integer))
    );
    assert_eq!(
        analyzed.parameters[2].name.as_str(),
        "other",
        "bare uses remain in the source contract"
    );
    assert_eq!(analyzed.parameters[2].declared_type, None);
}

#[test]
fn request_adapter_matches_existing_row_executor_values_and_schema() {
    let graph = SharedGraph::new(GraphId::new(8_100));
    let source = "RETURN 7 AS answer, 'Ada' AS name";
    let direct = Session::new(&graph)
        .execute_source(source, &EmptyProcedureRegistry)
        .unwrap();
    let StatementOutput::Rows(direct) = direct else {
        panic!("direct executor should return rows");
    };
    let adapted = execute(
        &graph,
        source,
        input(
            [],
            jiff::Timestamp::new(1_788_692_096, 0).unwrap(),
            jiff::tz::TimeZone::UTC,
        ),
    )
    .unwrap();
    let CatalogSessionOutput::RequestOutcome(ExecutionOutcome::RegularResult {
        table,
        declared,
        ..
    }) = adapted
    else {
        panic!("request adapter should return regular rows");
    };

    assert_eq!(table, direct);
    assert_eq!(declared.fields().len(), direct.schema().columns.len());
    for (field, column) in declared.fields().iter().zip(&direct.schema().columns) {
        assert_eq!(field.name(), column.name.as_ref().map(DbString::as_str));
        assert_eq!(field.declared_type(), &column.ty);
    }
}

#[test]
fn request_preflight_handles_missing_inherited_shadowed_and_unused_parameters() {
    let graph = SharedGraph::new(GraphId::new(8101));
    let timestamp = jiff::Timestamp::new(1_788_692_096, 123).unwrap();
    let zone = jiff::tz::TimeZone::UTC;

    let missing = execute(
        &graph,
        "RETURN $missing",
        input([], timestamp, zone.clone()),
    )
    .unwrap_err();
    assert_eq!(missing.gqlstatus().as_str(), "22G03");

    let invalid_unused = execute(
        &graph,
        "RETURN 1",
        input(
            [(
                "unused",
                RequestParameter::new(GqlType::Integer, Value::String(parameter_name("wrong"))),
            )],
            timestamp,
            zone.clone(),
        ),
    )
    .unwrap_err();
    assert_eq!(invalid_unused.gqlstatus().as_str(), "22G03");

    let inherited = execute(
        &graph,
        "RETURN $value AS x_before, $value::INTEGER AS x_declared",
        input(
            [(
                "value",
                RequestParameter::new(GqlType::String, Value::String(parameter_name("text"))),
            )],
            timestamp,
            zone.clone(),
        ),
    )
    .unwrap_err();
    assert_eq!(inherited.gqlstatus().as_str(), "22G03");

    let output = execute(
        &graph,
        "RETURN $value",
        input(
            [
                (
                    "unused",
                    RequestParameter::new(GqlType::Integer, Value::Int(1)),
                ),
                (
                    "value",
                    RequestParameter::new(GqlType::Integer, Value::Int(2)),
                ),
            ],
            timestamp,
            zone,
        ),
    )
    .unwrap();
    assert_eq!(values(output), vec![Value::Int(2)]);
}

#[test]
fn inline_type_failure_precedes_insert_transaction_and_publication() {
    let graph = SharedGraph::new(GraphId::new(8102));
    let before = graph.read();
    let generation = before.meta.generation;
    assert_eq!(before.node_count(), 0);
    drop(before);

    let error = execute(
        &graph,
        "INSERT (:Bad { value: $value::INTEGER }) FINISH",
        input(
            [(
                "value",
                RequestParameter::new(GqlType::String, Value::String(parameter_name("bad"))),
            )],
            jiff::Timestamp::new(1_788_692_096, 0).unwrap(),
            jiff::tz::TimeZone::UTC,
        ),
    )
    .unwrap_err();
    assert_eq!(error.gqlstatus().as_str(), "22G03");
    let after = graph.read();
    assert_eq!(after.meta.generation, generation);
    assert_eq!(after.node_count(), 0);
    drop(after);

    let reference_error = execute(
        &graph,
        "INSERT (:Bad {value: $stale.value}) FINISH",
        input(
            [(
                "stale",
                RequestParameter::new(
                    GqlType::NodeRef,
                    Value::NodeRef(selene_core::NodeId::new(99)),
                ),
            )],
            jiff::Timestamp::new(1_788_692_096, 0).unwrap(),
            jiff::tz::TimeZone::UTC,
        ),
    )
    .unwrap_err();
    assert_eq!(reference_error.gqlstatus().as_str(), "22G11");
    let after_reference = graph.read();
    assert_eq!(after_reference.meta.generation, generation);
    assert_eq!(after_reference.node_count(), 0);
}

#[test]
fn one_supplied_instant_reaches_read_and_write_current_datetime_contexts() {
    let graph = SharedGraph::new(GraphId::new(8103));
    let timestamp = jiff::Timestamp::new(1_788_692_096, 123_456_789).unwrap();
    let offset = jiff::tz::Offset::from_seconds(3_600).unwrap();
    let zone = jiff::tz::TimeZone::fixed(offset);
    let zoned = timestamp.to_zoned(zone.clone());

    let read = execute(
        &graph,
        "RETURN CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, CURRENT_DATE, CURRENT_TIME, LOCAL_DATETIME(), LOCAL_TIME()",
        input([], timestamp, zone.clone()),
    )
    .unwrap();
    assert_eq!(
        values(read),
        vec![
            Value::ZonedDateTime(Box::new(zoned.clone())),
            Value::ZonedDateTime(Box::new(zoned.clone())),
            Value::Date(zoned.date()),
            Value::ZonedTime(Box::new(zoned.clone())),
            Value::LocalDateTime(zoned.datetime()),
            Value::LocalTime(zoned.time()),
        ]
    );

    let write = execute(
        &graph,
        "INSERT (n:Clock { at: CURRENT_TIMESTAMP }) RETURN n.at, CURRENT_TIMESTAMP",
        input([], timestamp, zone),
    )
    .unwrap();
    let CatalogSessionOutput::RequestOutcome(ExecutionOutcome::Written {
        write,
        declared: Some(declared),
        ..
    }) = write
    else {
        panic!("expected write output");
    };
    let table = write.rows.expect("write returns values");
    assert_eq!(declared.fields().len(), table.schema().columns.len());
    assert_eq!(
        table.rows()[0].values(),
        &[
            Value::ZonedDateTime(Box::new(zoned.clone())),
            Value::ZonedDateTime(Box::new(zoned)),
        ]
    );
}

#[test]
fn explicit_requests_bypass_plan_caches_that_omit_the_parameter_contract() {
    let graph = SharedGraph::new(GraphId::new(8104));
    let cache = Arc::new(CallPlanCache::new(NonZeroUsize::new(8).unwrap()));
    let registry = BuiltinProcedureRegistry::new();
    let timestamp = jiff::Timestamp::new(1_788_692_096, 0).unwrap();
    let source = "CALL selene.health() YIELD graph_id RETURN $value::INTEGER";

    let first = input(
        [(
            "value",
            RequestParameter::new(GqlType::Integer, Value::Int(1)),
        )],
        timestamp,
        jiff::tz::TimeZone::UTC,
    );
    Session::new(&graph)
        .with_call_plan_cache(Arc::clone(&cache))
        .execute_source_catalog_request(source, &registry, first)
        .unwrap();

    let second = input(
        [(
            "value",
            RequestParameter::new(GqlType::String, Value::String(parameter_name("wrong"))),
        )],
        timestamp,
        jiff::tz::TimeZone::UTC,
    );
    let error = Session::new(&graph)
        .with_call_plan_cache(Arc::clone(&cache))
        .execute_source_catalog_request(source, &registry, second)
        .unwrap_err();
    assert_eq!(error.gqlstatus().as_str(), "22G03");
    assert_eq!(cache.stats(), Default::default());
}

#[test]
fn request_fields_are_scoped_over_prior_parameters_and_time_zone() {
    let graph = SharedGraph::new(GraphId::new(8105));
    let timestamp = jiff::Timestamp::new(1_788_692_096, 0).unwrap();
    let request_zone =
        jiff::tz::TimeZone::fixed(jiff::tz::Offset::from_seconds(9 * 3_600).unwrap());
    let mut session = Session::new(&graph);
    session.bind_parameter(parameter_name("prior"), Value::Int(7));
    session
        .execute_source("SESSION SET TIME ZONE '+01:00'", &EmptyProcedureRegistry)
        .unwrap();

    let request_output = session
        .execute_source_catalog_request(
            "RETURN $prior, $request_only, CURRENT_TIMESTAMP",
            &EmptyProcedureRegistry,
            input(
                [
                    (
                        "prior",
                        RequestParameter::new(GqlType::Integer, Value::Int(99)),
                    ),
                    (
                        "request_only",
                        RequestParameter::new(GqlType::Integer, Value::Int(11)),
                    ),
                ],
                timestamp,
                request_zone.clone(),
            ),
        )
        .unwrap();
    assert_eq!(
        values(request_output),
        vec![
            Value::Int(99),
            Value::Int(11),
            Value::ZonedDateTime(Box::new(timestamp.to_zoned(request_zone))),
        ]
    );

    let ordinary = statement_values(
        session
            .execute_source("RETURN $prior, CURRENT_TIMESTAMP", &EmptyProcedureRegistry)
            .unwrap(),
    );
    assert_eq!(ordinary[0], Value::Int(7));
    let Value::ZonedDateTime(current) = &ordinary[1] else {
        panic!("expected zoned current timestamp");
    };
    assert_eq!(current.offset().seconds(), 3_600);
    let leaked = session
        .execute_source("RETURN $request_only", &EmptyProcedureRegistry)
        .unwrap_err();
    assert_eq!(leaked.gqlstatus().as_str(), "22G03");
}

#[test]
fn request_fields_restore_after_returned_error() {
    let graph = SharedGraph::new(GraphId::new(8106));
    let timestamp = jiff::Timestamp::new(1_788_692_096, 0).unwrap();
    let mut session = Session::new(&graph);
    session.bind_parameter(parameter_name("prior"), Value::Int(7));
    session
        .execute_source("SESSION SET TIME ZONE '+02:00'", &EmptyProcedureRegistry)
        .unwrap();

    let error = session
        .execute_source_catalog_request(
            "RETURN 1 / 0",
            &EmptyProcedureRegistry,
            input(
                [(
                    "request_only",
                    RequestParameter::new(GqlType::Integer, Value::Int(11)),
                )],
                timestamp,
                jiff::tz::TimeZone::UTC,
            ),
        )
        .unwrap_err();
    assert_eq!(error.gqlstatus().as_str(), "22012");

    let ordinary = statement_values(
        session
            .execute_source("RETURN $prior, CURRENT_TIMESTAMP", &EmptyProcedureRegistry)
            .unwrap(),
    );
    assert_eq!(ordinary[0], Value::Int(7));
    let Value::ZonedDateTime(current) = &ordinary[1] else {
        panic!("expected zoned current timestamp");
    };
    assert_eq!(current.offset().seconds(), 2 * 3_600);
    let leaked = session
        .execute_source("RETURN $request_only", &EmptyProcedureRegistry)
        .unwrap_err();
    assert_eq!(leaked.gqlstatus().as_str(), "22G03");
}

#[test]
fn ordinary_execution_uses_plan_cache_after_scoped_request() {
    let graph = SharedGraph::new(GraphId::new(8107));
    let mut session = Session::new(&graph).with_plan_cache(NonZeroUsize::new(8).expect("nonzero"));
    session
        .execute_source_catalog_request(
            "RETURN 1",
            &EmptyProcedureRegistry,
            input(
                [],
                jiff::Timestamp::new(1_788_692_096, 0).unwrap(),
                jiff::tz::TimeZone::UTC,
            ),
        )
        .unwrap();
    assert_eq!(session.plan_cache_stats().unwrap(), Default::default());

    session
        .execute_source("RETURN 1", &EmptyProcedureRegistry)
        .unwrap();
    session
        .execute_source("RETURN 1", &EmptyProcedureRegistry)
        .unwrap();
    let stats = session.plan_cache_stats().unwrap();
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.hits, 1);
}

#[test]
fn transaction_local_reference_passes_preflight_without_publication() {
    let graph = SharedGraph::new(GraphId::new(8108));
    let generation = graph.read().meta.generation;
    let mut session = Session::new(&graph);
    session.start_transaction().unwrap();
    session
        .execute_source("INSERT (:Pending) FINISH", &EmptyProcedureRegistry)
        .unwrap();

    let output = session
        .execute_source_catalog_request(
            "RETURN $value",
            &EmptyProcedureRegistry,
            input(
                [(
                    "value",
                    RequestParameter::new(
                        GqlType::NodeRef,
                        Value::NodeRef(selene_core::NodeId::new(1)),
                    ),
                )],
                jiff::Timestamp::new(1_788_692_096, 0).unwrap(),
                jiff::tz::TimeZone::UTC,
            ),
        )
        .unwrap();
    assert_eq!(
        values(output),
        vec![Value::NodeRef(selene_core::NodeId::new(1))]
    );
    session.rollback_transaction().unwrap();
    let published = graph.read();
    assert_eq!(published.meta.generation, generation);
    assert_eq!(published.node_count(), 0);
}
