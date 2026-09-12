//! BRIEF-38 statement-level executor tests.

use std::sync::{Arc, Mutex};

use selene_core::{GraphId, LabelSet, Value};
use selene_gql::{
    EmptyProcedureRegistry, ExecutionPlan, ExecutorError, ExecutorWarning, GqlStatus, Session,
    StatementOutput, WarningSink, WriteOutcome, analyze, execute_statement, parse, plan,
};
use selene_graph::{GraphTypeDef, NodeTypeDef, SharedGraph, ValidationMode};

fn db_string(value: &str) -> selene_core::DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn planned(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

fn execute(source: &str, session: &mut Session<'_>) -> Result<StatementOutput, ExecutorError> {
    let plan = planned(source);
    execute_statement(&plan, session, &EmptyProcedureRegistry)
}

fn rows(output: StatementOutput) -> selene_gql::BindingTable {
    match output {
        StatementOutput::Rows(table) => table,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn written(output: StatementOutput) -> WriteOutcome {
    match output {
        StatementOutput::Written(outcome) => outcome,
        other => panic!("expected written output, got {other:?}"),
    }
}

fn empty_closed_graph(id: u64) -> SharedGraph {
    SharedGraph::builder(GraphId::new(id))
        .bound_to(GraphTypeDef {
            name: db_string("statement.test.graph"),
            node_types: Vec::new(),
            edge_types: Vec::new(),
        })
        .unwrap()
        .build()
        .unwrap()
}

fn closed_person_graph(id: u64) -> SharedGraph {
    let person = db_string("Person");
    SharedGraph::builder(GraphId::new(id))
        .bound_to(GraphTypeDef {
            name: db_string("statement.person.graph"),
            node_types: vec![NodeTypeDef {
                name: person.clone(),
                key_labels: LabelSet::single(person),
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: Vec::new(),
        })
        .unwrap()
        .build()
        .unwrap()
}

#[derive(Clone)]
struct RecordingSink {
    warnings: Arc<Mutex<Vec<ExecutorWarning>>>,
}

impl WarningSink for RecordingSink {
    fn emit(&mut self, warning: ExecutorWarning) {
        self.warnings.lock().expect("warning mutex").push(warning);
    }
}

#[test]
fn read_only_returns_rows_from_pattern_less_plan() {
    let graph = SharedGraph::new(GraphId::new(3800));
    let mut session = Session::new(&graph);

    let table = rows(execute("RETURN 1 AS n", &mut session).expect("statement executes"));

    assert_eq!(table.row_count(), 1);
    assert_eq!(table.rows()[0].values(), &[Value::Int(1)]);
}

#[test]
fn read_only_returns_rows_from_pattern_plan() {
    let graph = SharedGraph::new(GraphId::new(3801));
    {
        let mut txn = graph.begin_write();
        txn.mutator()
            .create_node(LabelSet::single(db_string("Person")), Default::default())
            .expect("fixture node inserts");
        txn.commit().expect("fixture commits");
    }
    let mut session = Session::new(&graph);

    let table =
        rows(execute("MATCH (n:Person) RETURN n", &mut session).expect("statement executes"));

    assert_eq!(table.row_count(), 1);
    assert!(matches!(table.rows()[0].values(), [Value::NodeRef(_)]));
}

#[test]
fn write_statement_produces_written() {
    let graph = SharedGraph::new(GraphId::new(3802));
    let mut session = Session::new(&graph);

    let output = execute("INSERT (n:Person) FINISH", &mut session).expect("insert executes");
    let outcome = written(output);

    assert!(outcome.rows.is_none());
    assert_eq!(outcome.changes.len(), 1);
    assert_eq!(outcome.durable_at, None);
    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn mutation_with_return_carries_rows() {
    let graph = SharedGraph::new(GraphId::new(3803));
    let mut session = Session::new(&graph);

    let outcome =
        written(execute("INSERT (n:Person) RETURN n", &mut session).expect("insert executes"));
    let table = outcome.rows.expect("write with RETURN carries rows");

    assert_eq!(outcome.changes.len(), 1);
    assert_eq!(table.row_count(), 1);
    assert!(matches!(table.rows()[0].values(), [Value::NodeRef(_)]));
    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn data_modifying_auto_rolls_back_on_error() {
    let graph = SharedGraph::new(GraphId::new(3804));
    let mut session = Session::new(&graph);

    let err = execute("INSERT (n:Person) SET n.age = 1 / 0 FINISH", &mut session)
        .expect_err("statement errors");

    assert!(matches!(err, ExecutorError::DataException { .. }));
    assert_eq!(graph.read().node_count(), 0);
}

#[test]
fn catalog_modifying_auto_commits_create_type() {
    let graph = empty_closed_graph(3805);
    let mut session = Session::new(&graph);

    let output = execute("CREATE NODE TYPE :Foo ()", &mut session).expect("catalog executes");
    let outcome = written(output);

    assert!(outcome.rows.is_none());
    assert_eq!(outcome.changes.len(), 1);
    let graph_type = graph.graph_type().expect("closed graph type");
    assert_eq!(graph_type.node_types[0].name.as_str(), "Foo");
}

#[test]
fn catalog_default_property_materializes_on_insert() {
    let graph = empty_closed_graph(3814);
    let mut session = Session::new(&graph);

    execute(
        "CREATE NODE TYPE :Person (active :: BOOLEAN DEFAULT true)",
        &mut session,
    )
    .expect("catalog succeeds");
    execute("INSERT (n:Person) FINISH", &mut session).expect("insert succeeds");
    let table = rows(
        execute("MATCH (n:Person) RETURN n.active AS active", &mut session)
            .expect("match succeeds"),
    );

    assert_eq!(table.rows()[0].values(), &[Value::Bool(true)]);
}

#[test]
fn catalog_bytes_default_property_materializes_on_insert() {
    let graph = empty_closed_graph(3817);
    let mut session = Session::new(&graph);

    execute(
        "CREATE NODE TYPE :Blob (payload :: BYTES DEFAULT X'CAFE')",
        &mut session,
    )
    .expect("catalog succeeds");
    execute("INSERT (n:Blob) FINISH", &mut session).expect("insert succeeds");
    let table = rows(
        execute("MATCH (n:Blob) RETURN n.payload AS payload", &mut session)
            .expect("match succeeds"),
    );

    assert_eq!(
        table.rows()[0].values(),
        &[Value::Bytes(Arc::<[u8]>::from([0xCA, 0xFE]))]
    );
}

#[test]
fn catalog_float_default_properties_materialize_and_round_trip() {
    let graph = empty_closed_graph(3825);
    let mut session = Session::new(&graph);

    execute(
        "CREATE NODE TYPE :Metric (score :: FLOAT DEFAULT 1.5D, \"small\" :: FLOAT32 DEFAULT 2.25D)",
        &mut session,
    )
    .expect("catalog succeeds");
    execute("INSERT (n:Metric) FINISH", &mut session).expect("insert succeeds");
    let table = rows(
        execute(
            "MATCH (n:Metric) RETURN n.score AS score, n.small AS \"small\"",
            &mut session,
        )
        .expect("match succeeds"),
    );

    assert_eq!(
        table.rows()[0].values(),
        &[Value::Float(1.5), Value::Float32(2.25_f32)]
    );

    let table = rows(execute("SHOW NODE TYPES", &mut session).expect("show succeeds"));
    assert_eq!(
        table.rows()[0].values()[1],
        Value::String(db_string(
            "CREATE NODE TYPE :Metric (score :: FLOAT DEFAULT 1.5D, \"small\" :: FLOAT32 DEFAULT 2.25D)"
        ))
    );
}

#[test]
fn catalog_exact_numeric_default_properties_materialize_and_round_trip() {
    let graph = empty_closed_graph(3826);
    let mut session = Session::new(&graph);

    execute(
        "CREATE NODE TYPE :Metric (u :: UINT64 DEFAULT 42, \
         i128 :: INT128 DEFAULT '-170141183460469231731687303715884105728', \
         u128 :: UINT128 DEFAULT '340282366920938463463374607431768211455', \
         dec_i :: DECIMAL DEFAULT 7, dec_s :: DECIMAL DEFAULT '123.450', \
         dec_f :: DECIMAL DEFAULT 1.25)",
        &mut session,
    )
    .expect("catalog succeeds");
    execute("INSERT (n:Metric) FINISH", &mut session).expect("insert succeeds");
    let table = rows(
        execute(
            "MATCH (n:Metric) RETURN n.u AS u, n.i128 AS i128, n.u128 AS u128, \
             n.dec_i AS dec_i, n.dec_s AS dec_s, n.dec_f AS dec_f",
            &mut session,
        )
        .expect("match succeeds"),
    );

    assert_eq!(
        table.rows()[0].values(),
        &[
            Value::Uint(42),
            Value::Int128(i128::MIN),
            Value::Uint128(u128::MAX),
            Value::Decimal("7".parse().unwrap()),
            Value::Decimal("123.450".parse().unwrap()),
            Value::Decimal("1.25".parse().unwrap()),
        ]
    );

    let table = rows(execute("SHOW NODE TYPES", &mut session).expect("show succeeds"));
    assert_eq!(
        table.rows()[0].values()[1],
        Value::String(db_string(
            "CREATE NODE TYPE :Metric (u :: UINT64 DEFAULT '42', \
         i128 :: INT128 DEFAULT '-170141183460469231731687303715884105728', \
         u128 :: UINT128 DEFAULT '340282366920938463463374607431768211455', \
         dec_i :: DECIMAL DEFAULT '7', dec_s :: DECIMAL DEFAULT '123.450', \
         dec_f :: DECIMAL DEFAULT '1.25')"
        ))
    );
}

#[test]
fn catalog_immutable_property_rejects_gql_set() {
    let graph = empty_closed_graph(3815);
    let mut session = Session::new(&graph);

    execute(
        "CREATE NODE TYPE :Person (serial :: STRING NOT NULL IMMUTABLE)",
        &mut session,
    )
    .expect("catalog succeeds");
    execute("INSERT (n:Person {serial: 'A'}) FINISH", &mut session).expect("insert succeeds");

    let err = execute("MATCH (n:Person) SET n.serial = 'B' FINISH", &mut session)
        .expect_err("immutable property rejects update");

    assert_eq!(err.gqlstatus(), GqlStatus::GRAPH_TYPE_VIOLATION);
}

#[test]
fn catalog_warn_mode_emits_relaxed_write_warning_after_commit() {
    let graph = empty_closed_graph(3816);
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let sink = RecordingSink {
        warnings: Arc::clone(&warnings),
    };
    let mut session = Session::new(&graph).with_warning_sink(sink);

    execute("CREATE NODE TYPE :Person () WARN", &mut session).expect("catalog succeeds");
    execute("INSERT (n:Person {extra: 1}) FINISH", &mut session)
        .expect("warn-mode insert succeeds");
    let observed = warnings.lock().expect("warning mutex").clone();

    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].code, GqlStatus::VALIDATION_MODE_RELAXED_WRITE);
}

#[test]
fn explicit_commit_emits_relaxed_write_warning_after_commit() {
    let graph = empty_closed_graph(3817);
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let sink = RecordingSink {
        warnings: Arc::clone(&warnings),
    };
    let mut session = Session::new(&graph).with_warning_sink(sink);

    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("CREATE NODE TYPE :Person () WARN", &mut session).expect("catalog succeeds");
    execute("INSERT (n:Person {extra: 1}) FINISH", &mut session).expect("insert succeeds");
    execute("COMMIT", &mut session).expect("commit succeeds");
    let observed = warnings.lock().expect("warning mutex").clone();

    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].code, GqlStatus::VALIDATION_MODE_RELAXED_WRITE);
}

#[test]
fn catalog_show_yields_rows() {
    let graph = SharedGraph::builder(GraphId::new(3806))
        .bound_to(GraphTypeDef {
            name: db_string("statement.show.graph"),
            node_types: vec![NodeTypeDef {
                name: db_string("types.person"),
                key_labels: LabelSet::single(db_string("Person")),
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: Vec::new(),
        })
        .unwrap()
        .build()
        .unwrap();
    let mut session = Session::new(&graph);

    let table = rows(execute("SHOW NODE TYPES", &mut session).expect("show executes"));

    assert_eq!(table.row_count(), 1);
    assert_eq!(
        table.rows()[0].values()[0],
        Value::String(db_string("Person"))
    );
}

#[test]
fn explicit_tx_commit_persists_changes() {
    let graph = SharedGraph::new(GraphId::new(3807));
    let mut session = Session::new(&graph);

    assert!(matches!(
        execute("START TRANSACTION", &mut session).expect("start succeeds"),
        StatementOutput::Empty
    ));
    assert!(session.has_active_txn());
    execute("INSERT (n:Person) FINISH", &mut session).expect("insert succeeds");
    assert_eq!(graph.read().node_count(), 0);
    let outcome = written(execute("COMMIT", &mut session).expect("commit succeeds"));

    assert_eq!(outcome.changes.len(), 1);
    assert!(!session.has_active_txn());
    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn commit_with_zero_changes_returns_written() {
    let graph = SharedGraph::new(GraphId::new(3819));
    let mut session = Session::new(&graph);

    execute("START TRANSACTION", &mut session).expect("start succeeds");
    let outcome = written(execute("COMMIT", &mut session).expect("commit succeeds"));

    assert!(outcome.rows.is_none());
    assert!(outcome.changes.is_empty());
    assert_eq!(outcome.generation, 1);
    assert!(!session.has_active_txn());
}

#[test]
fn explicit_tx_rollback_discards_changes() {
    let graph = SharedGraph::new(GraphId::new(3808));
    let mut session = Session::new(&graph);

    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("INSERT (n:Person) FINISH", &mut session).expect("insert succeeds");
    execute("ROLLBACK", &mut session).expect("rollback succeeds");

    assert!(!session.has_active_txn());
    assert_eq!(graph.read().node_count(), 0);
}

#[test]
fn explicit_tx_statement_error_aborts_session() {
    let graph = SharedGraph::new(GraphId::new(3809));
    let mut session = Session::new(&graph);

    execute("START TRANSACTION", &mut session).expect("start succeeds");
    let err = execute("INSERT (n:Person) SET n.age = 1 / 0 FINISH", &mut session)
        .expect_err("insert errors");

    assert!(matches!(err, ExecutorError::DataException { .. }));
    assert!(session.has_active_txn());
    assert!(session.is_aborted());
    assert_eq!(graph.read().node_count(), 0);
    session.abort();
    assert!(!session.has_active_txn());
    assert!(!session.is_aborted());
    assert_eq!(graph.read().node_count(), 0);
}

#[test]
fn read_only_during_active_tx_sees_uncommitted_writes() {
    let graph = SharedGraph::new(GraphId::new(3810));
    let mut session = Session::new(&graph);

    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("INSERT (n:Person) FINISH", &mut session).expect("insert succeeds");
    let table = rows(execute("MATCH (n:Person) RETURN n", &mut session).expect("read executes"));

    assert_eq!(table.row_count(), 1);
    assert!(session.has_active_txn());
    session.abort();
}

#[test]
fn start_transaction_with_active_txn_returns_already_active() {
    let graph = SharedGraph::new(GraphId::new(3811));
    let mut session = Session::new(&graph);
    execute("START TRANSACTION", &mut session).expect("start succeeds");

    let err = execute("START TRANSACTION", &mut session).expect_err("second start errors");

    assert!(matches!(
        err,
        ExecutorError::TransactionAlreadyActive { .. }
    ));
    assert_eq!(err.gqlstatus(), GqlStatus::ACTIVE_TRANSACTION);
    session.abort();
}

#[test]
fn commit_without_active_txn_returns_no_active_transaction() {
    let graph = SharedGraph::new(GraphId::new(3812));
    let mut session = Session::new(&graph);

    let err = execute("COMMIT", &mut session).expect_err("commit errors");

    assert!(matches!(err, ExecutorError::NoActiveTransaction { .. }));
    assert_eq!(err.gqlstatus(), GqlStatus::INVALID_TRANSACTION_TERMINATION);
}

#[test]
fn rollback_without_active_txn_returns_no_active_transaction() {
    let graph = SharedGraph::new(GraphId::new(3813));
    let mut session = Session::new(&graph);

    let err = execute("ROLLBACK", &mut session).expect_err("rollback errors");

    assert!(matches!(err, ExecutorError::NoActiveTransaction { .. }));
    assert_eq!(err.gqlstatus(), GqlStatus::INVALID_TRANSACTION_TERMINATION);
}

#[test]
fn aborted_session_rejects_data_modifying_with_in_failed_transaction() {
    let graph = SharedGraph::new(GraphId::new(3814));
    let mut session = Session::new(&graph);
    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("INSERT (n:Person) SET n.age = 1 / 0 FINISH", &mut session)
        .expect_err("statement aborts transaction");

    let err = execute("INSERT (n:Other) FINISH", &mut session).expect_err("aborted tx rejects");

    assert!(matches!(err, ExecutorError::InFailedTransaction { .. }));
    assert_eq!(err.gqlstatus(), GqlStatus::IN_FAILED_TRANSACTION);
    assert!(session.is_aborted());
    session.abort();
}

#[test]
fn aborted_session_rejects_read_only_with_in_failed_transaction() {
    let graph = SharedGraph::new(GraphId::new(3815));
    let mut session = Session::new(&graph);
    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("INSERT (n:Person) SET n.age = 1 / 0 FINISH", &mut session)
        .expect_err("statement aborts transaction");

    let err = execute("RETURN 1 AS n", &mut session).expect_err("aborted tx rejects");

    assert!(matches!(err, ExecutorError::InFailedTransaction { .. }));
    assert!(session.is_aborted());
    session.abort();
}

#[test]
fn aborted_session_commit_rolls_back_and_returns_in_failed_transaction() {
    let graph = SharedGraph::new(GraphId::new(3816));
    let mut session = Session::new(&graph);
    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("INSERT (n:Person) SET n.age = 1 / 0 FINISH", &mut session)
        .expect_err("statement aborts transaction");

    let err = execute("COMMIT", &mut session).expect_err("commit refuses aborted tx");

    assert!(matches!(err, ExecutorError::InFailedTransaction { .. }));
    assert!(!session.has_active_txn());
    assert!(!session.is_aborted());
    assert_eq!(graph.read().node_count(), 0);
}

#[test]
fn aborted_session_rollback_clears_aborted_state() {
    let graph = SharedGraph::new(GraphId::new(3817));
    let mut session = Session::new(&graph);
    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("INSERT (n:Person) SET n.age = 1 / 0 FINISH", &mut session)
        .expect_err("statement aborts transaction");

    execute("ROLLBACK", &mut session).expect("rollback succeeds");

    assert!(!session.has_active_txn());
    assert!(!session.is_aborted());
    assert_eq!(graph.read().node_count(), 0);
}

#[test]
fn aborted_session_abort_helper_clears_aborted_state() {
    let graph = SharedGraph::new(GraphId::new(3818));
    let mut session = Session::new(&graph);
    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("INSERT (n:Person) SET n.age = 1 / 0 FINISH", &mut session)
        .expect_err("statement aborts transaction");

    session.abort();

    assert!(!session.has_active_txn());
    assert!(!session.is_aborted());
    assert_eq!(graph.read().node_count(), 0);
}

#[test]
fn partial_writes_from_failed_statement_do_not_persist_after_rollback() {
    let graph = SharedGraph::new(GraphId::new(3819));
    let mut session = Session::new(&graph);
    execute("START TRANSACTION", &mut session).expect("start succeeds");

    execute("INSERT (n:Person) SET n.age = 1 / 0 FINISH", &mut session)
        .expect_err("statement aborts transaction");
    execute("ROLLBACK", &mut session).expect("rollback succeeds");

    assert_eq!(graph.read().node_count(), 0);
}

#[test]
fn multi_statement_explicit_tx_commits_all_changes() {
    let graph = SharedGraph::new(GraphId::new(3820));
    let mut session = Session::new(&graph);

    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("INSERT (n:Person) FINISH", &mut session).expect("first insert succeeds");
    execute("INSERT (n:Company) FINISH", &mut session).expect("second insert succeeds");
    execute("COMMIT", &mut session).expect("commit succeeds");

    assert_eq!(graph.read().node_count(), 2);
}

#[test]
fn session_abort_clears_active_txn() {
    let graph = SharedGraph::new(GraphId::new(3821));
    let mut session = Session::new(&graph);

    execute("START TRANSACTION", &mut session).expect("start succeeds");
    session.abort();

    assert!(!session.has_active_txn());
    assert!(!session.is_aborted());
}

#[test]
fn session_with_principal_commits_successfully() {
    let graph = SharedGraph::new(GraphId::new(3822));
    let principal = Arc::from([1_u8, 2, 3]);
    let mut session = Session::with_principal(&graph, principal);

    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("INSERT (n:Person) FINISH", &mut session).expect("insert succeeds");
    execute("COMMIT", &mut session).expect("commit succeeds");

    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn catalog_modifying_rejects_drop_with_surviving_instances_no_partial_state() {
    // Seam-B fix (audit Item 3): under RESTRICT (the default) the rejection is
    // now early and class GraphTypeViolation (G2000), not the old late
    // commit-time GraphMutation. The "no partial state" invariant is preserved:
    // the type survives AND the instances survive.
    let graph = closed_person_graph(3823);
    {
        let mut txn = graph.begin_write();
        txn.mutator()
            .create_node(LabelSet::single(db_string("Person")), Default::default())
            .expect("fixture node inserts");
        txn.commit().expect("fixture commits");
    }
    let mut session = Session::new(&graph);

    let err = execute("DROP NODE TYPE :Person", &mut session).expect_err("RESTRICT rejects drop");

    assert!(matches!(err, ExecutorError::GraphTypeViolation { .. }));
    assert_eq!(graph.graph_type().unwrap().node_types.len(), 1);
    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn explicit_tx_allows_data_then_catalog_mix_in_phase_a() {
    let graph = empty_closed_graph(3824);
    let mut session = Session::new(&graph);

    execute("START TRANSACTION", &mut session).expect("start succeeds");
    execute("INSERT (n:Person) FINISH", &mut session).expect("insert succeeds");
    execute("CREATE NODE TYPE :Person ()", &mut session).expect("catalog succeeds");
    execute("COMMIT", &mut session).expect("commit succeeds");

    assert_eq!(graph.read().node_count(), 1);
    assert_eq!(
        graph.graph_type().unwrap().node_types[0].name.as_str(),
        "Person"
    );
}
