//! Explicit value-type nullability coverage (GV90).

use selene_core::{GraphId, Value};
use selene_gql::{
    EmptyProcedureRegistry, ExecutorError, GqlStatus, GqlType, PipelineStatement, Session,
    Statement, StatementOutput, ValueExpr, ast::format_read_statement, feature_walk, parse,
};
use selene_graph::{GraphTypeDef, PropertyElementType, RecordFieldType, SharedGraph};
use selene_profile::FeatureId;

fn db_string(value: &str) -> selene_core::DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn one_value(source: &str) -> Value {
    let graph = SharedGraph::new(GraphId::new(9010));
    let mut session = Session::new(&graph);
    let output = session
        .execute_source(source, &EmptyProcedureRegistry)
        .expect("statement succeeds");
    first_value(output)
}

fn first_value(output: StatementOutput) -> Value {
    let table = match output {
        StatementOutput::Rows(table) => table,
        StatementOutput::Written(outcome) => outcome.rows.expect("write returned rows"),
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(table.row_count(), 1);
    table.rows()[0].values()[0].clone()
}

fn closed_graph(id: u64) -> SharedGraph {
    SharedGraph::builder(GraphId::new(id))
        .bound_to(GraphTypeDef {
            name: db_string("nullability.test.graph"),
            node_types: Vec::new(),
            edge_types: Vec::new(),
        })
        .expect("closed graph type is valid")
        .build()
        .expect("closed graph builds")
}

#[test]
fn not_null_type_names_round_trip_through_parser_and_formatter() {
    let parsed = parse("RETURN NULL IS TYPED STRING NOT NULL AS ok").expect("source parses");
    let Statement::Query(pipeline) = &parsed else {
        panic!("expected query");
    };
    let PipelineStatement::Return(clause) = &pipeline.statements[0] else {
        panic!("expected return");
    };
    let ValueExpr::IsCheck { kind, .. } = &clause.items[0].expr else {
        panic!("expected typed predicate");
    };
    let selene_gql::IsCheckKind::Typed(GqlType::NotNull(inner)) = kind else {
        panic!("expected NOT NULL type marker");
    };
    assert_eq!(**inner, GqlType::String);
    assert_eq!(
        format_read_statement(&parsed).expect("statement formats"),
        "RETURN null IS TYPED STRING NOT NULL AS ok"
    );
}

#[test]
fn feature_walk_records_gv90_and_inner_type_features() {
    let parsed = parse("RETURN $x :: UINT8 NOT NULL AS x").expect("source parses");
    let features = feature_walk(&parsed)
        .into_iter()
        .map(|use_| use_.feature_id)
        .collect::<Vec<_>>();
    assert!(features.contains(&FeatureId::GV90), "{features:?}");
    assert!(features.contains(&FeatureId::GV01), "{features:?}");
    assert!(features.contains(&FeatureId::GV09), "{features:?}");
    assert!(
        features.contains(&FeatureId::IM_TYPED_PARAMS),
        "{features:?}"
    );

    let parsed = parse("RETURN 1 AS n LIMIT $limit :: UINT8 NOT NULL").expect("source parses");
    let features = feature_walk(&parsed)
        .into_iter()
        .map(|use_| use_.feature_id)
        .collect::<Vec<_>>();
    assert!(features.contains(&FeatureId::GV90), "{features:?}");
    assert!(features.contains(&FeatureId::GV01), "{features:?}");
    assert!(features.contains(&FeatureId::GV09), "{features:?}");
    assert!(
        features.contains(&FeatureId::IM_TYPED_PARAMS),
        "{features:?}"
    );
}

#[test]
fn typed_predicates_treat_unspecified_value_types_as_nullable() {
    let cases = [
        ("RETURN NULL IS TYPED STRING AS ok", true),
        ("RETURN NULL IS TYPED STRING NOT NULL AS ok", false),
        ("RETURN NULL IS NOT TYPED STRING NOT NULL AS ok", true),
        ("RETURN [NULL] IS TYPED LIST<INTEGER> AS ok", true),
        ("RETURN [NULL] IS TYPED LIST<INTEGER NOT NULL> AS ok", false),
        ("RETURN {a: NULL} IS TYPED RECORD{a :: INTEGER} AS ok", true),
        (
            "RETURN {a: NULL} IS TYPED RECORD{a :: INTEGER NOT NULL} AS ok",
            false,
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(one_value(source), Value::Bool(expected), "{source}");
    }
    for source in [
        "RETURN NULL IS TYPED NULL AS ok",
        "RETURN NULL IS TYPED NOTHING AS ok",
    ] {
        let error =
            parse(source).expect_err("optional immaterial type syntax is not runtime-supported");
        assert_eq!(error.gqlstatus(), GqlStatus::FEATURE_NOT_SUPPORTED);
    }
}

#[test]
fn cast_to_not_null_value_type_rejects_null_and_casts_non_null_sources() {
    assert_eq!(one_value("RETURN CAST(NULL AS STRING) AS v"), Value::Null);
    assert_eq!(
        one_value("RETURN CAST('42' AS INT NOT NULL) AS v"),
        Value::Int(42)
    );

    let graph = SharedGraph::new(GraphId::new(9011));
    let mut session = Session::new(&graph);
    let err = session
        .execute_source(
            "RETURN CAST(NULL AS STRING NOT NULL) AS v",
            &EmptyProcedureRegistry,
        )
        .expect_err("not-null cast rejects NULL");
    assert_eq!(err.gqlstatus(), GqlStatus::NULL_VALUE_NOT_ALLOWED);

    let err = session
        .execute_source(
            "RETURN CAST([1, NULL] AS LIST<INT NOT NULL>) AS v",
            &EmptyProcedureRegistry,
        )
        .expect_err("nested not-null list cast rejects NULL element");
    assert_eq!(err.gqlstatus(), GqlStatus::NULL_VALUE_NOT_ALLOWED);
}

#[test]
fn typed_parameters_enforce_not_null_marker() {
    let graph = SharedGraph::new(GraphId::new(9012));
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("p"), Value::Null);
    assert_eq!(
        first_value(
            session
                .execute_source("RETURN $p :: STRING AS v", &EmptyProcedureRegistry)
                .expect("nullable typed parameter accepts NULL")
        ),
        Value::Null
    );

    let err = session
        .execute_source("RETURN $p :: STRING NOT NULL AS v", &EmptyProcedureRegistry)
        .expect_err("not-null typed parameter rejects NULL");
    assert!(matches!(
        err,
        ExecutorError::InvalidParameterType {
            ref expected,
            actual: "NULL",
            ..
        } if expected == "STRING NOT NULL"
    ));

    session.bind_parameter(db_string("p"), Value::String(db_string("ok")));
    assert_eq!(
        first_value(
            session
                .execute_source("RETURN $p :: STRING NOT NULL AS v", &EmptyProcedureRegistry)
                .expect("not-null typed parameter accepts string")
        ),
        Value::String(db_string("ok"))
    );
}

#[test]
fn limit_typed_parameters_accept_not_null_numeric_types() {
    let graph = SharedGraph::new(GraphId::new(9015));
    let mut session = Session::new(&graph);
    session.bind_parameter(db_string("limit"), Value::Uint(1));

    assert_eq!(
        first_value(
            session
                .execute_source(
                    "RETURN 1 AS n LIMIT $limit :: UINT8 NOT NULL",
                    &EmptyProcedureRegistry,
                )
                .expect("not-null numeric limit parameter executes")
        ),
        Value::Int(1)
    );
}

#[test]
fn catalog_lowers_top_level_not_null_to_required_property() {
    let graph = closed_graph(9013);
    let mut session = Session::new(&graph);
    session
        .execute_source(
            "CREATE NODE TYPE :Thing (name :: STRING NOT NULL)",
            &EmptyProcedureRegistry,
        )
        .expect("top-level NOT NULL property creates");

    let graph_type = graph.graph_type().expect("graph has type");
    assert!(graph_type.node_types[0].properties[0].required);

    let show = session
        .execute_source("SHOW NODE TYPES", &EmptyProcedureRegistry)
        .expect("show succeeds");
    let StatementOutput::Rows(table) = show else {
        panic!("SHOW returns rows");
    };
    assert_eq!(table.row_count(), 1);
    let definition = table.rows()[0].values()[1].clone();
    let Value::String(definition) = definition else {
        panic!("definition is string");
    };
    assert_eq!(
        definition.as_str(),
        "CREATE NODE TYPE :Thing (name :: STRING NOT NULL)"
    );
}

#[test]
fn catalog_persists_nested_property_nullability_descriptors() {
    let graph = closed_graph(9014);
    let mut session = Session::new(&graph);
    session
        .execute_source(
            "CREATE NODE TYPE :Thing (\
             nullable_xs :: LIST<INT>, \
             strict_xs :: LIST<INT NOT NULL>, \
             payload :: RECORD{\
                 nullable :: INT, \
                 strict :: INT NOT NULL, \
                 labels :: LIST<STRING NOT NULL>})",
            &EmptyProcedureRegistry,
        )
        .expect("nested catalog nullability creates");

    let graph_type = graph.graph_type().expect("graph has type");
    let properties = &graph_type.node_types[0].properties;
    assert!(matches!(
        properties[0].list_element_type.as_ref(),
        Some(PropertyElementType::Scalar(
            selene_core::PropertyValueType::Int
        ))
    ));
    assert!(matches!(
        properties[1].list_element_type.as_ref(),
        Some(PropertyElementType::NotNull(inner))
            if matches!(inner.as_ref(), PropertyElementType::Scalar(selene_core::PropertyValueType::Int))
    ));
    let record_fields = properties[2]
        .record_field_types
        .as_ref()
        .expect("record fields");
    assert!(!record_fields.0[0].required);
    assert!(record_fields.0[1].required);
    assert!(matches!(
        &record_fields.0[2].field_type,
        RecordFieldType::List(inner)
            if matches!(
                inner.as_ref(),
                RecordFieldType::NotNull(strict)
                    if matches!(
                        strict.as_ref(),
                        RecordFieldType::Scalar(selene_core::PropertyValueType::String)
                    )
            )
    ));

    let show = session
        .execute_source("SHOW NODE TYPES", &EmptyProcedureRegistry)
        .expect("show succeeds");
    let StatementOutput::Rows(table) = show else {
        panic!("SHOW returns rows");
    };
    let Value::String(definition) = table.rows()[0].values()[1].clone() else {
        panic!("definition is string");
    };
    assert_eq!(
        definition.as_str(),
        "CREATE NODE TYPE :Thing (nullable_xs :: LIST<INTEGER>, strict_xs :: LIST<INTEGER NOT NULL>, payload :: RECORD { nullable :: INTEGER, \"strict\" :: INTEGER NOT NULL, \"labels\" :: LIST<STRING NOT NULL> })"
    );
    parse(definition.as_str()).expect("SHOW definition round-trips through parser");
}

#[test]
fn nested_property_nullability_validates_data_writes() {
    let graph = closed_graph(9016);
    let mut session = Session::new(&graph);
    session
        .execute_source(
            "CREATE NODE TYPE :Thing (\
             nullable_xs :: LIST<INT>, \
             strict_xs :: LIST<INT NOT NULL>, \
             payload :: RECORD{\
                 nullable :: INT, \
                 strict :: INT NOT NULL, \
                 labels :: LIST<STRING NOT NULL>})",
            &EmptyProcedureRegistry,
        )
        .expect("nested catalog nullability creates");

    session
        .execute_source(
            "INSERT (:Thing {\
             nullable_xs: [NULL, 1], \
             strict_xs: [1], \
             payload: RECORD{nullable: NULL, strict: 7, labels: ['a']}})",
            &EmptyProcedureRegistry,
        )
        .expect("nullable nested fields and elements accept NULL");

    for source in [
        "INSERT (:Thing {nullable_xs: [1], strict_xs: [NULL], payload: RECORD{nullable: NULL, strict: 7, labels: ['a']}})",
        "INSERT (:Thing {nullable_xs: [1], strict_xs: [1], payload: RECORD{nullable: NULL, strict: NULL, labels: ['a']}})",
        "INSERT (:Thing {nullable_xs: [1], strict_xs: [1], payload: RECORD{nullable: NULL, strict: 7, labels: [NULL]}})",
        "INSERT (:Thing {nullable_xs: [1], strict_xs: [1], payload: RECORD{strict: 7, labels: ['a']}})",
    ] {
        let err = session
            .execute_source(source, &EmptyProcedureRegistry)
            .expect_err("non-conforming nested nullability is rejected");
        assert_eq!(err.gqlstatus(), GqlStatus::GRAPH_TYPE_VIOLATION);
    }
}
