//! Public facade behavior at the consumer boundary.

use std::error::Error as _;

use selene_db::{
    CreatePolicy, Database, DatabaseBuilder, DatabaseConfig, DeclaredType, DropPolicy, ErrorKind,
    ExecutionOutcome, GeneralParameter, GqlStatus, ObjectPath, OpenMode, Request, RequestOutcome,
    RequestParams, SchemaPath, Session, SessionTerminationState, TransactionAccessMode,
    TransactionSlotState, Type, Value, WriteSummary,
};

fn schema(name: &str) -> SchemaPath {
    SchemaPath::regular("selene", name).unwrap()
}

fn graph(schema: &str, name: &str) -> ObjectPath {
    ObjectPath::regular("selene", schema, name).unwrap()
}

fn fixture() -> (Database, ObjectPath) {
    let database = Database::builder().build();
    let catalog = database.catalog();
    let path = graph("memory", "main");
    catalog
        .create_schema(&schema("memory"), CreatePolicy::Strict)
        .unwrap();
    catalog
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    (database, path)
}

fn row_count(outcome: ExecutionOutcome) -> usize {
    outcome
        .row_count()
        .unwrap_or_else(|| panic!("expected row result, got {outcome:?}"))
}

fn single_int(outcome: &ExecutionOutcome) -> i64 {
    let ExecutionOutcome::Rows { result, .. } = outcome else {
        panic!("expected rows, got {outcome:?}");
    };
    let Value::Int(value) = result.rows()[0].values()[0] else {
        panic!("expected integer result");
    };
    value
}

#[test]
fn facade_writes_and_queries_a_selected_catalog_graph() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();

    let write = session
        .execute("INSERT (:Person { name: 'Ada' })")
        .expect("insert succeeds");
    assert_eq!(write.write_summary(), Some(WriteSummary::new(1, None)));
    assert_eq!(
        row_count(
            session
                .execute("MATCH (n:Person) RETURN n")
                .expect("query succeeds")
        ),
        1
    );
}

#[test]
fn facade_summarizes_write_return_outcomes() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();

    let write = session
        .execute("INSERT (n:Person) RETURN n")
        .expect("write with return succeeds");
    assert_eq!(write.write_summary(), Some(WriteSummary::new(1, Some(1))));
    assert_eq!(write.row_count(), Some(1));
}

#[test]
fn facade_preserves_rows_names_and_analyzer_declared_types() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();

    let outcome = session
        .execute("RETURN 7 AS answer, 'Ada' AS name")
        .expect("query succeeds");
    let ExecutionOutcome::Rows {
        result,
        diagnostics,
    } = outcome
    else {
        panic!("expected regular result");
    };
    assert_eq!(
        diagnostics.primary().status(),
        GqlStatus::SUCCESSFUL_COMPLETION
    );
    assert_eq!(result.rows().len(), 1);
    assert_eq!(
        result.rows()[0].values(),
        &[
            Value::Int(7),
            Value::String(selene_core::db_string("Ada").unwrap())
        ]
    );
    assert_eq!(result.descriptor().fields()[0].name(), Some("answer"));
    assert_eq!(result.descriptor().fields()[1].name(), Some("name"));
    assert_eq!(
        result.descriptor().fields()[0].declared_type(),
        &DeclaredType::Resolved(Type::INT64)
    );
    assert_eq!(
        result.descriptor().fields()[1].declared_type(),
        &DeclaredType::Resolved(Type::STRING)
    );

    let procedure = session
        .execute("CALL selene.health() YIELD graph_id, node_count")
        .unwrap();
    let ExecutionOutcome::Rows { result, .. } = procedure else {
        panic!("expected procedure rows");
    };
    assert_eq!(result.descriptor().fields()[0].name(), Some("graph_id"));
    assert_eq!(result.descriptor().fields()[1].name(), Some("node_count"));
    assert_eq!(
        result.descriptor().fields()[1].declared_type(),
        &DeclaredType::Resolved(Type::UINT64)
    );
    assert_eq!(result.rows().len(), 1);

    session.execute("INSERT (:Person)").unwrap();
    let node = session
        .execute("MATCH (n:Person) RETURN n AS node")
        .unwrap();
    let ExecutionOutcome::Rows { result, .. } = node else {
        panic!("expected node-reference row");
    };
    assert_eq!(
        result.rows()[0].values(),
        &[Value::NodeRef(
            session.node_reference(selene_core::NodeId::new(1)).unwrap()
        )]
    );
    assert_eq!(result.descriptor().fields()[0].name(), Some("node"));
    assert_eq!(
        result.descriptor().fields()[0].declared_type(),
        &DeclaredType::Resolved(Type::NODE)
    );
}

