//! Closed-record catalog end-to-end tests (JSON/L1c-d): typed RECORD property DDL lowering
//! and DATA-write validation through the full GQL pipeline — the analyzer → lowering →
//! runtime seam that the analyzer-only and graph-layer tests do not exercise together.
//! Included via `#[path]` from `exec_pipeline_catalog.rs` to keep that test root under the
//! 700-LOC cap; reuses the parent binary's `planned`/`run_write`/`empty_closed_graph`.

use selene_core::Value;
use selene_gql::{ExecutorError, parse};
use selene_graph::{PropertyDefaultRecordField, PropertyDefaultValue};

use super::{db_string, empty_closed_graph, planned, run_write};

fn list_default(items: Vec<PropertyDefaultValue>) -> PropertyDefaultValue {
    PropertyDefaultValue::List(items.into_iter().map(Box::new).collect())
}

fn record_default(fields: Vec<(&str, PropertyDefaultValue)>) -> PropertyDefaultValue {
    PropertyDefaultValue::Record(
        fields
            .into_iter()
            .map(|(name, value)| PropertyDefaultRecordField {
                name: db_string(name),
                value: Box::new(value),
            })
            .collect(),
    )
}

#[test]
fn show_node_types_renders_closed_record_field_structure() {
    // GQLRT-23: a closed RECORD property must render its field structure in SHOW
    // (so open vs closed is distinguishable) and round-trip through the parser,
    // rather than collapsing to a bare "RECORD".
    let graph = empty_closed_graph(3719);
    let ddl = planned("CREATE NODE TYPE :Host (config :: RECORD{host :: STRING, port :: INT})");
    run_write(&graph, &ddl)
        .expect("closed RECORD type executes")
        .1
        .expect("closed RECORD property type commits");

    let (table, outcome) = run_write(&graph, &planned("SHOW NODE TYPES")).expect("show executes");
    outcome.expect("show commits");

    let Value::String(definition) = &table.rows()[0].values()[1] else {
        panic!("definition is a string");
    };
    assert_eq!(
        definition.as_str(),
        "CREATE NODE TYPE :Host (config :: RECORD { host :: STRING, port :: INTEGER })"
    );
    parse(definition.as_str()).expect("closed-RECORD definition round-trips through the parser");
}

#[test]
fn show_node_types_renders_nested_open_record_fields() {
    let graph = empty_closed_graph(3746);
    let ddl = planned(
        "CREATE NODE TYPE :Event (payload :: RECORD{meta :: RECORD, snapshots :: LIST<RECORD>})",
    );
    run_write(&graph, &ddl)
        .expect("nested open RECORD type executes")
        .1
        .expect("nested open RECORD property type commits");

    let (table, outcome) = run_write(&graph, &planned("SHOW NODE TYPES")).expect("show executes");
    outcome.expect("show commits");

    let Value::String(definition) = &table.rows()[0].values()[1] else {
        panic!("definition is a string");
    };
    assert_eq!(
        definition.as_str(),
        "CREATE NODE TYPE :Event (payload :: RECORD { meta :: RECORD, snapshots :: LIST<RECORD> })"
    );
    parse(definition.as_str()).expect("nested open RECORD definition round-trips");
}

#[test]
fn show_node_types_distinguishes_open_and_closed_unit_record_types() {
    let graph = empty_closed_graph(3748);
    let ddl = planned("CREATE NODE TYPE :Event (\"open\" :: ANY RECORD, closed :: RECORD{})");
    run_write(&graph, &ddl)
        .expect("record type DDL executes")
        .1
        .expect("record type DDL commits");

    let (table, outcome) = run_write(&graph, &planned("SHOW NODE TYPES")).expect("show executes");
    outcome.expect("show commits");

    let Value::String(definition) = &table.rows()[0].values()[1] else {
        panic!("definition is a string");
    };
    assert_eq!(
        definition.as_str(),
        "CREATE NODE TYPE :Event (\"open\" :: RECORD, closed :: RECORD {})"
    );
    parse(definition.as_str()).expect("closed unit RECORD definition round-trips");
}

#[test]
fn closed_record_property_type_lowers_end_to_end() {
    // Full grammar -> builder -> analyzer -> lowering -> closed-graph commit for a typed
    // RECORD declaration.
    let graph = empty_closed_graph(3717);
    let plan = planned("CREATE NODE TYPE :Host (config :: RECORD{host :: STRING, port :: INT})");
    let (_table, outcome) = run_write(&graph, &plan).expect("closed RECORD type executes");
    outcome.expect("closed RECORD property type commits");
}

