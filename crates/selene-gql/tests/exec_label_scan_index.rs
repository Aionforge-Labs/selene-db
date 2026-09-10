//! OPT-1/OPT-2/OPT-3 end-to-end execution tests through `Session::execute_source`.
//!
//! These prove the optimizer is wired into the production path with a
//! snapshot-pinned `LiveIndexCatalog` (default-on), that index acceleration is
//! semantically transparent (byte-identical rows vs the `without_index_selection`
//! Linear path), and that EXPLAIN renders the optimized access path. This is the
//! "would it catch the DbString admission race" bar: a divergence between the Linear
//! and indexed paths is the failure mode under test.

use std::sync::Arc;

use selene_core::{DbString, GraphId, LabelSet, NodeId, PropertyMap, Value};
use selene_gql::{BindingTable, EmptyProcedureRegistry, Session, StatementOutput};
use selene_graph::{SharedGraph, TypedIndexKind};

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn props<const N: usize>(pairs: [(DbString, Value); N]) -> PropertyMap {
    PropertyMap::from_pairs(pairs).expect("test properties fit caps")
}

fn decimal(value: &str) -> rust_decimal::Decimal {
    value.parse().expect("test decimal parses")
}

fn rows(output: StatementOutput) -> BindingTable {
    match output {
        StatementOutput::Rows(table) => table,
        StatementOutput::Written(outcome) => outcome.rows.expect("write returned rows"),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn node_ids(table: &BindingTable) -> Vec<u64> {
    table
        .rows()
        .iter()
        .filter_map(|row| match row.values().first() {
            Some(Value::NodeRef(id)) => Some(id.get()),
            _ => None,
        })
        .collect()
}

fn explain_dump(session: &mut Session<'_>, source: &str) -> String {
    let table = rows(
        session
            .execute_source(&format!("EXPLAIN {source}"), &EmptyProcedureRegistry)
            .expect("EXPLAIN executes"),
    );
    match table.rows().first().and_then(|r| r.values().first()) {
        Some(Value::String(dump)) => dump.as_str().to_owned(),
        other => panic!("expected EXPLAIN dump string, got {other:?}"),
    }
}

/// Build a graph with mixed labels: 3 `Person` (one also `Employee`), 2
/// `Robot`, indexed `Person.age` (I64). Returns the graph and the alive
/// Person NodeIds.
fn build_graph() -> (SharedGraph, Vec<NodeId>) {
    let graph = SharedGraph::new(GraphId::new(901));
    let person = db_string("Person");
    let employee = db_string("Employee");
    let robot = db_string("Robot");
    let age = db_string("age");
    let mut persons = Vec::new();
    {
        let mut txn = graph.begin_write();
        {
            let mut m = txn.mutator();
            persons.push(
                m.create_node(
                    LabelSet::single(person.clone()),
                    props([(age.clone(), Value::Int(30))]),
                )
                .unwrap(),
            );
            // A Person that also carries Employee — proves the post-filter +
            // bitmap agree for multi-label nodes.
            let mut both = LabelSet::single(person.clone());
            both.insert(employee);
            persons.push(
                m.create_node(both, props([(age.clone(), Value::Int(40))]))
                    .unwrap(),
            );
            persons.push(
                m.create_node(
                    LabelSet::single(person.clone()),
                    props([(age.clone(), Value::Int(50))]),
                )
                .unwrap(),
            );
            m.create_node(LabelSet::single(robot.clone()), PropertyMap::new())
                .unwrap();
            m.create_node(LabelSet::single(robot), PropertyMap::new())
                .unwrap();
        }
        txn.commit().unwrap();
    }
    graph
        .create_property_index(person, age, TypedIndexKind::I64)
        .unwrap();
    (graph, persons)
}

#[test]
fn label_scan_returns_same_rows_as_linear() {
    let (graph, _persons) = build_graph();
    let source = "MATCH (n:Person) RETURN n";

    let mut indexed = Session::new(&graph);
    let indexed_rows = node_ids(&rows(
        indexed
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    ));

    let mut linear = Session::new(&graph).without_index_selection();
    let linear_rows = node_ids(&rows(
        linear
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    ));

    assert_eq!(
        indexed_rows, linear_rows,
        "indexed vs linear row sets diverge"
    );
    assert_eq!(indexed_rows.len(), 3, "exactly the 3 Person nodes");
}

#[test]
fn typed_index_returns_same_rows_as_linear() {
    let (graph, _persons) = build_graph();
    let source = "MATCH (n:Person) WHERE n.age = 40 RETURN n";

    let mut indexed = Session::new(&graph);
    let indexed_rows = node_ids(&rows(
        indexed
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    ));

    let mut linear = Session::new(&graph).without_index_selection();
    let linear_rows = node_ids(&rows(
        linear
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    ));

    assert_eq!(indexed_rows, linear_rows);
    assert_eq!(indexed_rows.len(), 1, "one Person with age=40");
}

#[test]
fn bool_typed_index_returns_same_rows_as_linear() {
    let graph = SharedGraph::new(GraphId::new(904));
    let sensor = db_string("Sensor");
    let active = db_string("active");
    {
        let mut txn = graph.begin_write();
        {
            let mut m = txn.mutator();
            for value in [true, false, true] {
                m.create_node(
                    LabelSet::single(sensor.clone()),
                    props([(active.clone(), Value::Bool(value))]),
                )
                .unwrap();
            }
        }
        txn.commit().unwrap();
    }
    graph
        .create_property_index(sensor, active, TypedIndexKind::Bool)
        .unwrap();
    let source = "MATCH (n:Sensor) WHERE n.active = true RETURN n";

    let mut indexed = Session::new(&graph);
    let indexed_rows = node_ids(&rows(
        indexed
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    ));

    let mut linear = Session::new(&graph).without_index_selection();
    let linear_rows = node_ids(&rows(
        linear
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    ));

    assert_eq!(indexed_rows, linear_rows);
    assert_eq!(indexed_rows.len(), 2, "two active sensors");

    let dump = explain_dump(&mut indexed, source);
    assert!(
        dump.contains("TypedIndexRange"),
        "boolean equality should render TypedIndexRange; got:\n{dump}"
    );
}

#[test]
fn u64_typed_index_returns_same_rows_as_linear() {
    let graph = SharedGraph::new(GraphId::new(905));
    let sensor = db_string("Sensor");
    let reading_count = db_string("reading_count");
    {
        let mut txn = graph.begin_write();
        {
            let mut m = txn.mutator();
            for value in [7_u64, 8, u64::MAX, 8] {
                m.create_node(
                    LabelSet::single(sensor.clone()),
                    props([(reading_count.clone(), Value::Uint(value))]),
                )
                .unwrap();
            }
        }
        txn.commit().unwrap();
    }
    graph
        .create_property_index(sensor, reading_count, TypedIndexKind::U64)
        .unwrap();
    let source = "MATCH (n:Sensor) WHERE n.reading_count = $needle :: UINT64 RETURN n";

    let mut indexed = Session::new(&graph);
    indexed.bind_parameter(db_string("needle"), Value::Uint(8));
    let indexed_rows = node_ids(&rows(
        indexed
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    ));

    let mut linear = Session::new(&graph).without_index_selection();
    linear.bind_parameter(db_string("needle"), Value::Uint(8));
    let linear_rows = node_ids(&rows(
        linear
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    ));

    assert_eq!(indexed_rows, linear_rows);
    assert_eq!(indexed_rows.len(), 2, "two sensors with count=8");

    let dump = explain_dump(&mut indexed, source);
    assert!(
        dump.contains("TypedIndexRange"),
        "u64 equality should render TypedIndexRange; got:\n{dump}"
    );
}

#[test]
fn exact_numeric_typed_indexes_return_same_rows_as_linear() {
    let graph = SharedGraph::new(GraphId::new(906));
    let metric = db_string("Metric");
    let signed = db_string("signed");
    let unsigned = db_string("unsigned");
    let amount = db_string("amount");
    {
        let mut txn = graph.begin_write();
        {
            let mut m = txn.mutator();
            for (signed_value, unsigned_value, amount_value) in [
                (i128::MIN + 8, u64::MAX as u128 + 8, decimal("1.25")),
                (i128::MAX - 8, u128::MAX - 8, decimal("2.50")),
                (i128::MAX - 8, u128::MAX - 8, decimal("2.50")),
            ] {
                m.create_node(
                    LabelSet::single(metric.clone()),
                    props([
                        (signed.clone(), Value::Int128(signed_value)),
                        (unsigned.clone(), Value::Uint128(unsigned_value)),
                        (amount.clone(), Value::Decimal(amount_value)),
                    ]),
                )
                .unwrap();
            }
        }
        txn.commit().unwrap();
    }
    graph
        .create_property_index(metric.clone(), signed, TypedIndexKind::I128)
        .unwrap();
    graph
        .create_property_index(metric.clone(), unsigned, TypedIndexKind::U128)
        .unwrap();
    graph
        .create_property_index(metric, amount, TypedIndexKind::Decimal)
        .unwrap();

    for (source, param, value) in [
        (
            "MATCH (n:Metric) WHERE n.signed = $signed :: INT128 RETURN n",
            "signed",
            Value::Int128(i128::MAX - 8),
        ),
        (
            "MATCH (n:Metric) WHERE n.unsigned = $unsigned :: UINT128 RETURN n",
            "unsigned",
            Value::Uint128(u128::MAX - 8),
        ),
        (
            "MATCH (n:Metric) WHERE n.amount = $amount :: DECIMAL RETURN n",
            "amount",
            Value::Decimal(decimal("2.50")),
        ),
    ] {
        let mut indexed = Session::new(&graph);
        indexed.bind_parameter(db_string(param), value.clone());
        let indexed_rows = node_ids(&rows(
            indexed
                .execute_source(source, &EmptyProcedureRegistry)
                .unwrap(),
        ));

        let mut linear = Session::new(&graph).without_index_selection();
        linear.bind_parameter(db_string(param), value);
        let linear_rows = node_ids(&rows(
            linear
                .execute_source(source, &EmptyProcedureRegistry)
                .unwrap(),
        ));

        assert_eq!(indexed_rows, linear_rows, "row divergence for {source}");
        assert_eq!(indexed_rows.len(), 2);
        let dump = explain_dump(&mut indexed, source);
        assert!(
            dump.contains("TypedIndexRange"),
            "exact numeric equality should render TypedIndexRange; got:\n{dump}"
        );
    }
}

#[test]
fn duration_typed_index_returns_same_rows_as_linear() {
    let graph = SharedGraph::new(GraphId::new(907));
    let event = db_string("Event");
    let elapsed = db_string("elapsed");
    {
        let mut txn = graph.begin_write();
        {
            let mut m = txn.mutator();
            for value in ["PT30M", "PT1H", "PT2H", "PT4H"] {
                m.create_node(
                    LabelSet::single(event.clone()),
                    props([(
                        elapsed.clone(),
                        Value::Duration(Box::new(value.parse().unwrap())),
                    )]),
                )
                .unwrap();
            }
        }
        txn.commit().unwrap();
    }
    graph
        .create_property_index(event, elapsed, TypedIndexKind::Duration)
        .unwrap();
    let source = "MATCH (n:Event) WHERE n.elapsed >= DURATION 'PT1H' AND n.elapsed < DURATION 'PT3H' RETURN n";

    let mut indexed = Session::new(&graph);
    let indexed_rows = node_ids(&rows(
        indexed
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    ));

    let mut linear = Session::new(&graph).without_index_selection();
    let linear_rows = node_ids(&rows(
        linear
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    ));

    assert_eq!(indexed_rows, linear_rows);
    assert_eq!(indexed_rows.len(), 2);
    let dump = explain_dump(&mut indexed, source);
    assert!(
        dump.contains("TypedIndexRange"),
        "duration range should render TypedIndexRange; got:\n{dump}"
    );
}

#[test]
fn explain_renders_label_index_by_default_and_linear_when_disabled() {
    let (graph, _persons) = build_graph();
    let source = "MATCH (n:Person) RETURN n";

    let mut indexed = Session::new(&graph);
    let indexed_dump = explain_dump(&mut indexed, source);
    assert!(
        indexed_dump.contains("LabelIndex"),
        "default-on EXPLAIN should render LabelIndex; got:\n{indexed_dump}"
    );

    let mut linear = Session::new(&graph).without_index_selection();
    let linear_dump = explain_dump(&mut linear, source);
    assert!(
        linear_dump.contains("Linear"),
        "without_index_selection EXPLAIN should render Linear; got:\n{linear_dump}"
    );
    assert!(
        !linear_dump.contains("LabelIndex"),
        "without_index_selection EXPLAIN must not pick LabelIndex; got:\n{linear_dump}"
    );
}

#[test]
fn explain_renders_typed_index_for_indexed_equality() {
    let (graph, _persons) = build_graph();
    let mut session = Session::new(&graph);
    let dump = explain_dump(&mut session, "MATCH (n:Person) WHERE n.age = 30 RETURN n");
    assert!(
        dump.contains("TypedIndexRange"),
        "indexed equality should render TypedIndexRange; got:\n{dump}"
    );
}

#[test]
fn empty_label_returns_no_rows() {
    let (graph, _persons) = build_graph();
    let mut session = Session::new(&graph);
    let table = rows(
        session
            .execute_source("MATCH (n:Ghost) RETURN n", &EmptyProcedureRegistry)
            .unwrap(),
    );
    assert!(table.rows().is_empty(), "absent label yields zero rows");
}

#[test]
fn correctness_corpus_indexed_matches_linear() {
    // A corpus of read shapes; every one must produce identical rows on the
    // default (indexed) and without_index_selection (Linear) paths.
    let (graph, _persons) = build_graph();
    let corpus = [
        "MATCH (n:Person) RETURN n",
        "MATCH (n:Person) WHERE n.age = 30 RETURN n",
        "MATCH (n:Person) WHERE n.age > 30 RETURN n",
        "MATCH (n:Person) WHERE n.age >= 30 AND n.age < 50 RETURN n",
        "MATCH (n:Person) WHERE n.age IN [30, 50] RETURN n",
        "MATCH (n:Person) WHERE n.age = 30 OR n.age = 50 RETURN n",
        "MATCH (n:Robot) RETURN n",
        "MATCH (n) RETURN n",
        "MATCH (n:Person) RETURN n ORDER BY n.age DESC LIMIT 2",
    ];
    for source in corpus {
        let mut indexed = Session::new(&graph);
        let mut linear = Session::new(&graph).without_index_selection();
        let indexed_rows = node_ids(&rows(
            indexed
                .execute_source(source, &EmptyProcedureRegistry)
                .unwrap(),
        ));
        let linear_rows = node_ids(&rows(
            linear
                .execute_source(source, &EmptyProcedureRegistry)
                .unwrap(),
        ));
        assert_eq!(indexed_rows, linear_rows, "row divergence for `{source}`");
    }
}

#[test]
fn create_index_invalidates_cached_plan_and_re_optimizes() {
    use std::num::NonZeroUsize;
    let graph = SharedGraph::new(GraphId::new(902));
    let person = db_string("Person");
    let age = db_string("age");
    {
        // Several distinct-age Person rows so the `age = 7` equality is
        // genuinely selective (1 of N): the OPT-5 cost gate then prefers the
        // typed index over the label-scoped residual baseline. A degenerate
        // single-row population would tie (equality cardinality == label
        // cardinality), and the cost model would correctly decline the index —
        // exactly the `equality_near_constant_declines_typed_index` case — so
        // this test pins the *selective* re-optimization the CREATE INDEX
        // enables, not the degenerate one.
        let mut txn = graph.begin_write();
        {
            let mut m = txn.mutator();
            for value in [7, 11, 13, 17, 19] {
                m.create_node(
                    LabelSet::single(person.clone()),
                    props([(age.clone(), Value::Int(value))]),
                )
                .unwrap();
            }
        }
        txn.commit().unwrap();
    }
    let mut session = Session::new(&graph).with_plan_cache(NonZeroUsize::new(8).unwrap());
    let source = "MATCH (n:Person) WHERE n.age = 7 RETURN n";

    // Before the typed index, equality is served via LabelIndex (label index is
    // intrinsic) with a residual predicate — not a typed index.
    let before = explain_dump(&mut session, source);
    assert!(
        !before.contains("TypedIndexRange"),
        "no typed index yet; got:\n{before}"
    );

    // CREATE INDEX bumps schema_version, invalidating the cached plan.
    graph
        .create_property_index(person, age, TypedIndexKind::I64)
        .unwrap();

    let after = explain_dump(&mut session, source);
    assert!(
        after.contains("TypedIndexRange"),
        "post-CREATE-INDEX plan must use the typed index; got:\n{after}"
    );

    // The result is still correct.
    let table = rows(
        session
            .execute_source(source, &EmptyProcedureRegistry)
            .unwrap(),
    );
    assert_eq!(node_ids(&table).len(), 1);
}

#[test]
fn concurrent_create_index_mid_statement_has_no_optimize_execute_skew() {
    // Pinned-snapshot guardrail: a plan optimized against a pinned snapshot
    // executes correctly against that same snapshot even if another thread
    // commits a CREATE INDEX in between, and a subsequent statement re-pins and
    // sees the new index via the bumped epoch. No panic, correct rows.
    let (graph, _persons) = build_graph();
    let graph = Arc::new(graph);
    let source = "MATCH (n:Person) WHERE n.age = 30 RETURN n";

    // Statement 1: optimize + execute against the current snapshot.
    {
        let mut session = Session::new(&graph);
        let table = rows(
            session
                .execute_source(source, &EmptyProcedureRegistry)
                .unwrap(),
        );
        assert_eq!(node_ids(&table).len(), 1);
    }

    // Concurrent index DDL on another thread.
    let writer = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || {
            graph
                .create_property_index(
                    db_string("Person"),
                    db_string("name"),
                    TypedIndexKind::String,
                )
                .unwrap();
        })
    };
    writer.join().unwrap();

    // Statement 2: re-pins, sees the bumped epoch, still correct.
    {
        let mut session = Session::new(&graph);
        let table = rows(
            session
                .execute_source(source, &EmptyProcedureRegistry)
                .unwrap(),
        );
        assert_eq!(node_ids(&table).len(), 1);
    }
}