#[test]
fn facade_preserves_all_rows_non_scalar_values_and_duplicate_field_diagnostics() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();

    let outcome = session
        .execute("FOR x IN [1, 2, 3] RETURN x AS value, [x] AS items")
        .expect("all rows and list values remain lossless");
    let ExecutionOutcome::Rows { result, .. } = outcome else {
        panic!("expected regular result");
    };

    assert_eq!(result.descriptor().fields()[0].name(), Some("value"));
    assert_eq!(result.descriptor().fields()[1].name(), Some("items"));
    assert_eq!(result.rows().len(), 3);
    for (index, row) in result.rows().iter().enumerate() {
        let value = Value::Int((index + 1) as i64);
        assert_eq!(row.values(), &[value.clone(), Value::List(vec![value])]);
    }

    let duplicate = session.execute_request(Request::new("RETURN 1 AS value, 2 AS value"));
    assert!(matches!(duplicate, RequestOutcome::Failed { .. }));
    assert_eq!(duplicate.diagnostics().primary().status().as_str(), "42N10");
    assert!(
        duplicate
            .diagnostics()
            .primary()
            .message()
            .contains("already declared")
    );
}

#[test]
fn facade_distinguishes_no_data_warning_omitted_and_failure() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();

    let no_data = session.execute("MATCH (n:Missing) RETURN n").unwrap();
    assert_eq!(no_data.row_count(), Some(0));
    assert_eq!(no_data.diagnostics().primary().status(), GqlStatus::NO_DATA);

    let warning = session
        .execute("FOR x IN [1, NULL, 2] RETURN sum(x) AS total")
        .unwrap();
    assert_eq!(
        warning.diagnostics().primary().status(),
        GqlStatus::NULL_VALUE_ELIMINATED_IN_SET_FUNCTION
    );
    assert_eq!(warning.diagnostics().additional().len(), 1);
    assert_eq!(
        warning.diagnostics().additional()[0].status(),
        GqlStatus::SUCCESSFUL_COMPLETION
    );

    let catalog_session = {
        let catalog = database.catalog();
        let schema = SchemaPath::regular("selene", "omitted").unwrap();
        catalog
            .create_schema(&schema, CreatePolicy::Strict)
            .unwrap();
        let graph = ObjectPath::regular("selene", "omitted", "selected").unwrap();
        catalog
            .create_graph(&graph, None, CreatePolicy::Strict)
            .unwrap();
        database.session(&graph).unwrap()
    };
    let omitted = catalog_session.execute("CREATE GRAPH other ANY").unwrap();
    assert!(matches!(omitted, ExecutionOutcome::OmittedResult { .. }));
    assert_eq!(
        omitted.diagnostics().primary().status(),
        GqlStatus::SUCCESSFUL_COMPLETION_OMITTED_RESULT
    );

    let failed = session.execute_request(Request::new("RETURN 1 / 0"));
    assert!(matches!(failed, RequestOutcome::Failed { .. }));
    assert_eq!(failed.diagnostics().primary().status().as_str(), "22012");
    assert!(failed.into_result().is_err());

    let warned_failure = session.execute_request(Request::new(
        "FOR x IN [1, NULL, 2] RETURN sum(x) AS total NEXT RETURN total, 1 / 0",
    ));
    assert_eq!(
        warned_failure.diagnostics().primary().status().as_str(),
        "22012"
    );
    assert_eq!(warned_failure.diagnostics().additional().len(), 1);
    assert_eq!(
        warned_failure.diagnostics().additional()[0].status(),
        GqlStatus::NULL_VALUE_ELIMINATED_IN_SET_FUNCTION
    );
}

#[test]
fn session_owns_database_after_originating_handle_is_dropped() {
    fn assert_send_static<T: Send + 'static>(_: &T) {}

    let session = {
        let (database, path) = fixture();
        let session = database.session(&path).unwrap();
        drop(database);
        session
    };
    assert_send_static(&session);

    assert_eq!(
        row_count(session.execute("RETURN 1").expect("session remains usable")),
        1
    );
}