#[test]
fn nested_open_record_fields_validate_data_writes() {
    let graph = empty_closed_graph(3747);

    let ddl = planned(
        "CREATE NODE TYPE :Event (payload :: RECORD{meta :: RECORD, snapshots :: LIST<RECORD>})",
    );
    run_write(&graph, &ddl)
        .expect("nested open RECORD type executes")
        .1
        .expect("nested open RECORD property type commits");

    let conforming = planned(
        "INSERT (n:Event {payload: RECORD{\
         meta: RECORD{kind: 'agent', score: 1}, \
         snapshots: [RECORD{id: 'a'}, RECORD{id: 'b', ok: true}]}})",
    );
    run_write(&graph, &conforming)
        .expect("conforming nested open record write executes")
        .1
        .expect("conforming nested open record commits");

    let violating = planned(
        "INSERT (n:Event {payload: RECORD{meta: 'not-a-record', snapshots: [RECORD{id: 'a'}]}})",
    );
    let (_table, outcome) = run_write(&graph, &violating).expect("violating write executes");
    let error = outcome.expect_err("non-record nested field is rejected at commit");
    assert_eq!(error.gqlstatus(), "G2000");
}

#[test]
fn record_value_data_write_validates_against_closed_record_property() {
    // Declare the typed RECORD property, then DATA-write record values into it. The
    // conforming value must analyze AND commit (regression guard for the analyzer/runtime
    // divergence that made typed-RECORD writes unexecutable); the non-conforming value is
    // rejected at commit with G2000 (C7 enforcement through the real query interface).
    let graph = empty_closed_graph(3718);

    let ddl = planned("CREATE NODE TYPE :Host (config :: RECORD{host :: STRING, port :: INT})");
    run_write(&graph, &ddl)
        .expect("record type DDL executes")
        .1
        .expect("record type DDL commits");

    let conforming = planned("INSERT (n:Host {config: RECORD{host: 'h', port: 1}})");
    run_write(&graph, &conforming)
        .expect("conforming record write executes")
        .1
        .expect("conforming record value commits");

    let violating = planned("INSERT (n:Host {config: RECORD{host: 'h', port: 'not-an-int'}})");
    let (_table, outcome) = run_write(&graph, &violating).expect("violating record write executes");
    let error = outcome.expect_err("non-conforming record value is rejected at commit");
    assert_eq!(error.gqlstatus(), "G2000");
}

