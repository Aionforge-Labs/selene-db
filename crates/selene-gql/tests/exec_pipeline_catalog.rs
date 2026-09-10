//! BRIEF-37 catalog pipeline executor tests.

mod exec_common;

// Catalog subdomains live in sibling files to keep this test root under the 700-LOC cap;
// they reuse this binary's `planned`/`run_write`/`empty_closed_graph`.
#[path = "exec_pipeline_catalog/inline_indexes.rs"]
mod inline_indexes;
#[path = "exec_pipeline_catalog/property_constraints.rs"]
mod property_constraints;
#[path = "exec_pipeline_catalog/record_catalog.rs"]
mod record_catalog;
#[path = "exec_pipeline_catalog/temporal_indexes.rs"]
mod temporal_indexes;
#[path = "exec_pipeline_catalog/typed_markers.rs"]
mod typed_markers;

use selene_core::{Change, GraphId, LabelSet, SchemaChange, Value};
use selene_gql::{
    Binding, BindingTable, BindingTableSchema, EmptyProcedureRegistry, ExecutionPlan,
    ExecutorError, PipelineOp, TxContext, analyze, execute_pipeline, parse, plan,
};
use selene_graph::{EdgeEndpointDef, EdgeTypeDef};
use selene_graph::{GraphError, GraphTypeDef, NodeTypeDef, SharedGraph, ValidationMode};

use exec_common::db_string;

fn planned(source: &str) -> ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

fn seed_table() -> BindingTable {
    BindingTable::new(
        BindingTableSchema {
            columns: Vec::new(),
        },
        vec![Binding::empty()],
    )
}

fn empty_closed_graph(id: u64) -> SharedGraph {
    SharedGraph::builder(GraphId::new(id))
        .bound_to(GraphTypeDef {
            name: db_string("catalog.test.graph"),
            node_types: Vec::new(),
            edge_types: Vec::new(),
        })
        .unwrap()
        .build()
        .unwrap()
}