#[test]
fn selected_sessions_do_not_bleed_state_between_graphs() {
    let database = Database::builder().build();
    let catalog = database.catalog();
    for (schema_name, graph_name) in [("alpha", "one"), ("beta", "two")] {
        catalog
            .create_schema(&schema(schema_name), CreatePolicy::Strict)
            .unwrap();
        catalog
            .create_graph(&graph(schema_name, graph_name), None, CreatePolicy::Strict)
            .unwrap();
    }
    let alpha = database.session(&graph("alpha", "one")).unwrap();
    let beta = database.session(&graph("beta", "two")).unwrap();

    alpha.execute("INSERT (:Alpha)").unwrap();
    beta.execute("INSERT (:Beta)").unwrap();
    assert_eq!(row_count(alpha.execute("MATCH (n) RETURN n").unwrap()), 1);
    assert_eq!(
        row_count(alpha.execute("MATCH (n:Beta) RETURN n").unwrap()),
        0
    );
    assert_eq!(row_count(beta.execute("MATCH (n) RETURN n").unwrap()), 1);
    assert_eq!(
        row_count(beta.execute("MATCH (n:Alpha) RETURN n").unwrap()),
        0
    );
}

#[test]
fn graph_drop_clears_procedure_state_only_after_successful_publication() {
    let (database, path) = fixture();
    let catalog = database.catalog();
    let session = database.session(&path).unwrap();

    session
        .execute("CALL algo.projection_build('facade_projection', NULL, NULL, NULL)")
        .unwrap();
    session.execute("INSERT (:Person)").unwrap();
    assert_eq!(
        catalog
            .drop_graph(&path, DropPolicy::Strict)
            .unwrap_err()
            .kind(),
        ErrorKind::CatalogRestrictViolation
    );
    assert_eq!(
        row_count(
            session
                .execute("CALL algo.projection_list() YIELD name")
                .unwrap()
        ),
        1
    );

    session.execute("MATCH (n) DELETE n").unwrap();
    catalog.drop_graph(&path, DropPolicy::Strict).unwrap();
    assert_eq!(
        session.execute("RETURN 1").unwrap_err().kind(),
        ErrorKind::StaleSessionReference
    );
    catalog
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let replacement = database.session(&path).unwrap();
    assert_eq!(
        row_count(
            replacement
                .execute("CALL algo.projection_list() YIELD name")
                .unwrap()
        ),
        0
    );
}

#[test]
fn transaction_and_session_controls_persist_and_close_terminally() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();

    session.execute("START TRANSACTION").unwrap();
    session.execute("COMMIT").unwrap();
    assert_eq!(
        session
            .execute("COMMIT")
            .unwrap_err()
            .gqlstatus()
            .unwrap()
            .as_str(),
        "2D000"
    );
    session.execute("START TRANSACTION").unwrap();
    session.execute("ROLLBACK").unwrap();

    session.execute("SESSION SET VALUE $answer = 42").unwrap();
    assert_eq!(
        row_count(
            session
                .execute("RETURN $answer")
                .expect("session value persists across requests")
        ),
        1
    );
    session.execute("INSERT (:Person)").unwrap();
    assert_eq!(
        row_count(
            database
                .session(&path)
                .unwrap()
                .execute("MATCH (n:Person) RETURN n")
                .unwrap()
        ),
        1
    );
    session.execute("SESSION CLOSE").unwrap();
    let error = session.execute("RETURN 1").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SessionClosed);
    assert_eq!(error.gqlstatus(), Some(GqlStatus::SESSION_CLOSED));
}

#[test]
fn session_characteristics_are_atomic_persistent_and_request_shadowed() {
    let (database, path) = fixture();
    let session = database.session(&path).unwrap();
    session
        .execute("SESSION SET VALUE $answer INTEGER = 42")
        .unwrap();
    assert_eq!(single_int(&session.execute("RETURN $answer").unwrap()), 42);

    let mut params = RequestParams::new();
    params
        .insert(
            "answer",
            GeneralParameter::new(Type::INT64, Value::Int(7)).unwrap(),
        )
        .unwrap();
    let shadowed = session.execute_request(Request::with_params("RETURN $answer", params));
    assert_eq!(single_int(shadowed.execution().unwrap()), 7);
    assert_eq!(single_int(&session.execute("RETURN $answer").unwrap()), 42);

    session
        .execute("SESSION SET VALUE $answer STRING = 99")
        .unwrap_err();
    assert_eq!(single_int(&session.execute("RETURN $answer").unwrap()), 42);
    session.execute("SESSION SET TIME ZONE '+01:00'").unwrap();
    assert_eq!(session.context().time_zone().seconds(), 3_600);
    session.execute("SESSION RESET PARAMETER $answer").unwrap();
    assert!(session.execute("RETURN $answer").is_err());
    session
        .execute("SESSION RESET ALL CHARACTERISTICS")
        .unwrap();
    assert_eq!(session.context().time_zone().seconds(), 0);
}