#[test]
fn closed_record_default_accepts_typed_record_literal() {
    let graph = empty_closed_graph(3720);
    let plan = planned(
        r#"CREATE NODE TYPE :Host (
            config :: RECORD{
                host :: STRING,
                port :: UINT64,
                payload :: JSON,
                embedding :: VECTOR,
                tags :: LIST<STRING>,
                nested :: RECORD{flag :: BOOLEAN},
                vectors :: LIST<VECTOR>
            } DEFAULT RECORD{
                host: 'h',
                port: 42,
                payload: '{"b":2,"a":1}',
                embedding: [1, 0],
                tags: ['agent', 'memory'],
                nested: RECORD{flag: true},
                vectors: [[1, 0], [0, 1]]
            }
        )"#,
    );

    run_write(&graph, &plan)
        .expect("RECORD default executes")
        .1
        .expect("RECORD default commits");

    let graph_type = graph.graph_type().expect("closed graph type");
    assert_eq!(
        graph_type.node_types[0].properties[0].default,
        Some(record_default(vec![
            ("host", PropertyDefaultValue::String(db_string("h"))),
            ("port", PropertyDefaultValue::Uint(42)),
            (
                "payload",
                PropertyDefaultValue::Json(db_string(r#"{"a":1,"b":2}"#))
            ),
            (
                "embedding",
                PropertyDefaultValue::Vector(vec![1.0_f32.to_bits(), 0.0_f32.to_bits()])
            ),
            (
                "tags",
                list_default(vec![
                    PropertyDefaultValue::String(db_string("agent")),
                    PropertyDefaultValue::String(db_string("memory")),
                ])
            ),
            (
                "nested",
                record_default(vec![("flag", PropertyDefaultValue::Boolean(true))])
            ),
            (
                "vectors",
                list_default(vec![
                    PropertyDefaultValue::Vector(vec![1.0_f32.to_bits(), 0.0_f32.to_bits()]),
                    PropertyDefaultValue::Vector(vec![0.0_f32.to_bits(), 1.0_f32.to_bits()]),
                ])
            ),
        ]))
    );
}

#[test]
fn open_record_default_accepts_recursive_untyped_record_literal() {
    let graph = empty_closed_graph(3721);
    let plan = planned(
        "CREATE NODE TYPE :Doc (payload :: RECORD DEFAULT \
         RECORD{kind: 'open', counts: [1, 2], nested: RECORD{ok: true}})",
    );

    run_write(&graph, &plan)
        .expect("open RECORD default executes")
        .1
        .expect("open RECORD default commits");

    let graph_type = graph.graph_type().expect("closed graph type");
    assert_eq!(
        graph_type.node_types[0].properties[0].default,
        Some(record_default(vec![
            ("kind", PropertyDefaultValue::String(db_string("open"))),
            (
                "counts",
                list_default(vec![
                    PropertyDefaultValue::Integer(1),
                    PropertyDefaultValue::Integer(2),
                ])
            ),
            (
                "nested",
                record_default(vec![("ok", PropertyDefaultValue::Boolean(true))])
            ),
        ]))
    );
}

#[test]
fn nested_open_record_default_accepts_record_literals() {
    let graph = empty_closed_graph(3748);
    let plan = planned(
        "CREATE NODE TYPE :Event (payload :: RECORD{meta :: RECORD, snapshots :: LIST<RECORD>} \
         DEFAULT RECORD{\
           meta: RECORD{kind: 'agent', score: 1}, \
           snapshots: [RECORD{id: 'a'}, RECORD{id: 'b', ok: true}]\
         })",
    );

    run_write(&graph, &plan)
        .expect("nested open RECORD default executes")
        .1
        .expect("nested open RECORD default commits");

    let graph_type = graph.graph_type().expect("closed graph type");
    assert_eq!(
        graph_type.node_types[0].properties[0].default,
        Some(record_default(vec![
            (
                "meta",
                record_default(vec![
                    ("kind", PropertyDefaultValue::String(db_string("agent"))),
                    ("score", PropertyDefaultValue::Integer(1)),
                ])
            ),
            (
                "snapshots",
                list_default(vec![
                    record_default(vec![("id", PropertyDefaultValue::String(db_string("a")))]),
                    record_default(vec![
                        ("id", PropertyDefaultValue::String(db_string("b"))),
                        ("ok", PropertyDefaultValue::Boolean(true)),
                    ]),
                ])
            ),
        ]))
    );
}

#[test]
fn record_default_rejects_missing_declared_field() {
    let graph = empty_closed_graph(3722);
    let plan = planned(
        "CREATE NODE TYPE :Host (config :: RECORD{host :: STRING, port :: INTEGER} \
         DEFAULT RECORD{host: 'h'})",
    );

    let err = run_write(&graph, &plan).expect_err("missing RECORD default field rejected");

    assert_eq!(err.gqlstatus().as_str(), "22G0U");
    assert!(matches!(
        err,
        ExecutorError::DataException { message, .. }
            if message.contains("missing declared field port")
    ));
}

#[test]
fn record_default_rejects_extra_field() {
    let graph = empty_closed_graph(3723);
    let plan = planned(
        "CREATE NODE TYPE :Host (config :: RECORD{host :: STRING, port :: INTEGER} \
         DEFAULT RECORD{host: 'h', port: 1, extra: true})",
    );

    let err = run_write(&graph, &plan).expect_err("extra RECORD default field rejected");

    assert_eq!(err.gqlstatus().as_str(), "22G0U");
    assert!(matches!(
        err,
        ExecutorError::DataException { message, .. }
            if message.contains("field extra is not declared")
    ));
}

#[test]
fn record_default_rejects_unassignable_field() {
    let graph = empty_closed_graph(3724);
    let plan = planned(
        "CREATE NODE TYPE :Host (config :: RECORD{host :: STRING, port :: INTEGER} \
         DEFAULT RECORD{host: 'h', port: 'x'})",
    );

    let err = run_write(&graph, &plan).expect_err("unassignable RECORD default field rejected");

    assert_eq!(err.gqlstatus().as_str(), "22G0X");
    assert!(matches!(
        err,
        ExecutorError::DataException { message, .. }
            if message.contains("not assignable to declared field type")
    ));
}

#[test]
fn record_default_rejects_null_not_null_field() {
    let graph = empty_closed_graph(3726);
    let plan = planned(
        "CREATE NODE TYPE :Host (config :: RECORD{host :: STRING NOT NULL, port :: INTEGER} \
         DEFAULT RECORD{host: NULL, port: 1})",
    );

    let err = run_write(&graph, &plan).expect_err("NULL RECORD default field rejected");

    assert_eq!(err.gqlstatus().as_str(), "22G0X");
    assert!(matches!(
        err,
        ExecutorError::DataException { message, .. }
            if message.contains("field host cannot be NULL")
    ));
}

#[test]
fn record_default_rejects_duplicate_field() {
    let graph = empty_closed_graph(3725);
    let error = selene_gql::Session::new(&graph).execute_source(
        "CREATE NODE TYPE :Host (config :: RECORD{host :: STRING} DEFAULT RECORD{host: 'h', host: 'again'})",
        &selene_gql::EmptyProcedureRegistry,
    ).unwrap_err();
    assert_eq!(error.gqlstatus().as_str(), "22G0X");
    assert!(matches!(
        error,
        ExecutorError::Analysis {
            source: selene_gql::AnalysisError::StructuralType {
                source: selene_core::StructuralTypeError::DuplicateField(_),
                ..
            }
        }
    ));
}