fn person_graph(id: u64) -> SharedGraph {
    let person = db_string("Person");
    SharedGraph::builder(GraphId::new(id))
        .bound_to(GraphTypeDef {
            name: db_string("catalog.person.graph"),
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

fn closed_graph_with_type(id: u64, graph_type: GraphTypeDef) -> SharedGraph {
    SharedGraph::builder(GraphId::new(id))
        .bound_to(graph_type)
        .unwrap()
        .build()
        .unwrap()
}

fn run_write(
    graph: &SharedGraph,
    plan: &ExecutionPlan,
) -> Result<
    (
        BindingTable,
        Result<selene_graph::CommitOutcome, GraphError>,
    ),
    ExecutorError,
> {
    let snapshot = graph.read();
    let mut txn = graph.begin_write();
    let result = {
        let mut ctx = TxContext::write(
            snapshot,
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            &mut txn,
            graph.index_providers(),
        );
        execute_pipeline(&plan.pipeline, seed_table(), &mut ctx)
    };
    match result {
        Ok(table) => Ok((table, txn.commit())),
        Err(error) => {
            txn.rollback();
            Err(error)
        }
    }
}

fn catalog_op(source: &str) -> PipelineOp {
    planned(source).pipeline.remove(0)
}

fn create_node_type_op(label: &str) -> PipelineOp {
    catalog_op(&format!("CREATE NODE TYPE :{label} ()"))
}

#[test]
fn create_node_type_creates_type_and_preserves_input_row() {
    let graph = empty_closed_graph(3700);
    let plan = planned("CREATE NODE TYPE :Person (name :: STRING NOT NULL)");

    let (table, outcome) = run_write(&graph, &plan).expect("catalog executes");
    let outcome = outcome.expect("commit succeeds");

    assert_eq!(table.row_count(), 1);
    let graph_type = graph.graph_type().unwrap();
    assert_eq!(graph_type.node_types[0].name.as_str(), "Person");
    assert_eq!(graph_type.node_types[0].properties[0].name.as_str(), "name");
    assert!(graph_type.node_types[0].properties[0].required);
    assert!(matches!(
        outcome.changes.as_slice(),
        [Change::SchemaChanged {
            change: SchemaChange::NodeTypeAddedV2 { label, .. },
            ..
        }] if label.as_str() == "Person"
    ));
}

#[test]
fn show_node_types_on_open_graph_returns_empty_schemaful_table() {
    let graph = SharedGraph::new(GraphId::new(3701));
    let plan = planned("SHOW NODE TYPES");
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );

    let table = execute_pipeline(&plan.pipeline, seed_table(), &mut ctx).expect("show executes");

    assert_eq!(table.row_count(), 0);
    assert_eq!(
        table.schema().columns[0].name.as_ref().unwrap().as_str(),
        "label"
    );
    assert_eq!(
        table.schema().columns[1].name.as_ref().unwrap().as_str(),
        "definition"
    );
}

#[test]
fn show_node_types_after_create_in_same_statement_includes_new_type() {
    let graph = empty_closed_graph(3702);
    let mut plan = planned("SHOW NODE TYPES");
    plan.pipeline.insert(
        0,
        catalog_op("CREATE NODE TYPE :Person (name :: STRING NOT NULL)"),
    );

    let (table, outcome) = run_write(&graph, &plan).expect("catalog executes");
    outcome.expect("commit succeeds");

    assert_eq!(table.row_count(), 1);
    assert_eq!(
        table.rows()[0].values()[0],
        Value::String(db_string("Person"))
    );
    assert_eq!(
        table.rows()[0].values()[1],
        Value::String(db_string(
            "CREATE NODE TYPE :Person (name :: STRING NOT NULL)"
        ))
    );
}

#[test]
fn show_node_types_renders_bytes_default_as_byte_literal() {
    let graph = empty_closed_graph(3718);
    run_write(
        &graph,
        &planned("CREATE NODE TYPE :Blob (payload :: BYTES DEFAULT X'00ff')"),
    )
    .expect("catalog executes")
    .1
    .expect("commit succeeds");

    let plan = planned("SHOW NODE TYPES");
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );

    let table = execute_pipeline(&plan.pipeline, seed_table(), &mut ctx).expect("show executes");

    assert_eq!(
        table.rows()[0].values()[1],
        Value::String(db_string(
            "CREATE NODE TYPE :Blob (payload :: BYTES DEFAULT X'00FF')"
        ))
    );
}

#[test]
fn show_node_types_renders_key_labels_not_internal_name() {
    let graph = closed_graph_with_type(
        3717,
        GraphTypeDef {
            name: db_string("catalog.asymmetric.graph"),
            node_types: vec![NodeTypeDef {
                name: db_string("types.person"),
                key_labels: LabelSet::single(db_string("Person")),
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: Vec::new(),
        },
    );
    let plan = planned("SHOW NODE TYPES");
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );

    let table = execute_pipeline(&plan.pipeline, seed_table(), &mut ctx).expect("show executes");

    assert_eq!(
        table.rows()[0].values()[0],
        Value::String(db_string("Person"))
    );
    assert_eq!(
        table.rows()[0].values()[1],
        Value::String(db_string("CREATE NODE TYPE :Person ()"))
    );
}

#[test]
fn create_edge_type_resolves_endpoints_created_earlier_in_same_statement() {
    let graph = empty_closed_graph(3703);
    let mut plan = planned("CREATE EDGE TYPE :WORKS_AT (FROM :Person TO :Company)");
    plan.pipeline.insert(0, create_node_type_op("Company"));
    plan.pipeline.insert(0, create_node_type_op("Person"));

    let (_, outcome) = run_write(&graph, &plan).expect("catalog executes");
    outcome.expect("commit succeeds");

    let graph_type = graph.graph_type().unwrap();
    let edge_type = &graph_type.edge_types[0];
    assert_eq!(edge_type.name.as_str(), "WORKS_AT");
    assert_eq!(edge_type.source_node_type, EdgeEndpointDef::NodeType(0));
    assert_eq!(edge_type.target_node_type, EdgeEndpointDef::NodeType(1));
}

#[test]
fn show_edge_types_renders_round_trippable_definition() {
    let graph = empty_closed_graph(3704);
    let mut plan = planned("SHOW EDGE TYPES");
    plan.pipeline.insert(
        0,
        catalog_op("CREATE EDGE TYPE :KNOWS (FROM :Person TO :Person, since :: DATE)"),
    );
    plan.pipeline.insert(0, create_node_type_op("Person"));

    let (table, outcome) = run_write(&graph, &plan).expect("catalog executes");
    outcome.expect("commit succeeds");

    let Value::String(definition) = &table.rows()[0].values()[1] else {
        panic!("definition is string");
    };
    assert_eq!(
        definition.as_str(),
        "CREATE EDGE TYPE :KNOWS (FROM :Person TO :Person, since :: DATE)"
    );
    parse(definition.as_str()).expect("definition round-trips through parser");
}

#[test]
fn show_edge_types_renders_label_not_internal_name() {
    let person = db_string("Person");
    let knows = db_string("KNOWS");
    let graph = closed_graph_with_type(
        3718,
        GraphTypeDef {
            name: db_string("catalog.edge.asymmetric.graph"),
            node_types: vec![NodeTypeDef {
                name: db_string("types.person"),
                key_labels: LabelSet::single(person),
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: vec![EdgeTypeDef {
                name: db_string("types.knows"),
                label: knows.clone(),
                source_node_type: EdgeEndpointDef::NodeType(0),
                target_node_type: EdgeEndpointDef::NodeType(0),
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
        },
    );
    let plan = planned("SHOW EDGE TYPES");
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );

    let table = execute_pipeline(&plan.pipeline, seed_table(), &mut ctx).expect("show executes");

    assert_eq!(table.rows()[0].values()[0], Value::String(knows.clone()));
    assert_eq!(
        table.rows()[0].values()[1],
        Value::String(db_string(
            "CREATE EDGE TYPE :KNOWS (FROM :Person TO :Person)"
        ))
    );
}

#[test]
fn show_edge_types_renders_any_endpoints_as_endpoint_less() {
    let graph = person_graph(3721);
    let mut plan = planned("SHOW EDGE TYPES");
    plan.pipeline
        .insert(0, catalog_op("CREATE EDGE TYPE :KNOWS ()"));

    let (table, outcome) = run_write(&graph, &plan).expect("catalog executes");
    outcome.expect("commit succeeds");
    let graph_type = graph.graph_type().unwrap();
    assert_eq!(
        graph_type.edge_types[0].source_node_type,
        EdgeEndpointDef::Any
    );
    assert_eq!(
        graph_type.edge_types[0].target_node_type,
        EdgeEndpointDef::Any
    );

    let Value::String(definition) = &table.rows()[0].values()[1] else {
        panic!("definition is string");
    };
    assert_eq!(definition.as_str(), "CREATE EDGE TYPE :KNOWS ()");
    parse(definition.as_str()).expect("endpoint-less definition round-trips");
}

#[test]
fn show_edge_types_renders_multi_label_endpoint_labels() {
    let knows = db_string("KNOWS");
    let labels = LabelSet::from_iter([db_string("Person"), db_string("Active")]);
    let graph = closed_graph_with_type(
        3719,
        GraphTypeDef {
            name: db_string("catalog.edge.multilabel.graph"),
            node_types: vec![NodeTypeDef {
                name: db_string("types.active_person"),
                key_labels: labels,
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: vec![EdgeTypeDef {
                name: db_string("types.knows"),
                label: knows,
                source_node_type: EdgeEndpointDef::NodeType(0),
                target_node_type: EdgeEndpointDef::NodeType(0),
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
        },
    );
    let plan = planned("SHOW EDGE TYPES");
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );

    let table = execute_pipeline(&plan.pipeline, seed_table(), &mut ctx).expect("show executes");

    assert_eq!(
        table.rows()[0].values()[1],
        Value::String(db_string(
            "CREATE EDGE TYPE :KNOWS (FROM :Active,:Person TO :Active,:Person)"
        ))
    );
    let Value::String(definition) = &table.rows()[0].values()[1] else {
        panic!("definition is string");
    };
    parse(definition.as_str()).expect("multi-label endpoint definition round-trips");
}

#[test]
fn create_node_type_then_insert_same_statement_validates_against_new_type() {
    let graph = empty_closed_graph(3705);
    let mut plan = planned("INSERT (n:Person {name: 'Ada'}) RETURN n");
    plan.pipeline.insert(
        0,
        catalog_op("CREATE NODE TYPE :Person (name :: STRING NOT NULL)"),
    );

    let (table, outcome) = run_write(&graph, &plan).expect("catalog plus mutation executes");
    outcome.expect("commit succeeds");

    assert_eq!(table.row_count(), 1);
    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn open_graph_create_node_type_returns_data_exception() {
    let graph = SharedGraph::new(GraphId::new(3706));
    let plan = planned("CREATE NODE TYPE :Person ()");

    let err = run_write(&graph, &plan).expect_err("open graph catalog DDL fails");

    assert!(matches!(
        err,
        ExecutorError::GraphTypeViolation { message, .. }
            if message.contains("open graph (GG01) does not support catalog type DDL")
    ));
}

#[test]
fn catalog_op_without_write_txn_returns_invalid_transaction_state() {
    let graph = empty_closed_graph(3707);
    let plan = planned("CREATE NODE TYPE :Person ()");
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );

    let err = execute_pipeline(&plan.pipeline, seed_table(), &mut ctx)
        .expect_err("catalog DDL needs write tx");

    assert!(matches!(
        err,
        ExecutorError::InvalidTransactionState {
            detail: "catalog op invoked without write transaction",
            ..
        }
    ));
}

#[test]
fn create_edge_type_without_write_txn_returns_invalid_transaction_state() {
    let graph = empty_closed_graph(3720);
    let plan = planned("CREATE EDGE TYPE :KNOWS (FROM :Missing TO :Missing)");
    let mut ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    );

    let err = execute_pipeline(&plan.pipeline, seed_table(), &mut ctx)
        .expect_err("catalog DDL needs write tx before endpoint lookup");

    assert!(matches!(
        err,
        ExecutorError::InvalidTransactionState {
            detail: "catalog op invoked without write transaction",
            ..
        }
    ));
}

#[test]
fn create_edge_type_unknown_endpoint_returns_data_exception() {
    let graph = empty_closed_graph(3708);
    let plan = planned("CREATE EDGE TYPE :KNOWS (FROM :Person TO :Person)");

    let err = run_write(&graph, &plan).expect_err("unknown endpoint fails");

    assert!(matches!(
        err,
        ExecutorError::GraphTypeViolation { message, .. }
            if message.contains("unknown node type label set :Person")
    ));
}

#[test]
fn create_edge_type_multi_label_endpoint_resolves_exact_label_set() {
    let labels = LabelSet::from_iter([db_string("Person"), db_string("Employee")]);
    let graph = closed_graph_with_type(
        3709,
        GraphTypeDef {
            name: db_string("catalog.multi.endpoint.graph"),
            node_types: vec![NodeTypeDef {
                name: db_string("types.employee_person"),
                key_labels: labels,
                properties: Vec::new(),
                validation_mode: ValidationMode::Strict,
            }],
            edge_types: Vec::new(),
        },
    );
    let plan = planned("CREATE EDGE TYPE :KNOWS (FROM :Person,:Employee TO :Person,:Employee)");

    let (_, outcome) = run_write(&graph, &plan).expect("multi-label endpoint resolves");
    outcome.expect("commit succeeds");

    let graph_type = graph.graph_type().unwrap();
    let edge_type = &graph_type.edge_types[0];
    assert_eq!(edge_type.source_node_type, EdgeEndpointDef::NodeType(0));
    assert_eq!(edge_type.target_node_type, EdgeEndpointDef::NodeType(0));
}

#[test]
fn drop_nonexistent_node_type_returns_data_exception() {
    let graph = empty_closed_graph(3712);
    let plan = planned("DROP NODE TYPE :Missing");

    let err = run_write(&graph, &plan).expect_err("drop missing fails");

    assert!(matches!(
        err,
        ExecutorError::GraphTypeViolation { message, .. }
            if message.contains("node type Missing does not exist")
    ));
}

#[path = "exec_pipeline_catalog/structural_unique.rs"]
mod structural_unique;