#[test]
fn schema_and_graph_controls_switch_then_reset_to_creation_defaults() {
    let database = Database::builder().build();
    let catalog = database.catalog();
    let first = graph("scope_one", "main");
    let second = graph("scope_two", "archive");
    for schema_path in [schema("scope_one"), schema("scope_two")] {
        catalog
            .create_schema(&schema_path, CreatePolicy::Strict)
            .unwrap();
    }
    for graph_path in [&first, &second] {
        catalog
            .create_graph(graph_path, None, CreatePolicy::Strict)
            .unwrap();
    }
    let session = database.session(&first).unwrap();
    session.execute("SESSION SET SCHEMA /scope_two").unwrap();
    session
        .execute("SESSION SET PROPERTY GRAPH archive")
        .unwrap();
    assert_eq!(session.context().current_schema().path, schema("scope_two"));
    assert_eq!(session.context().current_graph().path, second);
    session.execute("INSERT (:Selected)").unwrap();
    assert_eq!(
        database
            .session(&graph("scope_two", "archive"))
            .unwrap()
            .execute("MATCH (n:Selected) RETURN n")
            .unwrap()
            .row_count(),
        Some(1)
    );

    session.execute("SESSION RESET SCHEMA").unwrap();
    session.execute("SESSION RESET GRAPH").unwrap();
    assert_eq!(session.context().current_schema().path, schema("scope_one"));
    assert_eq!(session.context().current_graph().path, first);
}

#[test]
fn stale_graph_selection_can_reset_to_live_creation_defaults() {
    let database = Database::builder().build();
    let catalog = database.catalog();
    let first = graph("recovery_home", "main");
    let alternate = graph("recovery_alternate", "archive");
    for schema_path in [schema("recovery_home"), schema("recovery_alternate")] {
        catalog
            .create_schema(&schema_path, CreatePolicy::Strict)
            .unwrap();
    }
    for graph_path in [&first, &alternate] {
        catalog
            .create_graph(graph_path, None, CreatePolicy::Strict)
            .unwrap();
    }
    let select_alternate = |session: &Session, name: &str| {
        session
            .execute("SESSION SET SCHEMA /recovery_alternate")
            .unwrap();
        session
            .execute(&format!("SESSION SET PROPERTY GRAPH {name}"))
            .unwrap();
    };
    let sequential = database.session(&first).unwrap();
    select_alternate(&sequential, "archive");
    catalog.drop_graph(&alternate, DropPolicy::Strict).unwrap();
    sequential.execute("SESSION RESET SCHEMA").unwrap();
    assert_eq!(
        sequential.context().current_schema().path,
        schema("recovery_home")
    );
    sequential.execute("SESSION RESET GRAPH").unwrap();
    assert_eq!(sequential.context().current_graph().path, first);
    assert_eq!(single_int(&sequential.execute("RETURN 1").unwrap()), 1);
    let reset_all_graph = graph("recovery_alternate", "reset_all");
    catalog
        .create_graph(&reset_all_graph, None, CreatePolicy::Strict)
        .unwrap();
    let reset_all = database.session(&first).unwrap();
    select_alternate(&reset_all, "reset_all");
    reset_all
        .execute("SESSION SET VALUE $answer INTEGER = 42")
        .unwrap();
    reset_all.execute("SESSION SET TIME ZONE '+01:00'").unwrap();
    catalog
        .drop_graph(&reset_all_graph, DropPolicy::Strict)
        .unwrap();
    reset_all
        .execute("SESSION RESET ALL CHARACTERISTICS")
        .unwrap();
    assert_eq!(
        reset_all.context().current_schema().path,
        schema("recovery_home")
    );
    assert_eq!(reset_all.context().current_graph().path, first);
    assert_eq!(reset_all.context().time_zone().seconds(), 0);
    assert!(reset_all.context().parameters().is_empty());
    assert_eq!(single_int(&reset_all.execute("RETURN 2").unwrap()), 2);

    let atomic_graph = graph("recovery_alternate", "atomic");
    catalog
        .create_graph(&atomic_graph, None, CreatePolicy::Strict)
        .unwrap();
    let atomic = database.session(&first).unwrap();
    select_alternate(&atomic, "atomic");
    atomic
        .execute("SESSION SET VALUE $answer INTEGER = 42")
        .unwrap();
    atomic.execute("SESSION SET TIME ZONE '+01:00'").unwrap();
    catalog.drop_graph(&first, DropPolicy::Strict).unwrap();
    let error = atomic
        .execute("SESSION RESET ALL CHARACTERISTICS")
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::StaleSessionReference);
    assert_eq!(
        atomic.context().current_schema().path,
        schema("recovery_alternate")
    );
    assert_eq!(atomic.context().current_graph().path, atomic_graph);
    assert_eq!(atomic.context().time_zone().seconds(), 3_600);
    assert_eq!(single_int(&atomic.execute("RETURN $answer").unwrap()), 42);
}

#[test]
fn selection_changes_reject_active_transactions_and_close_releases_all_state() {
    let (database, path) = fixture();
    let catalog = database.catalog();
    let alternate = graph("memory", "alternate");
    catalog
        .create_graph(&alternate, None, CreatePolicy::Strict)
        .unwrap();
    let session = database.session(&path).unwrap();
    session
        .start_transaction(TransactionAccessMode::ReadWrite)
        .unwrap();
    let error = session.execute("SESSION SET GRAPH alternate").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ActiveTransaction);
    assert_eq!(session.context().current_graph().path, path);
    session.execute("INSERT (:Discarded)").unwrap();
    session.execute("SESSION CLOSE").unwrap();
    assert_eq!(
        session.context().termination(),
        SessionTerminationState::Closed
    );
    assert_eq!(
        session.context().transaction_slot(),
        TransactionSlotState::Vacant
    );
    assert_eq!(
        database
            .session(&graph("memory", "main"))
            .unwrap()
            .execute("MATCH (n:Discarded) RETURN n")
            .unwrap()
            .row_count(),
        Some(0)
    );
    for error in [
        session.execute("SESSION CLOSE").unwrap_err(),
        session.execute("RETURN 1").unwrap_err(),
    ] {
        assert_eq!(error.gqlstatus(), Some(GqlStatus::SESSION_CLOSED));
    }
}

#[test]
fn close_works_for_failed_transactions_and_stale_graph_selections() {
    let (database, path) = fixture();
    let failed = database.session(&path).unwrap();
    failed
        .start_transaction(TransactionAccessMode::ReadWrite)
        .unwrap();
    failed.execute("INSERT (:Discarded)").unwrap();
    failed.execute("RETURN 1 / 0").unwrap_err();
    failed.execute("SESSION CLOSE").unwrap();
    assert_eq!(
        failed.context().transaction_slot(),
        TransactionSlotState::Vacant
    );

    let stale_path = graph("memory", "stale");
    database
        .catalog()
        .create_graph(&stale_path, None, CreatePolicy::Strict)
        .unwrap();
    let stale = database.session(&stale_path).unwrap();
    database
        .catalog()
        .drop_graph(&stale_path, DropPolicy::Strict)
        .unwrap();
    stale.execute("SESSION CLOSE").unwrap();
    assert_eq!(
        stale.context().termination(),
        SessionTerminationState::Closed
    );
}

#[test]
fn session_selection_reports_missing_and_wrong_kind_paths() {
    let (database, path) = fixture();
    let catalog = database.catalog();
    let graph_type = graph("memory", "shape");
    let name = selene_db::PathSegment::regular("Person").unwrap();
    let definition = selene_db::GraphTypeDefinition::builder()
        .with_node_type(selene_db::NodeTypeDefinition::new(name.clone(), vec![name]).unwrap())
        .build()
        .unwrap();
    catalog
        .create_graph_type(&graph_type, definition, CreatePolicy::Strict)
        .unwrap();

    assert_eq!(
        database
            .session(&graph("memory", "missing"))
            .err()
            .unwrap()
            .kind(),
        ErrorKind::CatalogObjectNotFound
    );
    assert_eq!(
        database.session(&graph_type).err().unwrap().kind(),
        ErrorKind::CatalogObjectWrongKind
    );
    database.session(&path).unwrap();
}

#[test]
fn invalid_gql_maps_to_owned_facade_error() {
    let (database, path) = fixture();
    let error = database
        .session(&path)
        .unwrap()
        .execute("NOT A GQL STATEMENT")
        .expect_err("invalid source fails");

    assert_eq!(error.kind(), ErrorKind::InvalidGql);
    assert_eq!(
        error.gqlstatus().map(|status| status.as_str().to_owned()),
        Some("42001".to_owned())
    );
    assert!(!error.message().is_empty());
    assert!(error.source().is_some());
}

#[test]
fn builder_exposes_only_consumed_in_memory_configuration() {
    let config = DatabaseConfig::default();
    let builder = DatabaseBuilder::from_config(config.clone());

    assert_eq!(config.open_mode(), OpenMode::InMemory);
    assert_eq!(builder.config(), &config);
    assert_eq!(builder.build().config(), &config);
}

#[test]
fn facade_session_type_has_no_lifetime_parameter() {
    fn move_session(session: Session) -> Session {
        session
    }

    let (database, path) = fixture();
    let session = move_session(database.session(&path).unwrap());
    assert_eq!(
        row_count(session.execute("RETURN 1").expect("moved session works")),
        1
    );
}
